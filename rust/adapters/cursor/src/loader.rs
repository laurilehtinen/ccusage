use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use jiff::tz::TimeZone as JiffTimeZone;
use serde_json::{Map, Value, json};

use crate::{
    LoadedEntry, PricingMap, Result, cli::SharedArgs, debug_log, parse_tz, read_files_parallel,
};

use super::{
    parser::{
        json_objects_from_bytes, record_to_loaded_entry, records_from_json_value,
        records_from_sdk_index,
    },
    paths::{CursorDbKind, discover_source_files, identity_from_path},
};

pub fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(
        crate::progress::UsageLoadAgent("Cursor CLI"),
        shared.json,
        || load_entries_inner(shared, pricing),
    )
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let (dbs, ndjson) = discover_source_files()?;
    let db_paths: Vec<_> = dbs.iter().map(|file| file.path.clone()).collect();
    let kinds: Vec<_> = dbs.iter().map(|file| file.kind).collect();
    let loaded_dbs = read_files_parallel(&db_paths, shared.single_thread, |path| {
        let kind = db_paths
            .iter()
            .position(|candidate| candidate == path)
            .map(|index| kinds[index])
            .unwrap_or(CursorDbKind::Store);
        load_database(path, kind, tz.as_ref(), shared, pricing)
    });
    let ndjson_paths: Vec<_> = ndjson.iter().map(|file| file.path.clone()).collect();
    let loaded_ndjson = load_ndjson_groups(&ndjson_paths, tz.as_ref(), shared, pricing);

    let mut entries: Vec<_> = loaded_dbs.into_iter().flatten().collect();
    entries.extend(loaded_ndjson);
    let mut seen = HashSet::new();
    entries.retain(|entry| {
        let Some(message_id) = entry.data.message.id.as_deref() else {
            return true;
        };
        seen.insert(format!(
            "{}|{message_id}|{}",
            entry.session_id,
            entry.model.as_deref().unwrap_or_default()
        ))
    });
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

pub fn has_data() -> bool {
    discover_source_files().is_ok_and(|(dbs, ndjson)| !dbs.is_empty() || !ndjson.is_empty())
}

fn load_database(
    path: &Path,
    kind: CursorDbKind,
    tz: Option<&JiffTimeZone>,
    shared: &SharedArgs,
    pricing: &PricingMap,
) -> Vec<LoadedEntry> {
    let Ok(connection) =
        sqlite::Connection::open_with_flags(path, sqlite::OpenFlags::new().with_read_only())
    else {
        debug_log(
            shared,
            format!("Failed to open Cursor database: {}", path.display()),
        );
        return Vec::new();
    };
    let (fallback_session, project_path) = identity_from_path(path);
    let tables = table_names(&connection);
    let mut entries = Vec::new();
    match kind {
        CursorDbKind::Index => {
            let runs = if tables.iter().any(|table| table == "runs") {
                load_table_rows(&connection, "runs", shared)
            } else {
                Vec::new()
            };
            let events = if tables.iter().any(|table| table == "run_events") {
                load_table_rows(&connection, "run_events", shared)
            } else {
                Vec::new()
            };
            for record in records_from_sdk_index(&runs, &events, &fallback_session, &project_path) {
                entries.push(record_to_loaded_entry(record, tz, shared.mode, pricing));
            }
        }
        _ => {
            for table in tables {
                load_table(
                    &connection,
                    &table,
                    &fallback_session,
                    &project_path,
                    tz,
                    shared,
                    pricing,
                    &mut entries,
                );
            }
        }
    }
    entries
}

fn load_table_rows(
    connection: &sqlite::Connection,
    table: &str,
    shared: &SharedArgs,
) -> Vec<Value> {
    let mut rows = Vec::new();
    let Ok(mut statement) = connection.prepare(format!("SELECT * FROM {table}")) else {
        debug_log(shared, format!("Failed to read Cursor table {table}"));
        return rows;
    };
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                if let Some(row) = row_json(&statement) {
                    rows.push(row);
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => {
                debug_log(shared, format!("Failed to query Cursor table {table}"));
                break;
            }
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn load_table(
    connection: &sqlite::Connection,
    table: &str,
    fallback_session: &str,
    project_path: &str,
    tz: Option<&JiffTimeZone>,
    shared: &SharedArgs,
    pricing: &PricingMap,
    entries: &mut Vec<LoadedEntry>,
) {
    let Ok(mut statement) = connection.prepare(format!("SELECT * FROM {table}")) else {
        debug_log(shared, format!("Failed to read Cursor table {table}"));
        return;
    };
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                if let Some(row) = row_json(&statement) {
                    push_records_from_value(
                        &row,
                        fallback_session,
                        project_path,
                        tz,
                        shared.mode,
                        pricing,
                        entries,
                    );
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => {
                debug_log(shared, format!("Failed to query Cursor table {table}"));
                break;
            }
        }
    }
}

fn load_ndjson_groups(
    paths: &[PathBuf],
    tz: Option<&JiffTimeZone>,
    shared: &SharedArgs,
    pricing: &PricingMap,
) -> Vec<LoadedEntry> {
    let mut groups: BTreeMap<PathBuf, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
    for path in paths {
        let parent = path.parent().unwrap_or(path).to_path_buf();
        let entry = groups.entry(parent).or_default();
        match path.file_name().and_then(|name| name.to_str()) {
            Some("run_events.ndjson") => entry.1 = Some(path.clone()),
            _ => entry.0 = Some(path.clone()),
        }
    }

    let mut entries = Vec::new();
    for (dir, (runs_path, events_path)) in groups {
        let identity_path = runs_path
            .as_deref()
            .or(events_path.as_deref())
            .unwrap_or(&dir);
        let (fallback_session, project_path) = identity_from_path(identity_path);
        let run_values = runs_path
            .as_ref()
            .map(|path| read_ndjson_values(path, shared))
            .unwrap_or_default();
        let event_values = events_path
            .as_ref()
            .map(|path| read_ndjson_values(path, shared))
            .unwrap_or_default();
        if events_path.is_some() {
            for record in
                records_from_sdk_index(&run_values, &event_values, &fallback_session, &project_path)
            {
                entries.push(record_to_loaded_entry(record, tz, shared.mode, pricing));
            }
        } else {
            for value in run_values {
                push_records_from_value(
                    &value,
                    &fallback_session,
                    &project_path,
                    tz,
                    shared.mode,
                    pricing,
                    &mut entries,
                );
            }
        }
    }
    entries
}

fn read_ndjson_values(path: &Path, shared: &SharedArgs) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        debug_log(
            shared,
            format!("Failed to read Cursor ndjson: {}", path.display()),
        );
        return Vec::new();
    };
    let mut values = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            values.push(value);
        }
    }
    values
}

fn row_json(statement: &sqlite::Statement<'_>) -> Option<Value> {
    let names = statement.column_names().to_vec();
    let mut row = Map::new();
    for (index, name) in names.iter().enumerate() {
        if let Some(value) = cell_json(statement, index) {
            row.insert(name.clone(), value);
        }
    }
    if row.is_empty() {
        None
    } else {
        Some(Value::Object(row))
    }
}

fn push_records_from_value(
    value: &Value,
    fallback_session: &str,
    project_path: &str,
    tz: Option<&JiffTimeZone>,
    mode: crate::cli::CostMode,
    pricing: &PricingMap,
    entries: &mut Vec<LoadedEntry>,
) {
    for record in records_from_json_value(value, fallback_session, project_path) {
        entries.push(record_to_loaded_entry(record, tz, mode, pricing));
    }
}

fn table_names(connection: &sqlite::Connection) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(mut statement) = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    ) else {
        return names;
    };
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                if let Ok(name) = statement.read::<String, _>(0)
                    && is_safe_ident(&name)
                {
                    names.push(name);
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => break,
        }
    }
    names
}

fn is_safe_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn cell_json(statement: &sqlite::Statement<'_>, index: usize) -> Option<Value> {
    let kind = statement.column_type(index).ok()?;
    match kind {
        sqlite::Type::Null => None,
        sqlite::Type::Integer => statement
            .read::<i64, _>(index)
            .ok()
            .map(|value| json!(value)),
        sqlite::Type::Float => statement
            .read::<f64, _>(index)
            .ok()
            .map(|value| json!(value)),
        sqlite::Type::String => {
            let text = statement.read::<String, _>(index).ok()?;
            serde_json::from_str(&text)
                .ok()
                .or(Some(Value::String(text)))
        }
        sqlite::Type::Binary => {
            let bytes = statement.read::<Vec<u8>, _>(index).ok()?;
            if let Ok(text) = std::str::from_utf8(&bytes)
                && let Ok(value) = serde_json::from_str::<Value>(text)
            {
                return Some(value);
            }
            let objects = json_objects_from_bytes(&bytes);
            match objects.len() {
                0 => None,
                1 => objects.into_iter().next(),
                _ => Some(Value::Array(objects)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CostMode;
    use ccusage_test_support::{EnvVarsGuard, fs_fixture};
    use std::{ffi::OsString, path::Path};

    fn with_cursor_home(root: &Path) -> EnvVarsGuard {
        EnvVarsGuard::set_many([(
            super::super::paths::CURSOR_AGENT_HOME_ENV,
            Some(OsString::from(root.as_os_str())),
        )])
    }

    fn ensure_parent(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
    }

    fn create_runs_db(path: &Path, usage_json: &str) {
        ensure_parent(path);
        let db = sqlite::open(path).unwrap();
        db.execute(
            "CREATE TABLE runs (runId TEXT, agentId TEXT, model TEXT, usage TEXT, startedAt INTEGER)",
        )
        .unwrap();
        let mut statement = db
            .prepare(
                "INSERT INTO runs (runId, agentId, model, usage, startedAt) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .unwrap();
        statement.bind((1, "run-1")).unwrap();
        statement.bind((2, "agent-abc")).unwrap();
        statement.bind((3, "grok-4.6")).unwrap();
        statement.bind((4, usage_json)).unwrap();
        statement.bind((5, 1_750_000_000_000i64)).unwrap();
        statement.next().unwrap();
    }

    fn create_store_db(path: &Path, blob: &[u8], extra_zero: bool) {
        ensure_parent(path);
        let db = sqlite::open(path).unwrap();
        db.execute("CREATE TABLE blobs (id TEXT, data BLOB)")
            .unwrap();
        let mut statement = db
            .prepare("INSERT INTO blobs (id, data) VALUES (?1, ?2)")
            .unwrap();
        statement.bind((1, "blob-1")).unwrap();
        statement
            .bind((2, sqlite::Value::Binary(blob.to_vec())))
            .unwrap();
        statement.next().unwrap();
        if extra_zero {
            let mut zero = db
                .prepare("INSERT INTO blobs (id, data) VALUES (?1, ?2)")
                .unwrap();
            zero.bind((1, "blob-zero")).unwrap();
            zero.bind((
                2,
                sqlite::Value::Binary(
                    br#"{"timestamp":1750000001000,"model":"grok-4.6","usage":{"inputTokens":0,"outputTokens":0}}"#
                        .to_vec(),
                ),
            ))
            .unwrap();
            zero.next().unwrap();
        }
    }

    #[test]
    fn loads_sdk_index_runs_with_exclusive_totals() {
        let fixture = fs_fixture!({});
        create_runs_db(
            &fixture.path("projects/my-app/sdk-agent-store/hash1/index.db"),
            r#"{"inputTokens":100,"outputTokens":20,"cacheReadTokens":40,"cacheWriteTokens":10,"totalTokens":170}"#,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "agent-abc");
        assert_eq!(entries[0].model.as_deref(), Some("grok-4.6"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 40);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            10
        );
        assert_eq!(entries[0].data.message.usage.output_tokens, 20);
        assert_eq!(entries[0].project_path.as_ref(), "my-app");
        assert_eq!(entries[0].date, "2025-06-15");
        assert!(entries[0].cost > 0.0);
    }

    #[test]
    fn loads_store_blob_usage_and_skips_zero_token_rows() {
        let fixture = fs_fixture!({});
        let mut blob = vec![0x0a, 0x05];
        blob.extend_from_slice(
            br#"{"timestamp":1750000000000,"model":"cursor-grok-4.6-high-fast","model_id":"grok-4.6","conversation_id":"conv-store","generation_id":"gen-1","input_tokens":140,"output_tokens":20,"cache_read_tokens":40,"cache_write_tokens":10}"#,
        );
        create_store_db(
            &fixture.path("chats/my-app/sess-store/store.db"),
            &blob,
            true,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            mode: CostMode::Calculate,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "conv-store");
        assert_eq!(entries[0].data.message.id.as_deref(), Some("gen-1"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 90);
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 40);
        assert_eq!(entries[0].project_path.as_ref(), "my-app");
    }

    #[test]
    fn loads_ndjson_runs_and_dedupes_matching_index_rows() {
        let fixture = fs_fixture!({});
        let usage = r#"{"inputTokens":80,"outputTokens":8,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":88}"#;
        create_runs_db(
            &fixture.path("projects/api/sdk-agent-store/hash2/index.db"),
            usage,
        );
        let _ = fixture.write_file(
            "projects/api/sdk-agent-store/hash2/runs.ndjson",
            format!(
                r#"{{"runId":"run-1","agentId":"agent-abc","model":"grok-4.6","usage":{usage},"startedAt":1750000000000}}"#
            ) + "\n",
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 80);
    }

    #[test]
    fn keeps_usage_when_a_sibling_database_is_corrupt() {
        let fixture = fs_fixture!({
            "chats/ws/bad/store.db": "not-a-sqlite-file",
        });
        create_runs_db(
            &fixture.path("projects/ok/sdk-agent-store/h/index.db"),
            r#"{"inputTokens":11,"outputTokens":2,"totalTokens":13}"#,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 11);
    }

    #[test]
    fn sorts_entries_by_timestamp_across_sources() {
        let fixture = fs_fixture!({});
        let _ = fixture.write_file(
            "projects/api/sdk-agent-store/h/runs.ndjson",
            r#"{"runId":"late","agentId":"sess","model":"grok-4.6","usage":{"inputTokens":2,"outputTokens":1,"totalTokens":3},"startedAt":1750100000000}
{"runId":"early","agentId":"sess","model":"grok-4.6","usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2},"startedAt":1750000000000}
"#,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("early"));
        assert_eq!(entries[1].data.message.id.as_deref(), Some("late"));
    }

    #[test]
    fn has_data_detects_store_db() {
        let fixture = fs_fixture!({
            "chats/ws/sess/store.db": "",
        });
        let _guard = with_cursor_home(fixture.root());
        assert!(has_data());
    }

    #[test]
    fn has_data_is_false_when_home_has_no_cursor_sources() {
        let fixture = fs_fixture!({
            "plugins/ignore-me/store.db": "",
            "projects/app/agent-transcripts/sess.jsonl": "{}\n",
        });
        let _guard = with_cursor_home(fixture.root());
        assert!(!has_data());
    }

    fn create_index_db(path: &Path, runs_sql: &str, events_sql: Option<&str>) {
        ensure_parent(path);
        let db = sqlite::open(path).unwrap();
        db.execute(
            "CREATE TABLE runs (run_id TEXT, agent_id TEXT, model TEXT, usage TEXT, started_at INTEGER)",
        )
        .unwrap();
        db.execute(runs_sql).unwrap();
        if let Some(events_sql) = events_sql {
            db.execute(
                "CREATE TABLE run_events (run_id TEXT, seq INTEGER, payload_json TEXT, created_at INTEGER)",
            )
            .unwrap();
            db.execute(events_sql).unwrap();
        }
    }

    #[test]
    fn loads_second_agent_from_run_events_when_runs_usage_is_empty() {
        let fixture = fs_fixture!({});
        let usage_a = r#"{"type":"usage","agent_id":"agent-a","run_id":"run-a","usage":{"inputTokens":100,"outputTokens":10,"totalTokens":110}}"#;
        let usage_b = r#"{"type":"usage","agent_id":"agent-b","run_id":"run-b","usage":{"inputTokens":20,"outputTokens":2,"totalTokens":22}}"#;
        create_index_db(
            &fixture.path("projects/app/sdk-agent-store/h/index.db"),
            r#"INSERT INTO runs (run_id, agent_id, model, usage, started_at) VALUES
                ('run-a', 'agent-a', 'grok-4.6', '{"inputTokens":100,"outputTokens":10,"totalTokens":110}', 1750000000000),
                ('run-b', 'agent-b', 'grok-4.6', NULL, 1750000100000)"#,
            Some(&format!(
                "INSERT INTO run_events (run_id, seq, payload_json, created_at) VALUES
                    ('run-a', 1, '{usage_a}', 1750000000000),
                    ('run-b', 1, '{usage_b}', 1750000100000)"
            )),
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        let mut sessions: Vec<_> = entries
            .iter()
            .map(|entry| entry.session_id.as_ref())
            .collect();
        sessions.sort_unstable();
        assert_eq!(sessions, ["agent-a", "agent-b"]);
        let agent_b = entries
            .iter()
            .find(|entry| entry.session_id.as_ref() == "agent-b")
            .unwrap();
        assert_eq!(agent_b.data.message.usage.input_tokens, 20);
        assert_eq!(agent_b.data.message.usage.output_tokens, 2);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.data.message.usage.input_tokens)
                .sum::<u64>(),
            120
        );
    }

    #[test]
    fn loads_run_events_ndjson_when_runs_usage_is_empty() {
        let fixture = fs_fixture!({});
        let _ = fixture.write_file(
            "projects/api/sdk-agent-store/h/runs.ndjson",
            r#"{"runId":"run-new","agentId":"agent-new","model":{"id":"composer-2.5"},"startedAt":1750010000000}
"#,
        );
        let _ = fixture.write_file(
            "projects/api/sdk-agent-store/h/run_events.ndjson",
            r#"{"runId":"run-new","seq":3,"createdAt":1750010000500,"payload":{"type":"usage","agent_id":"agent-new","run_id":"run-new","usage":{"inputTokens":50,"outputTokens":5,"totalTokens":55}}}
"#,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "agent-new");
        assert_eq!(entries[0].model.as_deref(), Some("composer-2.5"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 50);
        assert_eq!(entries[0].data.message.usage.output_tokens, 5);
    }

    #[test]
    fn loads_sdk_agent_store_when_index_db_is_absent() {
        let fixture = fs_fixture!({});
        let mut blob = vec![0x0a, 0x05];
        blob.extend_from_slice(
            br#"{"timestamp":1750000000000,"model":"grok-4.6","conversation_id":"agent-from-store","generation_id":"gen-store","input_tokens":30,"output_tokens":4,"cache_read_tokens":0,"cache_write_tokens":0}"#,
        );
        create_store_db(
            &fixture.path("projects/app/sdk-agent-store/h/agents/deadbeef/store.db"),
            &blob,
            false,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "agent-from-store");
        assert_eq!(entries[0].data.message.usage.input_tokens, 30);
    }

    #[test]
    fn skips_sdk_agent_store_blobs_when_index_db_exists() {
        let fixture = fs_fixture!({});
        create_runs_db(
            &fixture.path("projects/app/sdk-agent-store/h/index.db"),
            r#"{"inputTokens":100,"outputTokens":20,"totalTokens":120}"#,
        );
        let mut blob = vec![0x0a, 0x05];
        blob.extend_from_slice(
            br#"{"timestamp":1750000000000,"model":"grok-4.6","conversation_id":"agent-from-store","generation_id":"gen-store","input_tokens":999,"output_tokens":9,"cache_read_tokens":0,"cache_write_tokens":0}"#,
        );
        create_store_db(
            &fixture.path("projects/app/sdk-agent-store/h/agents/deadbeef/store.db"),
            &blob,
            false,
        );
        let _guard = with_cursor_home(fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "agent-abc");
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
    }
}

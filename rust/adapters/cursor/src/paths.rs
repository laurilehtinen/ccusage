use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

use crate::Result;

/// Override for the Cursor Agent home (one root, or comma-separated roots).
pub(crate) const CURSOR_AGENT_HOME_ENV: &str = "CURSOR_AGENT_HOME";

/// Resolve Cursor data roots from `CURSOR_AGENT_HOME`, then `~/.cursor`.
pub(super) fn resolve_roots() -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(paths) = env::var(CURSOR_AGENT_HOME_ENV) {
        for raw in paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            let path = PathBuf::from(raw);
            if path.is_dir() && seen.insert(path.clone()) {
                roots.push(path);
            }
        }
        return Ok(roots);
    }

    let Some(home) = crate::home::home_dir() else {
        return Err(crate::cli_error("home directory is not set"));
    };
    let path = home.join(".cursor");
    if path.is_dir() {
        roots.push(path);
    }
    Ok(roots)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorDbKind {
    /// CLI / ACP `store.db` (`meta` + `blobs`).
    Store,
    /// SDK catalog `index.db` (`agents` / `runs` / `run_events`).
    Index,
}

#[derive(Debug, Clone)]
pub(super) struct CursorDbFile {
    pub path: PathBuf,
    pub kind: CursorDbKind,
}

#[derive(Debug, Clone)]
pub(super) struct CursorNdjsonFile {
    pub path: PathBuf,
}

/// Discover `store.db`, SDK `index.db`, per-agent stores, and optional JSONL.
pub(super) fn discover_source_files() -> Result<(Vec<CursorDbFile>, Vec<CursorNdjsonFile>)> {
    let mut dbs = Vec::new();
    let mut ndjson = Vec::new();
    let mut seen_db = HashSet::new();
    let mut seen_ndjson = HashSet::new();
    for root in resolve_roots()? {
        collect_named_files(
            &root.join("chats"),
            "store.db",
            CursorDbKind::Store,
            &mut dbs,
            &mut seen_db,
        );
        collect_named_files(
            &root.join("chats"),
            "index.db",
            CursorDbKind::Index,
            &mut dbs,
            &mut seen_db,
        );
        collect_named_files_ndjson(
            &root.join("chats"),
            "runs.ndjson",
            &mut ndjson,
            &mut seen_ndjson,
        );
        collect_named_files_ndjson(
            &root.join("chats"),
            "run_events.ndjson",
            &mut ndjson,
            &mut seen_ndjson,
        );
        collect_named_files(
            &root.join("acp-sessions"),
            "store.db",
            CursorDbKind::Store,
            &mut dbs,
            &mut seen_db,
        );
        collect_named_files(
            &root.join("acp-sessions"),
            "index.db",
            CursorDbKind::Index,
            &mut dbs,
            &mut seen_db,
        );
        collect_sdk_stores(
            &root.join("projects"),
            &mut dbs,
            &mut ndjson,
            &mut seen_db,
            &mut seen_ndjson,
        );
    }
    dbs.sort_by(|a, b| a.path.cmp(&b.path));
    ndjson.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((dbs, ndjson))
}

/// Session id and project path derived from a discovered file path.
pub(super) fn identity_from_path(path: &Path) -> (String, String) {
    let parts: Vec<&str> = path
        .iter()
        .filter_map(|component| component.to_str())
        .collect();
    if let Some(index) = parts.iter().position(|part| *part == "chats") {
        let project = parts.get(index + 1).copied().unwrap_or("cursor");
        let session = parts.get(index + 2).copied().unwrap_or("session");
        return (session.to_string(), project.to_string());
    }
    if let Some(index) = parts.iter().position(|part| *part == "acp-sessions") {
        let session = parts.get(index + 1).copied().unwrap_or("session");
        return (session.to_string(), "acp".to_string());
    }
    if let Some(index) = parts.iter().position(|part| *part == "projects") {
        let project = parts.get(index + 1).copied().unwrap_or("cursor");
        let session = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("sdk");
        return (session.to_string(), project.to_string());
    }
    let session = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("cursor");
    (session.to_string(), "cursor".to_string())
}

/// Chat-folder UUID for interactive CLI / ACP stores (`chats/<hash>/<uuid>/...`).
pub(super) fn cli_session_id_from_path(path: &Path) -> Option<String> {
    let parts: Vec<&str> = path
        .iter()
        .filter_map(|component| component.to_str())
        .collect();
    if let Some(index) = parts.iter().position(|part| *part == "chats") {
        return parts.get(index + 2).map(|session| (*session).to_string());
    }
    if let Some(index) = parts.iter().position(|part| *part == "acp-sessions") {
        return parts.get(index + 1).map(|session| (*session).to_string());
    }
    None
}

fn collect_sdk_stores(
    projects: &Path,
    dbs: &mut Vec<CursorDbFile>,
    ndjson: &mut Vec<CursorNdjsonFile>,
    seen_db: &mut HashSet<PathBuf>,
    seen_ndjson: &mut HashSet<PathBuf>,
) {
    let Ok(slugs) = fs::read_dir(projects) else {
        return;
    };
    for slug in slugs.filter_map(std::result::Result::ok) {
        let sdk_root = slug.path().join("sdk-agent-store");
        if !sdk_root.is_dir() {
            continue;
        }
        collect_named_files(&sdk_root, "index.db", CursorDbKind::Index, dbs, seen_db);
        collect_sdk_agent_stores(&sdk_root, dbs, seen_db);
        collect_named_files_ndjson(&sdk_root, "runs.ndjson", ndjson, seen_ndjson);
        collect_named_files_ndjson(&sdk_root, "run_events.ndjson", ndjson, seen_ndjson);
    }
}

/// Per-agent `store.db` under `agents/<sha>/`. Skip when the same SDK hash
/// already has an `index.db`, so catalog usage is not double-counted from blobs.
fn collect_sdk_agent_stores(
    sdk_root: &Path,
    dbs: &mut Vec<CursorDbFile>,
    seen_db: &mut HashSet<PathBuf>,
) {
    let mut agent_stores = Vec::new();
    let mut seen_agent = HashSet::new();
    collect_named_files(
        sdk_root,
        "store.db",
        CursorDbKind::Store,
        &mut agent_stores,
        &mut seen_agent,
    );
    for file in agent_stores {
        if sdk_hash_has_index_db(&file.path) {
            continue;
        }
        if seen_db.insert(file.path.clone()) {
            dbs.push(file);
        }
    }
}

fn sdk_hash_has_index_db(store_db: &Path) -> bool {
    for ancestor in store_db.ancestors().skip(1) {
        if ancestor
            .file_name()
            .is_some_and(|name| name == "sdk-agent-store")
        {
            return false;
        }
        if ancestor.join("index.db").is_file() {
            return true;
        }
    }
    false
}

fn collect_named_files(
    dir: &Path,
    file_name: &str,
    kind: CursorDbKind,
    files: &mut Vec<CursorDbFile>,
    seen: &mut HashSet<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some(file_name)
            && seen.insert(path.clone())
        {
            files.push(CursorDbFile { path, kind });
        } else if file_type.is_dir() {
            collect_named_files(&path, file_name, kind, files, seen);
        }
    }
}

fn collect_named_files_ndjson(
    dir: &Path,
    file_name: &str,
    files: &mut Vec<CursorNdjsonFile>,
    seen: &mut HashSet<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some(file_name)
            && seen.insert(path.clone())
        {
            files.push(CursorNdjsonFile { path });
        } else if file_type.is_dir() {
            collect_named_files_ndjson(&path, file_name, files, seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::{EnvVarsGuard, fs_fixture};
    use std::ffi::OsString;

    fn with_cursor_home(root: &Path) -> EnvVarsGuard {
        EnvVarsGuard::set_many([(
            CURSOR_AGENT_HOME_ENV,
            Some(OsString::from(root.as_os_str())),
        )])
    }

    #[test]
    fn discovers_cli_store_acp_store_and_sdk_index() {
        let fixture = fs_fixture!({
            "chats/abc/sess-1/store.db": "",
            "acp-sessions/sess-2/store.db": "",
            "projects/my-app/sdk-agent-store/hash1/index.db": "",
            "projects/my-app/sdk-agent-store/hash1/runs.ndjson": "{}\n",
            "projects/my-app/sdk-agent-store/hash1/run_events.ndjson": "{}\n",
            "projects/my-app/sdk-agent-store/hash1/agents/abc/store.db": "",
            "projects/my-app/agent-transcripts/sess-1/sess-1.jsonl": "{}\n",
            "plugins/ignore-me/store.db": "",
        });
        let _guard = with_cursor_home(fixture.root());
        let (dbs, ndjson) = discover_source_files().unwrap();
        let kinds: Vec<_> = dbs.iter().map(|file| file.kind).collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == CursorDbKind::Store)
                .count(),
            2
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == CursorDbKind::Index)
                .count(),
            1
        );
        assert_eq!(ndjson.len(), 2);
        assert!(
            dbs.iter()
                .all(|file| !file.path.to_string_lossy().contains("plugins"))
        );
        assert!(
            dbs.iter()
                .all(|file| !file.path.to_string_lossy().contains("agent-transcripts"))
        );
        assert!(
            dbs.iter()
                .all(|file| !file.path.to_string_lossy().contains("/agents/"))
        );
    }

    #[test]
    fn discovers_per_agent_store_when_index_db_is_absent() {
        let fixture = fs_fixture!({
            "projects/my-app/sdk-agent-store/hash1/agents/abc/store.db": "",
            "projects/my-app/sdk-agent-store/hash1/run_events.ndjson": "{}\n",
        });
        let _guard = with_cursor_home(fixture.root());
        let (dbs, ndjson) = discover_source_files().unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].kind, CursorDbKind::Store);
        assert!(dbs[0].path.to_string_lossy().ends_with("store.db"));
        assert_eq!(ndjson.len(), 1);
        assert!(
            ndjson[0]
                .path
                .to_string_lossy()
                .ends_with("run_events.ndjson")
        );
    }

    #[test]
    fn empty_override_yields_no_roots() {
        let fixture = fs_fixture!({});
        let missing = fixture.path("missing");
        let _guard = EnvVarsGuard::set_many([(
            CURSOR_AGENT_HOME_ENV,
            Some(OsString::from(missing.as_os_str())),
        )]);
        let (dbs, ndjson) = discover_source_files().unwrap();
        assert!(dbs.is_empty());
        assert!(ndjson.is_empty());
    }

    #[test]
    fn comma_separated_homes_are_searched() {
        let first = fs_fixture!({
            "chats/ws/sess-a/store.db": "",
        });
        let second = fs_fixture!({
            "acp-sessions/sess-b/store.db": "",
        });
        let _guard = EnvVarsGuard::set_many([(
            CURSOR_AGENT_HOME_ENV,
            Some(OsString::from(format!(
                "{},{}",
                first.root().display(),
                second.root().display()
            ))),
        )]);
        let (dbs, _) = discover_source_files().unwrap();
        assert_eq!(dbs.len(), 2);
    }

    #[test]
    fn identity_from_path_uses_chat_workspace_and_session() {
        let (session, project) =
            identity_from_path(Path::new("/home/me/.cursor/chats/my-app/sess-1/store.db"));
        assert_eq!(session, "sess-1");
        assert_eq!(project, "my-app");
    }

    #[test]
    fn identity_from_path_uses_acp_session_folder() {
        let (session, project) =
            identity_from_path(Path::new("/home/me/.cursor/acp-sessions/sess-2/store.db"));
        assert_eq!(session, "sess-2");
        assert_eq!(project, "acp");
    }

    #[test]
    fn identity_from_path_uses_sdk_project_slug() {
        let (session, project) = identity_from_path(Path::new(
            "/home/me/.cursor/projects/my-app/sdk-agent-store/hash1/index.db",
        ));
        assert_eq!(session, "hash1");
        assert_eq!(project, "my-app");
    }

    #[test]
    fn cli_session_id_from_path_uses_chat_folder_uuid() {
        let session = cli_session_id_from_path(Path::new(
            "/home/me/.cursor/chats/eea42053be10a3da86aa61bbf93e53bb/c5f09ff7-69d5-414a-a4c9-b8ac792420fd/store.db",
        ));
        assert_eq!(
            session.as_deref(),
            Some("c5f09ff7-69d5-414a-a4c9-b8ac792420fd")
        );
    }

    #[test]
    fn discovers_index_db_nested_under_cli_chat_folder() {
        let fixture = fs_fixture!({
            "chats/eea42053be10a3da86aa61bbf93e53bb/c5f09ff7-69d5-414a-a4c9-b8ac792420fd/index.db": "",
        });
        let _guard = with_cursor_home(fixture.root());
        let (dbs, _) = discover_source_files().unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].kind, CursorDbKind::Index);
    }
}

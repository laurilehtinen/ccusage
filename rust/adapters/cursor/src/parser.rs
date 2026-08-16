use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use jiff::tz::TimeZone as JiffTimeZone;
use serde_json::Value;

use crate::{
    LoadedEntry, PricingMap, TimestampMs, TokenUsageRaw, UsageEntry, UsageMessage,
    calculate_cost_for_usage, cli::CostMode, format_date_tz, format_rfc3339_millis,
    missing_pricing_model_for_candidates,
};

#[derive(Debug, Clone)]
pub(super) struct CursorUsageRecord {
    pub timestamp: TimestampMs,
    pub session_id: String,
    pub message_id: String,
    pub model: String,
    pub model_id: Option<String>,
    pub usage: TokenUsageRaw,
    pub cost_usd: Option<f64>,
    pub project_path: String,
}

/// Map a recorded usage object plus identity fields into a ccusage entry.
pub(super) fn record_to_loaded_entry(
    record: CursorUsageRecord,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: &PricingMap,
) -> LoadedEntry {
    let timestamp_text = format_rfc3339_millis(record.timestamp);
    let data = UsageEntry {
        session_id: Some(record.session_id.clone()),
        timestamp: timestamp_text,
        version: None,
        message: UsageMessage {
            usage: record.usage,
            model: Some(record.model.clone()),
            id: Some(record.message_id.clone()),
        },
        cost_usd: record.cost_usd,
        request_id: Some(record.message_id.clone()),
        is_api_error_message: None,
        is_sidechain: None,
    };
    let cost = calculate_cursor_cost(&record, mode, pricing);
    let missing_pricing_model = missing_cursor_pricing(&record, mode, pricing);
    LoadedEntry {
        date: format_date_tz(record.timestamp, tz),
        timestamp: record.timestamp,
        project: Arc::from("cursor"),
        session_id: Arc::from(record.session_id.as_str()),
        project_path: Arc::from(record.project_path.as_str()),
        cost,
        credits: None,
        extra_total_tokens: 0,
        message_count: None,
        model: Some(record.model),
        usage_limit_reset_time: None,
        missing_pricing_model,
        data,
    }
}

/// Parse one SDK `runs` row / `runs.ndjson` document.
pub(super) fn record_from_sdk_run(
    value: &Value,
    fallback_session: &str,
    project_path: &str,
) -> Option<CursorUsageRecord> {
    let run_id =
        json_string(value, &["runId", "run_id"]).unwrap_or_else(|| fallback_session.to_string());
    let agent_id = json_string(value, &["agentId", "agent_id"])
        .unwrap_or_else(|| fallback_session.to_string());
    let session_id = if agent_id.is_empty() {
        run_id.clone()
    } else {
        agent_id
    };
    let model_value = value.get("model");
    let (display, model_id) = model_from_value(model_value);
    let usage_value = value.get("usage").or_else(|| value.get("usage_json"))?;
    let parsed = parse_usage_value(usage_value)?;
    let timestamp = timestamp_from_value(value)?;
    let model = display
        .or(model_id.clone())
        .filter(|model| !model.is_empty())?;
    Some(CursorUsageRecord {
        timestamp,
        message_id: run_id,
        session_id,
        model,
        model_id,
        usage: parsed.usage,
        cost_usd: parsed.cost_usd,
        project_path: project_path.to_string(),
    })
}

struct SdkRunMeta {
    agent_id: String,
    model: Option<Value>,
    timestamp: Option<TimestampMs>,
}

/// Combine SDK `runs` rows with `run_events` usage payloads.
///
/// Per-turn `type: "usage"` events win over cumulative `runs.usage` for the
/// same `run_id`, so a catalog that stores tokens only on the event stream
/// still produces one record per agent turn without double-counting.
pub(super) fn records_from_sdk_index(
    runs: &[Value],
    events: &[Value],
    fallback_session: &str,
    project_path: &str,
) -> Vec<CursorUsageRecord> {
    let mut metas = HashMap::new();
    for run in runs {
        let Some(run_id) = json_string(run, &["runId", "run_id"]) else {
            continue;
        };
        let agent_id = json_string(run, &["agentId", "agent_id"])
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| fallback_session.to_string());
        metas.insert(
            run_id,
            SdkRunMeta {
                agent_id,
                model: run.get("model").cloned(),
                timestamp: timestamp_from_value(run),
            },
        );
    }

    let mut records = Vec::new();
    let mut runs_with_usage_events = HashSet::new();
    for event in events {
        let mut payloads = Vec::new();
        collect_usage_messages(event, &mut payloads);
        if payloads.is_empty() {
            continue;
        }
        let event_run_id = json_string(event, &["runId", "run_id"]);
        let seq = event
            .as_object()
            .and_then(|object| json_u64_keys(object, &["seq", "sequence"]))
            .unwrap_or(0);
        for (index, payload) in payloads.into_iter().enumerate() {
            let run_id = json_string(&payload, &["runId", "run_id"])
                .or_else(|| event_run_id.clone())
                .unwrap_or_else(|| fallback_session.to_string());
            let Some(record) = record_from_usage_event(
                &payload,
                event,
                metas.get(&run_id),
                &run_id,
                seq,
                index,
                fallback_session,
                project_path,
            ) else {
                continue;
            };
            runs_with_usage_events.insert(run_id);
            records.push(record);
        }
    }

    for run in runs {
        let Some(run_id) = json_string(run, &["runId", "run_id"]) else {
            if let Some(record) = record_from_sdk_run(run, fallback_session, project_path) {
                records.push(record);
            }
            continue;
        };
        if runs_with_usage_events.contains(&run_id) {
            continue;
        }
        if let Some(record) = record_from_sdk_run(run, fallback_session, project_path) {
            records.push(record);
        }
    }
    records
}

fn collect_usage_messages(value: &Value, out: &mut Vec<Value>) {
    if is_usage_message(value) {
        out.push(value.clone());
        return;
    }
    if let Value::String(text) = value
        && let Ok(parsed) = serde_json::from_str::<Value>(text)
    {
        collect_usage_messages(&parsed, out);
        return;
    }
    match value {
        Value::Object(map) => {
            for child in map.values() {
                collect_usage_messages(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_usage_messages(child, out);
            }
        }
        _ => {}
    }
}

fn is_usage_message(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("usage") && value.get("usage").is_some()
}

#[allow(clippy::too_many_arguments)]
fn record_from_usage_event(
    payload: &Value,
    event: &Value,
    meta: Option<&SdkRunMeta>,
    run_id: &str,
    seq: u64,
    index: usize,
    fallback_session: &str,
    project_path: &str,
) -> Option<CursorUsageRecord> {
    let parsed = payload
        .get("usage")
        .and_then(parse_usage_value)
        .or_else(|| parse_usage_value(payload))?;
    let timestamp = timestamp_from_value(payload)
        .or_else(|| timestamp_from_value(event))
        .or_else(|| meta.and_then(|meta| meta.timestamp))?;
    let (display, nested_model_id) = model_from_value(payload.get("model"));
    let (meta_display, meta_model_id) = model_from_value(meta.and_then(|meta| meta.model.as_ref()));
    let model_id = nested_model_id.or(meta_model_id);
    let model = display
        .or(model_id.clone())
        .or(meta_display)
        .filter(|model| !model.is_empty())?;
    let session_id = json_string(payload, &["agentId", "agent_id"])
        .or_else(|| meta.map(|meta| meta.agent_id.clone()))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| fallback_session.to_string());
    Some(CursorUsageRecord {
        timestamp,
        session_id,
        message_id: format!("{run_id}:{seq}:{index}"),
        model,
        model_id,
        usage: parsed.usage,
        cost_usd: parsed.cost_usd,
        project_path: project_path.to_string(),
    })
}

struct ParsedUsage {
    usage: TokenUsageRaw,
    cost_usd: Option<f64>,
}

fn parse_usage_value(value: &Value) -> Option<ParsedUsage> {
    let object = value.as_object()?;
    let input = json_u64_keys(object, &["inputTokens", "input_tokens", "prompt_tokens"])?;
    let output = json_u64_keys(
        object,
        &["outputTokens", "output_tokens", "completion_tokens"],
    )
    .unwrap_or(0);
    let cache_read = json_u64_keys(
        object,
        &[
            "cacheReadTokens",
            "cache_read_tokens",
            "cache_read_input_tokens",
        ],
    )
    .unwrap_or(0);
    let cache_write = json_u64_keys(
        object,
        &[
            "cacheWriteTokens",
            "cache_write_tokens",
            "cache_write_input_tokens",
            "cacheCreationTokens",
            "cache_creation_input_tokens",
        ],
    )
    .unwrap_or(0);
    let total = json_u64_keys(object, &["totalTokens", "total_tokens"]);
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
        return None;
    }
    let (input_tokens, cache_read_input_tokens, cache_creation_input_tokens) =
        split_input_tokens(input, cache_read, cache_write, total);
    if input_tokens == 0
        && output == 0
        && cache_read_input_tokens == 0
        && cache_creation_input_tokens == 0
    {
        return None;
    }
    let cost_usd = json_f64_keys(object, &["costUSD", "cost_usd", "costUsd"]).or_else(|| {
        json_f64_keys(object, &["chargedCents", "charged_cents"]).map(|cents| cents / 100.0)
    });
    Some(ParsedUsage {
        usage: TokenUsageRaw {
            input_tokens,
            output_tokens: output,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            speed: None,
            cache_creation: None,
        },
        cost_usd,
    })
}

/// Inclusive hook/SDK input keeps cache inside `inputTokens`. Exclusive totals
/// (`totalTokens == input + output + cacheRead + cacheWrite`) leave input as
/// the uncached remainder.
fn split_input_tokens(
    input: u64,
    cache_read: u64,
    cache_write: u64,
    total: Option<u64>,
) -> (u64, u64, u64) {
    let cached = cache_read.saturating_add(cache_write);
    let exclusive_total = input.saturating_add(cache_read).saturating_add(cache_write);
    let inclusive = match total {
        Some(total) if exclusive_total > 0 => {
            let exclusive_distance = total.abs_diff(exclusive_total);
            let inclusive_distance = total.abs_diff(input);
            exclusive_distance < inclusive_distance
        }
        _ => false,
    };
    if inclusive {
        return (input, cache_read, cache_write);
    }
    if input >= cached {
        (input - cached, cache_read, cache_write)
    } else {
        (input, cache_read, cache_write)
    }
}

pub(super) fn pricing_candidates(display: &str, model_id: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut push = |value: String| {
        if !value.is_empty() && !candidates.iter().any(|existing| existing == &value) {
            candidates.push(value);
        }
    };

    if let Some(model_id) = model_id.map(str::trim).filter(|id| !id.is_empty()) {
        push_model_forms(&mut push, model_id);
    }
    push_model_forms(&mut push, display.trim());
    if let Some(stripped) = display.trim().strip_prefix("cursor-") {
        push_model_forms(&mut push, stripped);
    }
    candidates
}

fn push_model_forms(push: &mut impl FnMut(String), raw: &str) {
    if raw.is_empty() {
        return;
    }
    push(raw.to_string());
    let normalized = match raw.strip_suffix("-build") {
        Some(stem) if stem != "grok" && !stem.is_empty() => stem,
        _ => raw,
    };
    if normalized != raw {
        push(normalized.to_string());
    }
    if normalized.starts_with("grok") {
        push(format!("xai/{normalized}"));
        push(format!("x-ai/{normalized}"));
    }
    if normalized == "grok-build" {
        push("grok-build-0.1".to_string());
        push("xai/grok-build-0.1".to_string());
        push("x-ai/grok-build-0.1".to_string());
    }
}

fn calculate_cursor_cost(record: &CursorUsageRecord, mode: CostMode, pricing: &PricingMap) -> f64 {
    match mode {
        CostMode::Display => record.cost_usd.unwrap_or(0.0),
        CostMode::Auto => record
            .cost_usd
            .unwrap_or_else(|| calculate_from_tokens(record, pricing)),
        CostMode::Calculate => calculate_from_tokens(record, pricing),
    }
}

fn calculate_from_tokens(record: &CursorUsageRecord, pricing: &PricingMap) -> f64 {
    for candidate in pricing_candidates(&record.model, record.model_id.as_deref()) {
        let cost = calculate_cost_for_usage(
            Some(&candidate),
            record.usage,
            None,
            CostMode::Calculate,
            Some(pricing),
        );
        if cost.is_finite() && cost > 0.0 {
            return cost;
        }
    }
    0.0
}

fn missing_cursor_pricing(
    record: &CursorUsageRecord,
    mode: CostMode,
    pricing: &PricingMap,
) -> Option<String> {
    if mode == CostMode::Display || (mode == CostMode::Auto && record.cost_usd.is_some()) {
        return None;
    }
    missing_pricing_model_for_candidates(
        &record.model,
        pricing_candidates(&record.model, record.model_id.as_deref()),
        crate::total_usage_tokens(record.usage),
        Some(pricing),
    )
}

fn model_from_value(value: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(value) = value else {
        return (None, None);
    };
    if let Some(id) = value.as_str().map(str::trim).filter(|id| !id.is_empty()) {
        return (Some(id.to_string()), None);
    }
    let object = match value.as_object() {
        Some(object) => object,
        None => return (None, None),
    };
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    (id.clone(), id)
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(text) = object
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_string());
        }
    }
    None
}

fn json_u64_keys(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(value) = object.get(*key)
            && let Some(number) = json_u64(value)
        {
            return Some(number);
        }
    }
    None
}

fn json_f64_keys(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(value) = object.get(*key)
            && let Some(number) = json_f64(value)
        {
            return Some(number);
        }
    }
    None
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n.max(0)).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n.trunc() as u64)
        })
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
}

fn timestamp_from_value(value: &Value) -> Option<TimestampMs> {
    const KEYS: &[&str] = &[
        "timestamp",
        "timestamp_ms",
        "startedAt",
        "started_at",
        "createdAt",
        "created_at",
        "endedAt",
        "ended_at",
        "updatedAt",
        "updated_at",
    ];
    for key in KEYS {
        if let Some(timestamp) = timestamp_from_number_value(value.get(*key)) {
            return Some(timestamp);
        }
    }
    None
}

/// Walk a JSON tree for SDK runs and hook/blob usage payloads.
pub(super) fn records_from_json_value(
    value: &Value,
    fallback_session: &str,
    project_path: &str,
) -> Vec<CursorUsageRecord> {
    let mut records = Vec::new();
    collect_records(
        value,
        InheritedIdentity {
            session: fallback_session.to_string(),
            pin_session: false,
            model: None,
            model_id: None,
            timestamp: None,
            project_path: project_path.to_string(),
        },
        &mut records,
    );
    records
}

#[derive(Debug, Clone)]
pub(super) struct StoreSessionMeta {
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub timestamp: Option<TimestampMs>,
}

/// Read CLI `meta` JSON (plain or hex-encoded) for session id, model, and time.
pub(super) fn store_session_meta_from_value(value: &Value) -> Option<StoreSessionMeta> {
    let decoded = value.get("value").map(coerce_json_value);
    let object = decoded
        .as_ref()
        .unwrap_or(value)
        .as_object()
        .or_else(|| value.as_object())?;
    let object_value = Value::Object(object.clone());
    let agent_id = json_string(&object_value, &["agentId", "agent_id"]);
    let model = json_string(
        &object_value,
        &["lastUsedModel", "last_used_model", "model"],
    );
    let timestamp = timestamp_from_value(&object_value);
    if agent_id.is_none() && model.is_none() && timestamp.is_none() {
        return None;
    }
    Some(StoreSessionMeta {
        agent_id,
        model,
        timestamp,
    })
}

/// Parse blobs from a CLI/ACP `store.db`, applying hex `meta` identity.
///
/// When `pin_session` is set, the chat-folder UUID stays the session id even
/// if a blob also carries an `agent-...` SDK id.
pub(super) fn records_from_store_payload(
    value: &Value,
    fallback_session: &str,
    project_path: &str,
    meta: Option<&StoreSessionMeta>,
    pin_session: bool,
) -> Vec<CursorUsageRecord> {
    let session = if pin_session {
        fallback_session.to_string()
    } else {
        meta.and_then(|meta| meta.agent_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| fallback_session.to_string())
    };
    let mut records = Vec::new();
    collect_records(
        value,
        InheritedIdentity {
            session: session.clone(),
            pin_session,
            model: meta.and_then(|meta| meta.model.clone()),
            model_id: None,
            timestamp: meta.and_then(|meta| meta.timestamp),
            project_path: project_path.to_string(),
        },
        &mut records,
    );
    if pin_session {
        for record in &mut records {
            record.session_id = session.clone();
        }
    }
    records
}

pub(super) fn parse_json_or_hex(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let bytes = hex_decode(trimmed)?;
    let decoded = std::str::from_utf8(&bytes).ok()?;
    serde_json::from_str(decoded).ok()
}

fn coerce_json_value(value: &Value) -> Value {
    match value {
        Value::String(text) => parse_json_or_hex(text).unwrap_or_else(|| value.clone()),
        other => other.clone(),
    }
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.is_ascii() || hex.is_empty() {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok())
        .collect()
}

#[derive(Clone)]
struct InheritedIdentity {
    session: String,
    pin_session: bool,
    model: Option<String>,
    model_id: Option<String>,
    timestamp: Option<TimestampMs>,
    project_path: String,
}

fn collect_records(
    value: &Value,
    inherited: InheritedIdentity,
    records: &mut Vec<CursorUsageRecord>,
) {
    let inherited = inherit_identity(value, inherited);
    if let Some(record) = record_from_sdk_run(value, &inherited.session, &inherited.project_path)
        .or_else(|| record_from_inherited_payload(value, &inherited))
    {
        records.push(record);
        return;
    }
    match value {
        Value::Object(map) => {
            for child in map.values() {
                collect_records(child, inherited.clone(), records);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_records(child, inherited.clone(), records);
            }
        }
        _ => {}
    }
}

fn inherit_identity(value: &Value, mut inherited: InheritedIdentity) -> InheritedIdentity {
    if !inherited.pin_session
        && let Some(session) = json_string(
            value,
            &[
                "conversation_id",
                "conversationId",
                "session_id",
                "sessionId",
                "agentId",
                "agent_id",
            ],
        )
        .filter(|id| !id.is_empty())
    {
        inherited.session = session;
    }
    let (display, model_id) = model_from_value(value.get("model"));
    if display.is_some() {
        inherited.model = display;
    }
    if let Some(model_id) = model_id.or_else(|| json_string(value, &["model_id", "modelId"])) {
        inherited.model_id = Some(model_id);
    }
    if let Some(model) = json_string(value, &["lastUsedModel", "last_used_model", "modelName"])
        .or_else(|| {
            value
                .pointer("/providerOptions/cursor/modelName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
    {
        inherited.model = Some(model);
    }
    if let Some(timestamp) = timestamp_from_value(value) {
        inherited.timestamp = Some(timestamp);
    }
    inherited
}

fn record_from_inherited_payload(
    value: &Value,
    inherited: &InheritedIdentity,
) -> Option<CursorUsageRecord> {
    let parsed = parse_usage_value(value)
        .or_else(|| value.get("usage").and_then(parse_usage_value))
        .or_else(|| value.get("tokenCount").and_then(parse_usage_value))?;
    let timestamp = timestamp_from_value(value).or(inherited.timestamp)?;
    let (display, nested_model_id) = model_from_value(value.get("model"));
    let model_id = nested_model_id
        .or_else(|| json_string(value, &["model_id", "modelId"]))
        .or_else(|| inherited.model_id.clone());
    let model = display
        .or(model_id.clone())
        .or_else(|| inherited.model.clone())
        .filter(|model| !model.is_empty())?;
    let session_id = if inherited.pin_session {
        inherited.session.clone()
    } else {
        json_string(
            value,
            &[
                "conversation_id",
                "conversationId",
                "session_id",
                "sessionId",
            ],
        )
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| inherited.session.clone())
    };
    let message_id = json_string(
        value,
        &[
            "generation_id",
            "generationId",
            "requestId",
            "id",
            "runId",
            "run_id",
        ],
    )
    .filter(|id| !id.is_empty())
    .unwrap_or_else(|| format!("{session_id}:usage"));
    Some(CursorUsageRecord {
        timestamp,
        session_id,
        message_id,
        model,
        model_id,
        usage: parsed.usage,
        cost_usd: parsed.cost_usd,
        project_path: inherited.project_path.clone(),
    })
}

fn timestamp_from_number_value(value: Option<&Value>) -> Option<TimestampMs> {
    let value = value?;
    if let Some(number) = json_f64(value) {
        return timestamp_from_number(number);
    }
    value.as_str().and_then(crate::parse_ts_timestamp)
}

fn timestamp_from_number(value: f64) -> Option<TimestampMs> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let millis = if value > 1e12 {
        value
    } else if value > 1e9 {
        value * 1000.0
    } else {
        return None;
    };
    Some(TimestampMs::from_millis(millis.trunc() as i64))
}

/// Pull JSON objects out of protobuf-wrapped `store.db` blobs.
pub(super) fn json_objects_from_bytes(data: &[u8]) -> Vec<Value> {
    let mut objects = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let Some(rel) = memchr::memchr(b'{', &data[offset..]) else {
            break;
        };
        offset += rel;
        let mut stream = serde_json::Deserializer::from_slice(&data[offset..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(Value::Object(map))) => {
                let consumed = stream.byte_offset().max(1);
                objects.push(Value::Object(map));
                offset += consumed;
            }
            Some(Ok(_)) => offset += 1,
            Some(Err(_)) | None => offset += 1,
        }
    }
    objects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_inclusive_hook_input_like_cursor_staff_docs() {
        let parsed = parse_usage_value(&serde_json::json!({
            "input_tokens": 1_180_993,
            "output_tokens": 8_146,
            "cache_read_tokens": 1_007_022,
            "cache_write_tokens": 173_957,
        }))
        .unwrap();
        assert_eq!(parsed.usage.input_tokens, 14);
        assert_eq!(parsed.usage.cache_read_input_tokens, 1_007_022);
        assert_eq!(parsed.usage.cache_creation_input_tokens, 173_957);
        assert_eq!(parsed.usage.output_tokens, 8_146);
    }

    #[test]
    fn keeps_exclusive_sdk_input_when_total_matches_the_sum() {
        let parsed = parse_usage_value(&serde_json::json!({
            "inputTokens": 100,
            "outputTokens": 20,
            "cacheReadTokens": 40,
            "cacheWriteTokens": 10,
            "totalTokens": 170,
        }))
        .unwrap();
        assert_eq!(parsed.usage.input_tokens, 100);
        assert_eq!(parsed.usage.cache_read_input_tokens, 40);
        assert_eq!(parsed.usage.cache_creation_input_tokens, 10);
    }

    #[test]
    fn ignores_all_zero_usage() {
        assert!(
            parse_usage_value(&serde_json::json!({
                "inputTokens": 0,
                "outputTokens": 0,
                "cacheReadTokens": 0,
                "cacheWriteTokens": 0,
            }))
            .is_none()
        );
    }

    #[test]
    fn pricing_candidates_prefer_model_id_and_grok_build_family() {
        let candidates = pricing_candidates("cursor-grok-build-high-fast", Some("grok-build"));
        assert_eq!(candidates[0], "grok-build");
        assert!(candidates.contains(&"grok-build-0.1".to_string()));
        assert!(candidates.contains(&"cursor-grok-build-high-fast".to_string()));
        assert!(candidates.contains(&"grok-build-high-fast".to_string()));
    }

    #[test]
    fn record_from_sdk_run_uses_agent_id_as_session() {
        let record = record_from_sdk_run(
            &serde_json::json!({
                "runId": "run-1",
                "agentId": "agent-abc",
                "model": { "id": "grok-4.6" },
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "cacheReadTokens": 0,
                    "cacheWriteTokens": 0,
                    "totalTokens": 120
                },
                "startedAt": 1_750_000_000_000i64
            }),
            "fallback",
            "/workspace",
        )
        .unwrap();
        assert_eq!(record.session_id, "agent-abc");
        assert_eq!(record.message_id, "run-1");
        assert_eq!(record.model, "grok-4.6");
        assert_eq!(record.usage.input_tokens, 100);
        assert_eq!(record.usage.output_tokens, 20);
        assert_eq!(record.project_path, "/workspace");
    }

    #[test]
    fn record_from_hook_payload_uses_conversation_and_generation_ids() {
        let records = records_from_json_value(
            &serde_json::json!({
                "hook_event_name": "stop",
                "conversation_id": "conv-1",
                "generation_id": "gen-1",
                "model": "cursor-grok-4.6-high-fast",
                "model_id": "grok-4.6",
                "input_tokens": 140,
                "output_tokens": 20,
                "cache_read_tokens": 40,
                "cache_write_tokens": 10,
                "timestamp": 1_750_000_000_000i64
            }),
            "fallback",
            "Cursor",
        );
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.session_id, "conv-1");
        assert_eq!(record.message_id, "gen-1");
        assert_eq!(record.model, "cursor-grok-4.6-high-fast");
        assert_eq!(record.model_id.as_deref(), Some("grok-4.6"));
        assert_eq!(record.usage.input_tokens, 90);
        assert_eq!(record.usage.cache_read_input_tokens, 40);
        assert_eq!(record.usage.cache_creation_input_tokens, 10);
    }

    #[test]
    fn prices_grok_4_6_from_embedded_tables() {
        let pricing = PricingMap::load_embedded();
        let record = CursorUsageRecord {
            timestamp: crate::parse_ts_timestamp("2026-08-16T00:00:00.000Z").unwrap(),
            session_id: "sess".into(),
            message_id: "msg".into(),
            model: "cursor-grok-4.6-high-fast".into(),
            model_id: Some("grok-4.6".into()),
            usage: TokenUsageRaw {
                input_tokens: 1_000,
                output_tokens: 100,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                speed: None,
                cache_creation: None,
            },
            cost_usd: None,
            project_path: "Cursor".into(),
        };
        let cost = calculate_cursor_cost(&record, CostMode::Calculate, &pricing);
        assert!(cost > 0.0, "grok-4.6 should resolve through model_id");
    }

    #[test]
    fn extracts_json_objects_from_protobuf_wrapped_bytes() {
        let mut bytes = vec![0x0a, 0x05];
        bytes.extend_from_slice(br#"{"role":"assistant","usage":{"inputTokens":1}}"#);
        bytes.extend_from_slice(&[0x00, 0x00]);
        let objects = json_objects_from_bytes(&bytes);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0]["role"], "assistant");
    }

    #[test]
    fn nested_usage_inherits_parent_timestamp_and_model() {
        let records = records_from_json_value(
            &serde_json::json!({
                "createdAt": 1_750_000_000_000i64,
                "model": "grok-4.6",
                "conversation_id": "conv-nested",
                "message": {
                    "usage": {
                        "input_tokens": 50,
                        "output_tokens": 5,
                        "cache_read_tokens": 0,
                        "cache_write_tokens": 0
                    }
                }
            }),
            "fallback",
            "/workspace",
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "conv-nested");
        assert_eq!(records[0].model, "grok-4.6");
        assert_eq!(records[0].usage.input_tokens, 50);
        assert_eq!(records[0].usage.output_tokens, 5);
    }

    #[test]
    fn sdk_index_uses_run_events_when_runs_usage_is_empty() {
        let records = records_from_sdk_index(
            &[serde_json::json!({
                "run_id": "run-new",
                "agent_id": "agent-new",
                "model": { "id": "composer-2.5" },
                "started_at": 1_750_010_000_000i64
            })],
            &[serde_json::json!({
                "run_id": "run-new",
                "seq": 4,
                "created_at": 1_750_010_000_500i64,
                "payload_json": {
                    "type": "usage",
                    "agent_id": "agent-new",
                    "run_id": "run-new",
                    "usage": {
                        "inputTokens": 50,
                        "outputTokens": 5,
                        "cacheReadTokens": 0,
                        "cacheWriteTokens": 0,
                        "totalTokens": 55
                    }
                }
            })],
            "fallback",
            "my-app",
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "agent-new");
        assert_eq!(records[0].model, "composer-2.5");
        assert_eq!(records[0].usage.input_tokens, 50);
        assert_eq!(records[0].usage.output_tokens, 5);
        assert_eq!(records[0].message_id, "run-new:4:0");
        assert_eq!(records[0].project_path, "my-app");
    }

    #[test]
    fn sdk_index_keeps_separate_agents_without_double_counting_events() {
        let records = records_from_sdk_index(
            &[
                serde_json::json!({
                    "run_id": "run-a",
                    "agent_id": "agent-a",
                    "model": "grok-4.6",
                    "usage": {
                        "inputTokens": 100,
                        "outputTokens": 10,
                        "totalTokens": 110
                    },
                    "started_at": 1_750_000_000_000i64
                }),
                serde_json::json!({
                    "run_id": "run-b",
                    "agent_id": "agent-b",
                    "model": "grok-4.6",
                    "started_at": 1_750_000_100_000i64
                }),
            ],
            &[
                serde_json::json!({
                    "run_id": "run-a",
                    "seq": 1,
                    "created_at": 1_750_000_000_000i64,
                    "payload_json": {
                        "type": "usage",
                        "agent_id": "agent-a",
                        "run_id": "run-a",
                        "usage": {
                            "inputTokens": 100,
                            "outputTokens": 10,
                            "totalTokens": 110
                        }
                    }
                }),
                serde_json::json!({
                    "run_id": "run-b",
                    "seq": 1,
                    "created_at": 1_750_000_100_000i64,
                    "payload_json": {
                        "type": "usage",
                        "agent_id": "agent-b",
                        "run_id": "run-b",
                        "usage": {
                            "inputTokens": 20,
                            "outputTokens": 2,
                            "totalTokens": 22
                        }
                    }
                }),
            ],
            "fallback",
            "proj",
        );
        assert_eq!(records.len(), 2);
        let mut sessions: Vec<_> = records
            .iter()
            .map(|record| record.session_id.as_str())
            .collect();
        sessions.sort_unstable();
        assert_eq!(sessions, ["agent-a", "agent-b"]);
        let agent_a = records
            .iter()
            .find(|record| record.session_id == "agent-a")
            .unwrap();
        assert_eq!(agent_a.usage.input_tokens, 100);
        let agent_b = records
            .iter()
            .find(|record| record.session_id == "agent-b")
            .unwrap();
        assert_eq!(agent_b.usage.input_tokens, 20);
    }

    #[test]
    fn sdk_index_falls_back_to_runs_usage_without_events() {
        let records = records_from_sdk_index(
            &[serde_json::json!({
                "runId": "run-1",
                "agentId": "agent-abc",
                "model": { "id": "grok-4.6" },
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "totalTokens": 120
                },
                "startedAt": 1_750_000_000_000i64
            })],
            &[],
            "fallback",
            "/workspace",
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "agent-abc");
        assert_eq!(records[0].message_id, "run-1");
        assert_eq!(records[0].usage.input_tokens, 100);
    }

    #[test]
    fn cli_store_hex_meta_pins_folder_uuid_and_reads_token_count() {
        let json = r#"{"agentId":"c5f09ff7-69d5-414a-a4c9-b8ac792420fd","lastUsedModel":"composer-2.5","createdAt":1750000000000}"#;
        let hex: String = json.bytes().map(|byte| format!("{byte:02x}")).collect();
        let meta = store_session_meta_from_value(&serde_json::json!({
            "key": "0",
            "value": hex
        }))
        .unwrap();
        assert_eq!(
            meta.agent_id.as_deref(),
            Some("c5f09ff7-69d5-414a-a4c9-b8ac792420fd")
        );
        assert_eq!(meta.model.as_deref(), Some("composer-2.5"));

        let records = records_from_store_payload(
            &serde_json::json!({
                "role": "assistant",
                "agentId": "agent-should-not-win",
                "tokenCount": {
                    "inputTokens": 40,
                    "outputTokens": 8
                }
            }),
            "c5f09ff7-69d5-414a-a4c9-b8ac792420fd",
            "eea42053be10a3da86aa61bbf93e53bb",
            Some(&meta),
            true,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].session_id,
            "c5f09ff7-69d5-414a-a4c9-b8ac792420fd"
        );
        assert_eq!(records[0].model, "composer-2.5");
        assert_eq!(records[0].usage.input_tokens, 40);
        assert_eq!(records[0].usage.output_tokens, 8);
        assert_eq!(records[0].project_path, "eea42053be10a3da86aa61bbf93e53bb");
    }

    #[test]
    fn hex_decode_rejects_odd_length_and_accepts_json() {
        assert!(parse_json_or_hex("7b2261223a317d").is_some());
        assert!(parse_json_or_hex("abc").is_none());
        assert_eq!(parse_json_or_hex(r#"{"a":1}"#).unwrap()["a"], 1);
    }
}

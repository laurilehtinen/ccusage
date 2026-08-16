use serde_json::{Value, json};

use crate::{UsageSummary, cli::AgentReportKind};

pub fn agent_summary_json(
    row: &UsageSummary,
    kind: AgentReportKind,
    include_session_metadata: bool,
) -> Value {
    let mut value = json!({
        period_key(kind): summary_period(row),
        "inputTokens": row.input_tokens,
        "outputTokens": row.output_tokens,
        "cacheCreationTokens": row.cache_creation_tokens,
        "cacheReadTokens": row.cache_read_tokens,
        "totalTokens": row.total_tokens(),
        "totalCost": row.total_cost,
        "modelsUsed": row.models_used,
        "modelBreakdowns": row.model_breakdowns,
    });
    if let (Some(obj), Some(credits)) = (value.as_object_mut(), row.credits) {
        obj.insert("credits".to_string(), json!(credits));
    }
    if let (Some(obj), Some(message_count)) = (value.as_object_mut(), row.message_count) {
        obj.insert("messageCount".to_string(), json!(message_count));
    }
    if include_session_metadata && let Some(obj) = value.as_object_mut() {
        obj.insert(
            "lastActivity".to_string(),
            row.last_activity
                .as_ref()
                .map_or(Value::Null, |value| json!(value)),
        );
        obj.insert(
            "firstActivity".to_string(),
            row.first_activity
                .as_ref()
                .map_or(Value::Null, |value| json!(value)),
        );
        obj.insert(
            "projectPath".to_string(),
            row.project_path
                .as_ref()
                .map_or(Value::Null, |value| json!(value)),
        );
    }
    value
}

pub fn first_column(kind: AgentReportKind) -> &'static str {
    match kind {
        AgentReportKind::Daily => "Date",
        AgentReportKind::Weekly => "Week",
        AgentReportKind::Monthly => "Month",
        AgentReportKind::Session => "Session",
    }
}

pub fn summary_period(row: &UsageSummary) -> &str {
    row.date
        .as_deref()
        .or(row.week.as_deref())
        .or(row.month.as_deref())
        .or(row.session_id.as_deref())
        .unwrap_or_default()
}

/// Sort key for a report row.
///
/// Session reports order by last activity so the newest session is last under
/// the default ascending `--order`. Other reports keep their period key.
pub(crate) fn report_sort_key(row: &UsageSummary, kind: AgentReportKind) -> &str {
    if kind == AgentReportKind::Session
        && let Some(last_activity) = row.last_activity.as_deref()
        && !last_activity.is_empty()
    {
        return last_activity;
    }
    summary_period(row)
}

pub fn sort_report_rows(
    rows: &mut [UsageSummary],
    kind: AgentReportKind,
    order: &crate::cli::SortOrder,
) {
    crate::sort_summaries(rows, order, |row| report_sort_key(row, kind));
}

fn period_key(kind: AgentReportKind) -> &'static str {
    match kind {
        AgentReportKind::Daily => "date",
        AgentReportKind::Weekly => "week",
        AgentReportKind::Monthly => "month",
        AgentReportKind::Session => "sessionId",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UsageSummary;

    fn session_row(session_id: &str, last_activity: Option<&str>) -> UsageSummary {
        UsageSummary {
            date: None,
            month: None,
            week: None,
            session_id: Some(session_id.to_string()),
            project_path: None,
            last_activity: last_activity.map(str::to_string),
            first_activity: None,
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            extra_total_tokens: 0,
            total_cost: 0.0,
            credits: None,
            message_count: None,
            models_used: Vec::new(),
            model_breakdowns: Vec::new(),
            project: None,
            versions: None,
        }
    }

    #[test]
    fn session_sort_key_uses_last_activity_instead_of_session_id() {
        let row = session_row("zzz-session", Some("2026-01-02T12:34:56.000Z"));

        assert_eq!(
            report_sort_key(&row, AgentReportKind::Session),
            "2026-01-02T12:34:56.000Z"
        );
        assert_eq!(summary_period(&row), "zzz-session");
    }

    #[test]
    fn session_sort_key_falls_back_to_session_id_without_last_activity() {
        let row = session_row("session-a", None);

        assert_eq!(report_sort_key(&row, AgentReportKind::Session), "session-a");
    }

    #[test]
    fn daily_sort_key_keeps_the_date_period() {
        let mut row = session_row("session-a", Some("2026-01-02T12:34:56.000Z"));
        row.date = Some("2026-01-01".to_string());

        assert_eq!(report_sort_key(&row, AgentReportKind::Daily), "2026-01-01");
    }
}

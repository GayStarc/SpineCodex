use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::num_format::format_si_suffix;
use codex_protocol::protocol::RolloutItem;
use spine_core::SpineProjection;
use spine_core::StatusSignal;

use super::pressure;

pub(crate) fn prompt_overlay(
    projection: &SpineProjection,
    rollout: &[RolloutItem],
    context_left_tokens: Option<i64>,
) -> ResponseItem {
    let pressures = pressure::project(rollout, projection);
    let sdk_pressures = pressures
        .into_iter()
        .map(|(node_id, pressure)| {
            (
                node_id,
                spine_core::ContextPressure {
                    open_input_tokens: pressure.open_input_tokens,
                    current_input_tokens: pressure.current_input_tokens,
                    context_tokens: pressure.context_tokens,
                    problem: pressure.problem.map(|problem| match problem {
                        pressure::NodeContextPressureProblem::MissingCurrentUsage => {
                            spine_core::ContextPressureProblem::MissingCurrentUsage
                        }
                        pressure::NodeContextPressureProblem::MissingOpenContextBaseline => {
                            spine_core::ContextPressureProblem::MissingOpenContextBaseline
                        }
                        pressure::NodeContextPressureProblem::CoordinateMismatch => {
                            spine_core::ContextPressureProblem::CoordinateMismatch
                        }
                    }),
                },
            )
        })
        .collect();
    let signal = spine_core::status_signal(projection, &sdk_pressures, context_left_tokens);
    developer_prompt_overlay_item(format_spine_status_prompt_overlay(&signal))
}

fn developer_prompt_overlay_item(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn format_optional_summary_attribute(summary: Option<&str>) -> String {
    match summary.map(str::trim).filter(|summary| !summary.is_empty()) {
        Some(summary) => escape_xml_attribute(summary),
        None => "none".to_string(),
    }
}

fn format_spine_status_prompt_overlay(signal: &StatusSignal) -> String {
    let cursor_node_context = signal
        .cursor_node_context_tokens
        .map(format_si_suffix)
        .unwrap_or_else(|| "unavailable".to_string());
    let context_left = signal
        .context_left_tokens
        .map(format_si_suffix)
        .unwrap_or_else(|| "unavailable".to_string());
    let summary = format_optional_summary_attribute(signal.node_summary.as_deref());
    let parent_summary = format_optional_summary_attribute(signal.parent_summary.as_deref());
    format!(
        r#"<spine_status cursor="{}" summary="{}" parent="{}" parent_summary="{}" cursor_context="{}" context_left="{}""#,
        signal.cursor,
        summary,
        signal
            .parent
            .as_ref()
            .map(ToString::to_string)
            .as_deref()
            .unwrap_or("none"),
        parent_summary,
        cursor_node_context,
        context_left,
    ) + " />"
}

fn escape_xml_attribute(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

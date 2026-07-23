use unicode_segmentation::UnicodeSegmentation;

const MAX_STATUS_LINE_SPINE_SUMMARY_GRAPHEMES: usize = 64;

pub(super) fn status_line_spine_node(
    snapshot: &codex_app_server_protocol::SpineTreeUpdatedNotification,
) -> Option<String> {
    let active_node_id = snapshot.active_node_id.trim();
    if active_node_id.is_empty() {
        return None;
    }
    let node = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == active_node_id)?;
    let summary = node
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|summary| !summary.is_empty());
    Some(match summary {
        Some(summary) => format!("{} {}", node.node_id, truncate_summary(summary)),
        None => node.node_id.clone(),
    })
}

fn truncate_summary(summary: &str) -> String {
    if summary.graphemes(true).count() <= MAX_STATUS_LINE_SPINE_SUMMARY_GRAPHEMES {
        return summary.to_string();
    }
    format!(
        "{}...",
        summary
            .graphemes(true)
            .take(MAX_STATUS_LINE_SPINE_SUMMARY_GRAPHEMES)
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::SpineTreeNode;
    use codex_app_server_protocol::SpineTreeNodeKind;
    use codex_app_server_protocol::SpineTreeNodeStatus;
    use codex_app_server_protocol::SpineTreeUpdatedNotification;
    use pretty_assertions::assert_eq;

    #[test]
    fn status_line_spine_node_uses_active_node_and_truncates_summary() {
        let long_summary = "a".repeat(MAX_STATUS_LINE_SPINE_SUMMARY_GRAPHEMES + 1);
        let snapshot = SpineTreeUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            snapshot_seq: 1,
            active_node_id: "1.2".to_string(),
            nodes: vec![SpineTreeNode {
                node_id: "1.2".to_string(),
                parent_id: Some("1".to_string()),
                kind: SpineTreeNodeKind::Task,
                status: SpineTreeNodeStatus::Live,
                summary: Some(long_summary),
                memory_summary: None,
                start: 0,
                end: None,
                context_pressure: None,
            }],
        };

        assert_eq!(
            status_line_spine_node(&snapshot),
            Some(format!(
                "1.2 {}...",
                "a".repeat(MAX_STATUS_LINE_SPINE_SUMMARY_GRAPHEMES)
            ))
        );
    }
}

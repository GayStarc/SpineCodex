use super::*;
use codex_app_server_protocol::SpineTreeUpdatedNotification;

impl ChatWidget {
    pub(super) fn on_spine_tree_update(&mut self, notification: SpineTreeUpdatedNotification) {
        let should_display = self
            .last_spine_tree_snapshot
            .as_ref()
            .is_some_and(|previous| {
                previous.thread_id == notification.thread_id
                    && spine_tree_structure_changed(previous, &notification)
            });
        self.last_spine_tree_snapshot = Some(notification.clone());
        self.refresh_status_surfaces();
        if notification.turn_id.is_empty() || !should_display {
            return;
        }
        self.app_event_tx.send(AppEvent::UpsertSpineTreeCell {
            turn_id: notification.turn_id.clone(),
            snapshot: notification,
        });
    }
}

fn spine_tree_structure_changed(
    previous: &SpineTreeUpdatedNotification,
    current: &SpineTreeUpdatedNotification,
) -> bool {
    previous.active_node_id != current.active_node_id
        || previous.nodes.len() != current.nodes.len()
        || previous
            .nodes
            .iter()
            .zip(&current.nodes)
            .any(|(left, right)| {
                left.node_id != right.node_id
                    || left.parent_id != right.parent_id
                    || left.kind != right.kind
                    || left.status != right.status
                    || left.summary != right.summary
                    || left.memory_summary != right.memory_summary
                    || left.start != right.start
                    || left.end != right.end
            })
}

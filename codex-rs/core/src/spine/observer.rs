use super::host::CodexSpineFrontier;
use super::host::CodexSpineHost;
use super::memory_projection::SpinetreeMemoryProjectionEntry;
use super::memory_projection::SpinetreeUserMessageProjectionEntry;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SpineTreeNodeSnapshot;
use codex_protocol::protocol::SpineTreeUpdateEvent;
use codex_protocol::spine_tree::SpineNodeContextPressureProblem;
use codex_protocol::spine_tree::SpineNodeContextPressureSnapshot;
use codex_protocol::spine_tree::SpineTreeNodeKind;
use codex_protocol::spine_tree::SpineTreeNodeStatus;
use spine_core::ContextPressureProblem;
use spine_core::NodeKind;
use spine_core::NodeStatus;
use spine_core::RuntimeProjection;
use spine_core::SpineObserverCause;
use spine_core::SpineObserverEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexSpineMemoryProjection {
    pub(crate) entries: Vec<SpinetreeMemoryProjectionEntry>,
    pub(crate) user_messages: Vec<SpinetreeUserMessageProjectionEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CodexSpineObserverEffect {
    pub(crate) tree_update: Option<SpineTreeUpdateEvent>,
    pub(crate) memory_projection: Option<CodexSpineMemoryProjection>,
}

impl CodexSpineObserverEffect {
    pub(super) fn from_event(
        host: CodexSpineHost,
        memory_projection_enabled: bool,
        event: SpineObserverEvent<'_, CodexSpineFrontier>,
    ) -> Self {
        let tree_update = host
            .jit_enabled
            .then(|| tree_update(event.runtime_projection));
        let memory_projection = (host.jit_enabled
            && memory_projection_enabled
            && should_publish_memory(event.cause, event.frontier))
        .then(|| CodexSpineMemoryProjection {
            entries: super::closed_memory_projection_entries(event.runtime_projection.spine()),
            user_messages: host.user_message_projection_entries(event.frontier),
        });
        Self {
            tree_update,
            memory_projection,
        }
    }

    pub(super) fn merge(&mut self, newer: Self) {
        if newer.tree_update.is_some() {
            self.tree_update = newer.tree_update;
        }
        if newer.memory_projection.is_some() {
            self.memory_projection = newer.memory_projection;
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tree_update.is_none() && self.memory_projection.is_none()
    }
}

fn should_publish_memory(cause: SpineObserverCause, frontier: &CodexSpineFrontier) -> bool {
    match cause {
        SpineObserverCause::Replay => true,
        SpineObserverCause::Live => !matches!(
            frontier.last_item(),
            Some(RolloutItem::EventMsg(EventMsg::TokenCount(_)))
        ),
    }
}

pub(crate) fn tree_update(projection: &RuntimeProjection) -> SpineTreeUpdateEvent {
    let settled_spawn_call_ids = projection.spine().settled_spawn_call_ids.clone();
    let snapshot = spine_core::tree_snapshot(projection.spine(), projection.usage_samples());
    SpineTreeUpdateEvent {
        snapshot_seq: snapshot.last_boundary.map_or(0, |boundary| boundary.0),
        active_node_id: snapshot.cursor.to_string(),
        nodes: snapshot
            .nodes
            .into_iter()
            .map(|node| SpineTreeNodeSnapshot {
                node_id: node.id.to_string(),
                parent_id: node.parent.map(|id| id.to_string()),
                kind: match node.kind {
                    NodeKind::RootEpoch => SpineTreeNodeKind::RootEpoch,
                    NodeKind::Task => SpineTreeNodeKind::Task,
                },
                status: match node.status {
                    NodeStatus::Live => SpineTreeNodeStatus::Live,
                    NodeStatus::Opened => SpineTreeNodeStatus::Opened,
                    NodeStatus::Closed => SpineTreeNodeStatus::Closed,
                    NodeStatus::Compacted => SpineTreeNodeStatus::Compacted,
                },
                summary: node.summary,
                memory_summary: node.memory_summary,
                spawn_outcome: node.spawn_outcome.map(|outcome| match outcome {
                    spine_core::SpawnOutcome::Completed => {
                        codex_protocol::spine_tree::SpineSpawnOutcome::Completed
                    }
                    spine_core::SpawnOutcome::Errored => {
                        codex_protocol::spine_tree::SpineSpawnOutcome::Errored
                    }
                    spine_core::SpawnOutcome::Aborted => {
                        codex_protocol::spine_tree::SpineSpawnOutcome::Aborted
                    }
                }),
                start: node.start.0,
                end: node.end.map(|boundary| boundary.0),
                context_pressure: node
                    .pressure
                    .map(|pressure| SpineNodeContextPressureSnapshot {
                        open_input_tokens: pressure.open_input_tokens,
                        current_input_tokens: pressure.current_input_tokens,
                        context_tokens: pressure.context_tokens,
                        problem: pressure.problem.map(|problem| match problem {
                            ContextPressureProblem::MissingCurrentUsage => {
                                SpineNodeContextPressureProblem::MissingCurrentUsage
                            }
                            ContextPressureProblem::MissingOpenContextBaseline => {
                                SpineNodeContextPressureProblem::MissingOpenContextBaseline
                            }
                            ContextPressureProblem::CoordinateMismatch => {
                                SpineNodeContextPressureProblem::CoordinateMismatch
                            }
                        }),
                    }),
            })
            .collect(),
        settled_spawn_call_ids,
    }
}

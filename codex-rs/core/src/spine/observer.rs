use super::context_handler::CodexContextHandler;
use super::memory_projection::SpinetreeMemoryProjection;
use super::memory_projection::SpinetreeMemoryProjectionEntry;
use super::memory_projection::SpinetreeUserMessageProjectionEntry;
use async_channel::Sender;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SpineTreeNodeSnapshot;
use codex_protocol::protocol::SpineTreeUpdateEvent;
use codex_protocol::spine_tree::SpineNodeContextPressureProblem;
use codex_protocol::spine_tree::SpineNodeContextPressureSnapshot;
use codex_protocol::spine_tree::SpineTreeNodeKind;
use codex_protocol::spine_tree::SpineTreeNodeStatus;
use spine_core::ContextPressureProblem;
use spine_core::NodeKind;
use spine_core::NodeStatus;
use spine_core::SpineContextProjection;
use spine_core::SpineObserverEffect;
use spine_core::SpineObserverEffectHandler;
use spine_core::SpineObserverEffectKind;
use tokio::sync::watch;
use tracing::warn;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexSpineMemoryProjection {
    entries: Vec<SpinetreeMemoryProjectionEntry>,
    user_messages: Vec<SpinetreeUserMessageProjectionEntry>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexSpineObserverHandler {
    tx_event: Option<Sender<Event>>,
    fallback_event_id: String,
    memory_projection_tx: Option<watch::Sender<Option<CodexSpineMemoryProjection>>>,
    jit_enabled: bool,
}

impl CodexSpineObserverHandler {
    pub(crate) fn new(
        tx_event: Sender<Event>,
        fallback_event_id: String,
        memory_projection: Option<SpinetreeMemoryProjection>,
        jit_enabled: bool,
    ) -> Self {
        Self {
            tx_event: Some(tx_event),
            fallback_event_id,
            memory_projection_tx: memory_projection.map(start_memory_projection_worker),
            jit_enabled,
        }
    }
}

impl SpineObserverEffectHandler<CodexContextHandler> for CodexSpineObserverHandler {
    fn handle(&mut self, effect: SpineObserverEffect<'_>, context_handler: &CodexContextHandler) {
        if !self.jit_enabled {
            return;
        }
        if let Some(tx_event) = &self.tx_event {
            let event = Event {
                id: context_handler
                    .latest_turn_id()
                    .unwrap_or(&self.fallback_event_id)
                    .to_string(),
                msg: EventMsg::SpineTreeUpdate(context_tree_update(effect.projection())),
            };
            if let Err(err) = tx_event.try_send(event) {
                warn!("failed to publish Spine tree update: {err}");
            }
        }
        if effect.kind() != SpineObserverEffectKind::ContextCommitted {
            return;
        }
        let Some(memory_projection_tx) = &self.memory_projection_tx else {
            return;
        };
        memory_projection_tx.send_replace(Some(CodexSpineMemoryProjection {
            entries: super::closed_memory_projection_entries(effect.projection().spine()),
            user_messages: context_handler
                .user_message_projection_entries(effect.projection().stack()),
        }));
    }
}

fn start_memory_projection_worker(
    projection: SpinetreeMemoryProjection,
) -> watch::Sender<Option<CodexSpineMemoryProjection>> {
    let (tx, mut rx) = watch::channel::<Option<CodexSpineMemoryProjection>>(None);
    let _worker = tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let Some(memory) = rx.borrow_and_update().clone() else {
                continue;
            };
            let projection = projection.clone();
            match tokio::task::spawn_blocking(move || {
                projection.persist(&memory.entries, &memory.user_messages)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!("failed to publish Spine memory projection: {err:#}"),
                Err(err) => warn!("Spine memory projection task failed: {err}"),
            }
        }
    });
    tx
}

pub(crate) fn context_tree_update(projection: &SpineContextProjection) -> SpineTreeUpdateEvent {
    tree_update_from_parts(projection.spine(), projection.usage_samples())
}

fn tree_update_from_parts(
    projection: &spine_core::SpineProjection,
    usage_samples: &[spine_core::TokenUsageSample],
) -> SpineTreeUpdateEvent {
    let settled_spawn_call_ids = projection.settled_spawn_call_ids.clone();
    let snapshot = spine_core::tree_snapshot(projection, usage_samples);
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

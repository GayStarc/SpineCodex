use super::context_handler::CodexContextHandler;
use super::context_handler::response_item_to_char;
use super::observer::CodexSpineObserverHandler;
use crate::context_manager::ContextManager;
use crate::session::session::SessionConfiguration;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TokenCountEvent;
use spine_core::RawBoundary;
use spine_core::SpineContextRuntime;
use spine_core::SpineRecoveryInput;
use spine_core::SpineSignal;
use spine_core::ToolUse;
use std::collections::HashMap;

pub(crate) struct SessionSpineRuntime {
    pub(crate) runtime: SpineContextRuntime<CodexContextHandler, CodexSpineObserverHandler>,
    next_boundary: u64,
    pending_calls: HashMap<String, ToolUse>,
    trim_enabled: bool,
    jit_enabled: bool,
}

impl SessionSpineRuntime {
    pub(crate) fn new(
        configuration: &SessionConfiguration,
        observer: CodexSpineObserverHandler,
    ) -> Option<Self> {
        let enabled = configuration.spine_jit_enabled() || configuration.spine_trim_enabled();
        enabled.then(|| {
            let config = configuration.spine_sdk_config();
            let handler = CodexContextHandler::new(&config);
            Self {
                runtime: SpineContextRuntime::new_with_observer(config, handler, observer)
                    .expect("validated session Spine configuration must initialize"),
                next_boundary: 0,
                pending_calls: HashMap::new(),
                trim_enabled: configuration.spine_trim_enabled(),
                jit_enabled: configuration.spine_jit_enabled(),
            }
        })
    }

    pub(crate) fn append_response_items(
        &mut self,
        items: &[ResponseItem],
        history: &mut ContextManager,
    ) {
        let mut sources = Vec::with_capacity(items.len());
        let chars = items
            .iter()
            .map(|item| {
                let boundary = RawBoundary(self.next_boundary);
                self.next_boundary = self.next_boundary.saturating_add(1);
                sources.push((boundary, item.clone()));
                response_item_to_char(
                    item,
                    boundary,
                    &mut self.pending_calls,
                    self.runtime.handler().spawn_enabled(),
                )
            })
            .collect::<Vec<_>>();
        self.runtime.handler_mut().stage_sources(sources);
        self.runtime
            .append(chars, history)
            .expect("native context append must produce a valid Spine projection");
    }

    pub(crate) fn observe_token_count(&mut self, event: TokenCountEvent) {
        if let Some(usage) = event.info.map(|info| info.last_token_usage) {
            self.runtime.observe_usage(spine_core::TokenUsageSample {
                boundary: RawBoundary(self.next_boundary),
                input_tokens: usage.input_tokens,
            });
        }
    }

    pub(crate) fn compact_live(&mut self, history: &mut ContextManager) {
        let compact_boundary = RawBoundary(self.next_boundary);
        self.next_boundary = self.next_boundary.saturating_add(1);
        self.runtime.handler_mut().reset_sources();
        self.pending_calls.clear();
        let mut sources = Vec::with_capacity(history.raw_items().len());
        let chars = history
            .raw_items()
            .iter()
            .map(|item| {
                let boundary = RawBoundary(self.next_boundary);
                self.next_boundary = self.next_boundary.saturating_add(1);
                sources.push((boundary, item.clone()));
                spine_core::SpineChar::Opaque { boundary }
            })
            .collect::<Vec<_>>();
        self.runtime.handler_mut().stage_sources(sources);
        self.runtime
            .compact_live(compact_boundary, chars, history)
            .expect("compacted context must produce a valid Spine projection");
    }

    pub(crate) fn replay(&mut self, rollout_items: &[RolloutItem], history: &mut ContextManager) {
        let effective = super::effective_rollout(rollout_items);
        let last_compact = effective
            .iter()
            .rposition(|(_, item)| matches!(item, RolloutItem::Compacted(_)));
        self.pending_calls.clear();
        let mut archived = Vec::new();
        for (index, (ordinal, item)) in effective.iter().copied().enumerate() {
            let archived_context = last_compact.is_some_and(|last| index <= last);
            match item {
                RolloutItem::ResponseItem(item) if archived_context => {
                    archived.push(SpineRecoveryInput::Char(response_item_to_char(
                        item,
                        RawBoundary(ordinal as u64),
                        &mut self.pending_calls,
                        self.runtime.handler().spawn_enabled(),
                    )))
                }
                RolloutItem::InterAgentCommunication(communication) if archived_context => {
                    let item = communication.to_model_input_item();
                    archived.push(SpineRecoveryInput::Char(response_item_to_char(
                        &item,
                        RawBoundary(ordinal as u64),
                        &mut self.pending_calls,
                        self.runtime.handler().spawn_enabled(),
                    )))
                }
                RolloutItem::Compacted(_) if archived_context => {
                    archived.push(SpineRecoveryInput::Signal(SpineSignal::Compact {
                        boundary: RawBoundary(ordinal as u64),
                    }))
                }
                RolloutItem::EventMsg(EventMsg::TokenCount(event)) => {
                    if let Some(usage) = event.info.as_ref().map(|info| &info.last_token_usage) {
                        archived.push(SpineRecoveryInput::Signal(SpineSignal::Usage(
                            spine_core::TokenUsageSample {
                                boundary: RawBoundary(ordinal as u64),
                                input_tokens: usage.input_tokens,
                            },
                        )));
                    }
                }
                _ => {}
            }
        }
        self.runtime.handler_mut().reset_sources();
        self.pending_calls.clear();
        let compact_boundary = last_compact.map(|index| effective[index].0 as u64);
        let postcompact_source_count = last_compact.map_or(0, |index| {
            effective
                .iter()
                .skip(index + 1)
                .filter(|(_, item)| {
                    matches!(
                        item,
                        RolloutItem::ResponseItem(_) | RolloutItem::InterAgentCommunication(_)
                    )
                })
                .count()
        });
        let replacement_len = last_compact
            .and_then(|index| match effective[index].1 {
                RolloutItem::Compacted(compacted) => compacted
                    .replacement_history
                    .as_ref()
                    .map(|items| {
                        if history.raw_items().starts_with(items) {
                            items.len()
                        } else {
                            Default::default()
                        }
                    })
                    .or_else(|| {
                        Some(
                            history
                                .raw_items()
                                .len()
                                .saturating_sub(postcompact_source_count),
                        )
                    }),
                _ => None,
            })
            .unwrap_or_default();
        self.next_boundary = compact_boundary.map_or(0, |boundary| boundary + 1);
        let mut sources = Vec::with_capacity(history.raw_items().len());
        let chars = history
            .raw_items()
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let boundary = RawBoundary(self.next_boundary);
                self.next_boundary = self.next_boundary.saturating_add(1);
                sources.push((boundary, item.clone()));
                if compact_boundary.is_some_and(|_| index < replacement_len) {
                    spine_core::SpineChar::Opaque { boundary }
                } else {
                    response_item_to_char(
                        item,
                        boundary,
                        &mut self.pending_calls,
                        self.runtime.handler().spawn_enabled(),
                    )
                }
            })
            .collect::<Vec<_>>();
        self.runtime.handler_mut().stage_sources(sources);
        self.runtime
            .recover(archived, chars, history)
            .expect("native rollout history must recover deterministically");
    }

    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str, _history_version: u64) {
        self.runtime
            .handler_mut()
            .replace_last_turn_images(placeholder);
    }

    #[cfg(test)]
    pub(crate) fn tree_update(&self) -> Option<codex_protocol::protocol::SpineTreeUpdateEvent> {
        self.jit_enabled
            .then(|| super::observer::context_tree_update(self.runtime.projection()))
    }

    pub(crate) fn status_prompt_overlay(
        &self,
        context_left_tokens: Option<i64>,
    ) -> Option<ResponseItem> {
        self.jit_enabled.then(|| {
            super::status::prompt_overlay(
                self.runtime.projection().spine(),
                self.runtime.projection().usage_samples(),
                context_left_tokens,
            )
        })?
    }

    pub(crate) fn validate_control(&self, tool: spine_core::SpineTool) -> Result<(), String> {
        if matches!(
            tool,
            spine_core::SpineTool::Close | spine_core::SpineTool::Next
        ) {
            let projection = self.runtime.projection().spine();
            let cursor = projection
                .nodes
                .iter()
                .find(|node| node.id == projection.cursor)
                .ok_or_else(|| "Spine cursor is missing from the derived tree".to_string())?;
            if cursor.kind == spine_core::NodeKind::RootEpoch {
                return Err("no open Spine node is available to close".to_string());
            }
        }
        Ok(())
    }

    pub(crate) fn validate_trim(
        &self,
        current_call_id: &str,
        request: &spine_core::TrimRequest,
    ) -> Result<(), String> {
        if !self.trim_enabled {
            return Err("Spine trim is not enabled for this session".to_string());
        }
        if self
            .pending_calls
            .get(current_call_id)
            .is_none_or(|call| call.name != "spine.trim")
        {
            return Err(
                "spine.trim failed: current toolcall is unavailable; do not retry".to_string(),
            );
        }
        self.runtime
            .projection()
            .trim_projection()
            .ok_or_else(|| "Spine trim runtime is unavailable".to_string())?
            .validate(request)
    }
}

use super::context_handler::CodexContextHandler;
use super::context_handler::response_item_to_char;
use super::context_handler::response_item_to_char_and_source;
use super::coordinator::ReplayMode;
use super::coordinator::SharedSpineCoordinator;
use super::coordinator::replay_mode;
use super::legacy_rollout::expand_response_item;
use super::legacy_rollout::expand_response_items;
use super::observer::CodexSpineObserverHandler;
use crate::context_manager::ContextManager;
use crate::session::session::SessionConfiguration;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TokenCountEvent;
use spine_core::RawBoundary;
use spine_core::SpineConfig;
use spine_core::SpineContextRuntime;
use spine_core::SpineRecoveryInput;
use spine_core::SpineSignal;
use spine_core::ToolUse;
use std::borrow::Cow;
use std::collections::HashMap;

pub(crate) struct SessionSpineRuntime {
    legacy_runtime: Option<SpineContextRuntime<CodexContextHandler, CodexSpineObserverHandler>>,
    next_boundary: u64,
    pending_calls: HashMap<String, ToolUse>,
    trim_enabled: bool,
    live_updates_enabled: bool,
    coordinator: SharedSpineCoordinator,
    legacy_config: SpineConfig,
    legacy_observer: CodexSpineObserverHandler,
}

impl SessionSpineRuntime {
    pub(crate) fn new(
        configuration: &SessionConfiguration,
        observer: CodexSpineObserverHandler,
        coordinator: SharedSpineCoordinator,
    ) -> Option<Self> {
        let enabled = configuration.spine_jit_enabled() || configuration.spine_trim_enabled();
        enabled.then(|| {
            let config = configuration.spine_sdk_config();
            let canonical_enabled = coordinator
                .lock()
                .unwrap_or_else(|_| panic!("Spine coordinator mutex must not be poisoned"))
                .is_some();
            let legacy_runtime = (!canonical_enabled).then(|| {
                let handler = CodexContextHandler::new(&config);
                SpineContextRuntime::new_with_observer(config.clone(), handler, observer.clone())
                    .expect("validated session Spine configuration must initialize")
            });
            Self {
                legacy_runtime,
                next_boundary: 0,
                pending_calls: HashMap::new(),
                trim_enabled: configuration.spine_trim_enabled(),
                live_updates_enabled: configuration.spine_trim_enabled()
                    && !configuration.spine_jit_enabled(),
                coordinator,
                legacy_config: config,
                legacy_observer: observer,
            }
        })
    }

    fn with_coordinator<R>(
        &self,
        f: impl FnOnce(&mut super::coordinator::CodexSpineCoordinator) -> R,
    ) -> Option<R> {
        self.coordinator
            .lock()
            .unwrap_or_else(|_| panic!("Spine coordinator mutex must not be poisoned"))
            .as_mut()
            .map(f)
    }

    fn legacy_enabled(&self) -> bool {
        self.legacy_runtime.is_some()
    }

    fn activate_legacy_runtime(&mut self) {
        if self.legacy_runtime.is_none() {
            let handler = CodexContextHandler::new(&self.legacy_config);
            self.legacy_runtime = Some(
                SpineContextRuntime::new_with_observer(
                    self.legacy_config.clone(),
                    handler,
                    self.legacy_observer.clone(),
                )
                .expect("validated session Spine configuration must initialize"),
            );
        }
        *self
            .coordinator
            .lock()
            .unwrap_or_else(|_| panic!("Spine coordinator mutex must not be poisoned")) = None;
    }

    pub(crate) fn append_response_items(
        &mut self,
        items: &[ResponseItem],
        history: &mut ContextManager,
    ) {
        if let Some(result) = self.with_coordinator(|coordinator| {
            coordinator.observe_response_items(items).map(|context| {
                super::replace_context_if_changed(history, context.items);
            })
        }) {
            if let Err(error) = result {
                tracing::warn!(%error, "failed to observe Spine source");
                self.with_coordinator(|coordinator| {
                    coordinator.latch_durability_fault(error.to_string());
                });
            }
            return;
        }
        if !self.legacy_enabled() {
            return;
        }
        let original_len = items.len();
        let items = if self.live_updates_enabled {
            items.to_vec()
        } else {
            expand_response_items(items)
        };
        if items.len() != original_len {
            let prefix_len = history.raw_items().len().saturating_sub(original_len);
            let mut rewritten = history.raw_items()[..prefix_len].to_vec();
            rewritten.extend(items.iter().cloned());
            history.replace(rewritten);
        }
        let runtime = self
            .legacy_runtime
            .as_mut()
            .expect("legacy Spine runtime was checked before append");
        let mut sources = Vec::with_capacity(items.len());
        let chars = items
            .iter()
            .map(|item| {
                let boundary = RawBoundary(self.next_boundary);
                self.next_boundary = self.next_boundary.saturating_add(1);
                let (character, source) = response_item_to_char_and_source(
                    item,
                    boundary,
                    &mut self.pending_calls,
                    runtime.handler().spawn_enabled(),
                );
                sources.push((boundary, source));
                character
            })
            .collect::<Vec<_>>();
        runtime.handler_mut().stage_sources(sources);
        runtime
            .append(chars, history)
            .expect("native context append must produce a valid Spine projection");
    }

    pub(crate) fn observe_token_count(&mut self, event: TokenCountEvent) {
        if self
            .with_coordinator(|coordinator| coordinator.observe_token_count(&event))
            .is_some()
        {
            return;
        }
        if !self.legacy_enabled() {
            return;
        }
        if let Some(usage) = event.info.map(|info| info.last_token_usage) {
            self.legacy_runtime
                .as_mut()
                .expect("legacy Spine runtime was checked before usage observation")
                .observe_usage(spine_core::TokenUsageSample {
                    boundary: RawBoundary(self.next_boundary),
                    input_tokens: usage.input_tokens,
                });
        }
    }

    pub(crate) fn current_input_tokens(&self) -> Option<i64> {
        self.with_coordinator(|coordinator| coordinator.current_input_tokens())
            .flatten()
    }

    pub(crate) fn compact_live(&mut self, history: &mut ContextManager) {
        if let Some(result) =
            self.with_coordinator(|coordinator| coordinator.compact_live(history.raw_items()))
        {
            match result {
                Ok(()) => {}
                Err(error) => {
                    tracing::error!(%error, "failed canonical Spine compact");
                    self.with_coordinator(|coordinator| {
                        coordinator.latch_durability_fault(error.to_string());
                    });
                }
            }
            return;
        }
        if !self.legacy_enabled() {
            return;
        }
        let compact_boundary = RawBoundary(self.next_boundary);
        self.next_boundary = self.next_boundary.saturating_add(1);
        let runtime = self
            .legacy_runtime
            .as_mut()
            .expect("legacy Spine runtime was checked before compact");
        runtime.handler_mut().reset_sources();
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
        runtime.handler_mut().stage_sources(sources);
        runtime
            .compact_live(compact_boundary, chars, history)
            .expect("compacted context must produce a valid Spine projection");
    }

    pub(crate) fn replay(&mut self, rollout_items: &[RolloutItem], history: &mut ContextManager) {
        let effective = super::effective_rollout(rollout_items);
        match replay_mode(&effective) {
            Ok(ReplayMode::Legacy) => {
                if self.with_coordinator(|_| ()).is_some()
                    && !contains_legacy_spine_transition(&effective)
                {
                    let expanded_history = expand_response_items(history.raw_items());
                    if expanded_history.len() != history.raw_items().len() {
                        history.replace(expanded_history);
                    }
                    let result = self.with_coordinator(|coordinator| {
                        coordinator.observe_response_items(history.raw_items())
                    });
                    match result {
                        Some(Ok(context)) => {
                            super::replace_context_if_changed(history, context.items);
                        }
                        Some(Err(error)) => {
                            let reason = error.to_string();
                            self.with_coordinator(|coordinator| {
                                coordinator.latch_durability_fault(reason.clone());
                            });
                            tracing::error!(%reason, "failed to migrate legacy Spine rollout");
                        }
                        None => {}
                    }
                    return;
                }
                self.activate_legacy_runtime();
            }
            Ok(ReplayMode::Canonical { thread, records }) => {
                self.with_coordinator(|coordinator| {
                    match coordinator.replay_canonical(
                        &effective,
                        history.raw_items(),
                        thread,
                        records,
                    ) {
                        Ok(installed) => {
                            super::replace_context_if_changed(
                                history,
                                installed.context.items.clone(),
                            );
                            coordinator.publish_canonical_sampling(&installed);
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            coordinator.latch_durability_fault(reason.clone());
                            tracing::error!(%reason, "failed to restore Spine rollout");
                        }
                    }
                });
                return;
            }
            Err(error) => {
                let reason = format!("invalid canonical Spine rollout metadata: {error}");
                self.with_coordinator(|coordinator| {
                    coordinator.latch_durability_fault(reason.clone());
                });
                tracing::error!(%reason, "failed to restore Spine rollout");
                return;
            }
        }
        let expanded_history = expand_response_items(history.raw_items());
        if expanded_history.len() != history.raw_items().len() {
            history.replace(expanded_history);
        }
        let last_compact = effective
            .iter()
            .rposition(|(_, item)| matches!(item, RolloutItem::Compacted(_)));
        self.pending_calls.clear();
        let runtime = self
            .legacy_runtime
            .as_mut()
            .expect("legacy Spine runtime must exist for legacy replay");
        let mut archived = Vec::new();
        let mut replay_boundary = 0u64;
        let mut compact_boundary = None;
        for (index, (ordinal, item)) in effective.iter().copied().enumerate() {
            let archived_context = last_compact.is_some_and(|last| index <= last);
            let archived_source = match item {
                RolloutItem::ResponseItem(item) if archived_context => Some(Cow::Borrowed(item)),
                RolloutItem::InterAgentCommunication(communication) if archived_context => {
                    Some(Cow::Owned(communication.to_model_input_item()))
                }
                _ => None,
            };
            if let Some(item) = archived_source {
                for expanded in
                    expand_response_item(&item).unwrap_or_else(|_| vec![item.into_owned()])
                {
                    archived.push(SpineRecoveryInput::Char(response_item_to_char(
                        &expanded,
                        RawBoundary(replay_boundary),
                        &mut self.pending_calls,
                        runtime.handler().spawn_enabled(),
                    )));
                    replay_boundary = replay_boundary.saturating_add(1);
                }
                continue;
            }
            match item {
                RolloutItem::Compacted(_) if archived_context => {
                    compact_boundary = Some(replay_boundary);
                    archived.push(SpineRecoveryInput::Signal(SpineSignal::Compact {
                        boundary: RawBoundary(replay_boundary),
                    }));
                    replay_boundary = replay_boundary.saturating_add(1);
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
        runtime.handler_mut().reset_sources();
        self.pending_calls.clear();
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
        let mut chars = Vec::with_capacity(history.raw_items().len());
        for (index, item) in history.raw_items().iter().enumerate() {
            if compact_boundary.is_some_and(|_| index < replacement_len) {
                let boundary = RawBoundary(self.next_boundary);
                self.next_boundary = self.next_boundary.saturating_add(1);
                sources.push((boundary, item.clone()));
                chars.push(spine_core::SpineChar::Opaque { boundary });
                continue;
            }
            for expanded in expand_response_item(item).unwrap_or_else(|_| vec![item.clone()]) {
                let boundary = RawBoundary(self.next_boundary);
                self.next_boundary = self.next_boundary.saturating_add(1);
                let (character, source) = response_item_to_char_and_source(
                    &expanded,
                    boundary,
                    &mut self.pending_calls,
                    runtime.handler().spawn_enabled(),
                );
                sources.push((boundary, source));
                chars.push(character);
            }
        }
        runtime.handler_mut().stage_sources(sources);
        runtime
            .recover(archived, chars, history)
            .expect("native rollout history must recover deterministically");
    }

    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str, _history_version: u64) {
        if self.with_coordinator(|_| ()).is_some() || !self.legacy_enabled() {
            return;
        }
        self.legacy_runtime
            .as_mut()
            .expect("legacy Spine runtime was checked before image replacement")
            .handler_mut()
            .replace_last_turn_images(placeholder);
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
        self.trim_projection()?.validate(request)
    }

    pub(crate) fn validate_trim_request(
        &self,
        request: &spine_core::TrimRequest,
    ) -> Result<(), String> {
        self.trim_projection()?.validate(request)
    }

    pub(crate) fn validate_control(&self, tool: spine_core::SpineTool) -> Result<(), String> {
        let runtime = self
            .legacy_runtime
            .as_ref()
            .ok_or_else(|| "legacy Spine runtime is unavailable".to_string())?;
        if matches!(
            tool,
            spine_core::SpineTool::Close | spine_core::SpineTool::Next
        ) {
            let projection = runtime.projection().spine();
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

    fn trim_projection(&self) -> Result<&spine_core::TrimProjection, String> {
        if !self.trim_enabled {
            return Err("Spine trim is not enabled for this session".to_string());
        }
        self.legacy_runtime
            .as_ref()
            .ok_or_else(|| "Spine trim runtime is unavailable".to_string())?
            .projection()
            .trim_projection()
            .ok_or_else(|| "Spine trim runtime is unavailable".to_string())
    }

    #[cfg(test)]
    pub(crate) fn legacy_projection(&self) -> Option<&spine_core::SpineContextProjection> {
        self.legacy_runtime
            .as_ref()
            .map(SpineContextRuntime::projection)
    }
}

fn contains_legacy_spine_transition(effective: &[(usize, &RolloutItem)]) -> bool {
    effective.iter().any(|(_, item)| {
        let RolloutItem::ResponseItem(item) = item else {
            return false;
        };
        match item {
            ResponseItem::FunctionCall { namespace, .. }
            | ResponseItem::CustomToolCall { namespace, .. } => {
                namespace.as_deref() == Some(spine_core::SPINE_NAMESPACE)
            }
            ResponseItem::CustomToolCallOutput { .. } => {
                expand_response_item(item).is_ok_and(|expanded| expanded.len() > 1)
            }
            _ => false,
        }
    })
}

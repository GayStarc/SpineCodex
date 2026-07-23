use crate::context_manager::ContextManager;
use crate::session::session::SessionConfiguration;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
#[cfg(test)]
use codex_protocol::protocol::SpineTreeUpdateEvent;
use spine_core::SpineRuntime;

use super::host::CodexSpineEventHandlers;
use super::host::CodexSpineHost;
use super::host::CodexSpineInput;
use super::observer::CodexSpineObserverEffect;

/// Codex's narrow ownership boundary for the SDK runtime and its derived views.
///
/// The session state owns this adapter, while runtime details, source ordinals,
/// projection replacement, and Spine validation remain local to the Spine module.
pub(crate) struct SessionSpineRuntime {
    pub(crate) runtime: SpineRuntime<CodexSpineHost, CodexSpineEventHandlers>,
    next_ordinal: usize,
    trim_enabled: bool,
}

impl SessionSpineRuntime {
    pub(crate) fn new(configuration: &SessionConfiguration) -> Option<Self> {
        let enabled = configuration.spine_jit_enabled() || configuration.spine_trim_enabled();
        enabled.then(|| {
            let host = CodexSpineHost {
                jit_enabled: configuration.spine_jit_enabled(),
                spawn_enabled: configuration.spine_spawn_enabled(),
            };
            let handlers = CodexSpineEventHandlers::new(
                host,
                configuration.spinetree_memory_projection_enabled(),
            );
            let runtime = SpineRuntime::new(configuration.spine_sdk_config(), host, handlers)
                .expect("validated session Spine configuration must initialize");
            Self {
                runtime,
                next_ordinal: 0,
                trim_enabled: configuration.spine_trim_enabled(),
            }
        })
    }

    pub(crate) fn append(&mut self, items: &[RolloutItem], history: &mut ContextManager) {
        for item in items {
            let input = CodexSpineInput {
                ordinal: self.next_ordinal,
                item: item.clone(),
            };
            self.runtime
                .eat(&input, history)
                .expect("native rollout append must produce a valid Spine projection");
            if super::is_spine_source_item(item) {
                self.next_ordinal = self.next_ordinal.saturating_add(1);
            }
        }
    }

    pub(crate) fn replay(&mut self, rollout_items: &[RolloutItem], history: &mut ContextManager) {
        let (inputs, next_ordinal) = super::host::replay_inputs(rollout_items, history);
        self.runtime
            .replay(inputs.iter(), history)
            .expect("native rollout history must replay deterministically");
        self.next_ordinal = next_ordinal;
    }

    pub(crate) fn replace_last_turn_images(
        &mut self,
        placeholder: &str,
        history_version: u64,
    ) -> bool {
        self.runtime
            .handlers_mut()
            .replace_last_turn_images(placeholder, history_version)
    }

    #[cfg(test)]
    pub(crate) fn tree_update(&self) -> Option<SpineTreeUpdateEvent> {
        self.runtime
            .host()
            .jit_enabled
            .then(|| super::observer::tree_update(self.runtime.runtime_projection()))
    }

    pub(crate) fn status_prompt_overlay(
        &self,
        context_left_tokens: Option<i64>,
    ) -> Option<ResponseItem> {
        if !self.runtime.host().jit_enabled {
            return None;
        }
        let projection = self.runtime.runtime_projection();
        super::status::prompt_overlay(
            projection.spine(),
            projection.usage_samples(),
            context_left_tokens,
        )
    }

    pub(crate) fn take_observer_effect(&mut self) -> Option<CodexSpineObserverEffect> {
        self.runtime.handlers_mut().take_observer_effect()
    }

    pub(crate) fn validate_control(&self, tool: spine_core::SpineTool) -> Result<(), String> {
        if matches!(
            tool,
            spine_core::SpineTool::Close | spine_core::SpineTool::Next
        ) {
            let projection = self.runtime.projection();
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
        let frontier = self
            .runtime
            .frontier()
            .ok_or_else(|| "Spine trim runtime is unavailable".to_string())?;
        self.runtime
            .host()
            .validate_trim_request(frontier, current_call_id, request)
    }
}

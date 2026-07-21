use super::effective_rollout;
use super::materialize_context;
use super::materialize_trim_only_context;
use super::project_trim_item;
use super::response_item_at;
use crate::context_manager::ContextManager;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use spine_core::HostStep;
use spine_core::NativeItemRef;
use spine_core::RawBoundary;
use spine_core::RuntimeProjection;
use spine_core::SpineHost;
use spine_core::TokenUsageSample;
use std::fmt;

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexSpineFrontier {
    source: Vec<CodexSpineInput>,
    emitted_events: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexSpineInput {
    pub(crate) ordinal: usize,
    pub(crate) item: RolloutItem,
}

pub(crate) fn selected_inputs(rollout: &[RolloutItem]) -> Vec<CodexSpineInput> {
    effective_rollout(rollout)
        .into_iter()
        .map(|(ordinal, item)| CodexSpineInput {
            ordinal,
            item: item.clone(),
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CodexSpineHost {
    pub(crate) jit_enabled: bool,
    pub(crate) trim_enabled: bool,
    pub(crate) spawn_enabled: bool,
    pub(crate) trim_threshold_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexSpineHostError(String);

impl fmt::Display for CodexSpineHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CodexSpineHostError {}

impl SpineHost for CodexSpineHost {
    type Input = CodexSpineInput;
    type Frontier = CodexSpineFrontier;
    type Error = CodexSpineHostError;

    fn initial_frontier(&self) -> Self::Frontier {
        CodexSpineFrontier::default()
    }

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error> {
        let mut source = frontier.source.clone();
        source.push(input.clone());
        let effective = source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        let events = super::stable_lex_rollout(&effective, self.spawn_enabled);
        if events.events.len() < frontier.emitted_events {
            return Err(CodexSpineHostError(
                "rollout changed before runtime reset; replay the selected prefix".to_string(),
            ));
        }
        let new_events = events.events[frontier.emitted_events..].to_vec();
        let observed_boundary = effective
            .last()
            .map(|(ordinal, _)| RawBoundary(*ordinal as u64));
        let next_frontier = CodexSpineFrontier {
            source,
            emitted_events: events.events.len(),
        };
        let step = HostStep::new(next_frontier, new_events, events.pending, observed_boundary);
        let usage_sample = match &input.item {
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => event
                .info
                .as_ref()
                .map(|info| info.last_token_usage.input_tokens)
                .filter(|input_tokens| *input_tokens > 0)
                .map(|input_tokens| TokenUsageSample {
                    boundary: RawBoundary(input.ordinal as u64),
                    input_tokens,
                }),
            _ => None,
        };
        let step = match usage_sample {
            Some(sample) => step.with_usage_sample(sample),
            None => step,
        };
        Ok(step)
    }
}

impl CodexSpineHost {
    pub(crate) fn project_context(
        &self,
        rollout: &[RolloutItem],
        base: &ContextManager,
        update: &RuntimeProjection,
    ) -> Result<ContextManager, CodexSpineHostError> {
        let effective = effective_rollout(rollout);
        let trim = self.trim_enabled.then(|| {
            let events = super::stable_lex_rollout(&effective, self.spawn_enabled);
            spine_core::TrimProjection::derive_with_threshold(
                &events.events,
                self.trim_threshold_bytes,
            )
        });
        let projected = if self.jit_enabled {
            materialize_context(
                &update.spine().visible_context,
                &effective,
                trim.as_ref(),
                Some(base),
                self.spawn_enabled,
            )
            .map_err(CodexSpineHostError)?
        } else {
            materialize_trim_only_context(&effective, trim.as_ref(), Some(base))
                .map_err(CodexSpineHostError)?
        };
        let mut projected = projected;
        for source in update.pending() {
            let NativeItemRef::Rollout { ordinal } = source else {
                continue;
            };
            let raw = response_item_at(&effective, *ordinal, Some(base)).ok_or_else(|| {
                CodexSpineHostError(format!("missing rollout source {}", ordinal.0))
            })?;
            projected.push(project_trim_item(
                raw,
                usize::try_from(ordinal.0).unwrap_or(usize::MAX),
                trim.as_ref(),
            ));
        }
        Ok(base.clone().with_projected_items(projected))
    }
}

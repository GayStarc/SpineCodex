use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use spine_core::NodeId;
use spine_core::SpineProjection;
use spine_core::TokenUsageSample;
use std::collections::BTreeMap;

use super::effective_rollout;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeContextPressure {
    pub(crate) open_input_tokens: Option<i64>,
    pub(crate) current_input_tokens: Option<i64>,
    pub(crate) context_tokens: Option<i64>,
    pub(crate) problem: Option<NodeContextPressureProblem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeContextPressureProblem {
    MissingCurrentUsage,
    MissingOpenContextBaseline,
    CoordinateMismatch,
}

pub(crate) fn project(
    rollout: &[RolloutItem],
    projection: &SpineProjection,
) -> BTreeMap<NodeId, NodeContextPressure> {
    let effective = effective_rollout(rollout);
    project_from_effective(&effective, projection)
}

pub(super) fn project_from_effective(
    effective_rollout: &[(usize, &RolloutItem)],
    projection: &SpineProjection,
) -> BTreeMap<NodeId, NodeContextPressure> {
    let samples = token_usage_samples_from_effective(effective_rollout);
    spine_core::context_pressures(projection, &samples)
        .into_iter()
        .map(|(node_id, pressure)| {
            (
                node_id,
                NodeContextPressure {
                    open_input_tokens: pressure.open_input_tokens,
                    current_input_tokens: pressure.current_input_tokens,
                    context_tokens: pressure.context_tokens,
                    problem: pressure.problem.map(|problem| match problem {
                        spine_core::ContextPressureProblem::MissingCurrentUsage => {
                            NodeContextPressureProblem::MissingCurrentUsage
                        }
                        spine_core::ContextPressureProblem::MissingOpenContextBaseline => {
                            NodeContextPressureProblem::MissingOpenContextBaseline
                        }
                        spine_core::ContextPressureProblem::CoordinateMismatch => {
                            NodeContextPressureProblem::CoordinateMismatch
                        }
                    }),
                },
            )
        })
        .collect()
}

fn token_usage_samples_from_effective(
    effective_rollout: &[(usize, &RolloutItem)],
) -> Vec<TokenUsageSample> {
    effective_rollout
        .iter()
        .filter_map(|(boundary, item)| {
            provider_input_tokens(item).map(|input_tokens| TokenUsageSample {
                boundary: spine_core::RawBoundary(*boundary as u64),
                input_tokens,
            })
        })
        .collect()
}

fn provider_input_tokens(item: &RolloutItem) -> Option<i64> {
    let RolloutItem::EventMsg(EventMsg::TokenCount(event)) = item else {
        return None;
    };
    let input_tokens = event.info.as_ref()?.last_token_usage.input_tokens;
    (input_tokens > 0).then_some(input_tokens)
}

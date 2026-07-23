use crate::context::ContextualUserFragment;
use crate::context::SpineStatusFragment;
use codex_protocol::models::ResponseItem;
use spine_core::SpineProjection;
use spine_core::TokenUsageSample;

pub(crate) fn prompt_overlay(
    projection: &SpineProjection,
    samples: &[TokenUsageSample],
    context_left_tokens: Option<i64>,
) -> Option<ResponseItem> {
    let pressures = spine_core::context_pressures(projection, samples);
    let signal = spine_core::status_signal(projection, &pressures, context_left_tokens);
    SpineStatusFragment::new(&signal)
        .ok()
        .map(ContextualUserFragment::into)
}

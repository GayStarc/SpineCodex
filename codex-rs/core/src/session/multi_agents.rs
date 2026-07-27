use crate::config::MultiAgentV2Config;
use crate::session::turn_context::TurnContext;
// Spine MODIFIED: Inspect the native MultiAgentV2 feature independently of Spine spawn.
// Reason: Enabling Spine tools must not activate Codex's native multi-agent prompt surface.
use codex_features::Feature;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

pub(super) fn usage_hint_text<'a>(
    turn_context: &'a TurnContext,
    session_source: &SessionSource,
) -> Option<&'a str> {
    // Spine MODIFIED: Gate usage hints on the explicit native surface, not only protocol version.
    // Reason: Spine spawn can share V2 transport while keeping native guidance disabled.
    if !multi_agent_v2_surface_enabled(turn_context) {
        return None;
    }

    let multi_agent_v2 = &turn_context.config.multi_agent_v2;
    configured_usage_hint_text_for_source(multi_agent_v2, session_source)
}

fn configured_usage_hint_text_for_source<'a>(
    multi_agent_v2: &'a MultiAgentV2Config,
    session_source: &SessionSource,
) -> Option<&'a str> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => {
            multi_agent_v2.subagent_usage_hint_text.as_deref()
        }
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => multi_agent_v2.root_agent_usage_hint_text.as_deref(),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

pub(crate) fn effective_multi_agent_mode(turn_context: &TurnContext) -> Option<MultiAgentMode> {
    // Spine MODIFIED: Apply the same explicit gate to effective multi-agent mode selection.
    // Reason: Native mode instructions must not leak into a Spine-only session.
    if !multi_agent_v2_surface_enabled(turn_context) {
        return None;
    }

    // A configured hint, including an empty string, defines a custom policy instead of an
    // effort-derived built-in policy.
    let multi_agent_mode = match &turn_context
        .config
        .multi_agent_v2
        .multi_agent_mode_hint_text
    {
        Some(hint_text) => MultiAgentMode::Custom(hint_text.clone()),
        None => match turn_context.effective_reasoning_effort() {
            Some(ReasoningEffort::Ultra) => MultiAgentMode::Proactive,
            _ => MultiAgentMode::ExplicitRequestOnly,
        },
    };

    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        | SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(multi_agent_mode),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

// Spine MODIFIED: Centralize the protocol-version and feature-flag conjunction.
// Reason: Both native prompt call sites require identical isolation from Spine spawn.
fn multi_agent_v2_surface_enabled(turn_context: &TurnContext) -> bool {
    turn_context.multi_agent_version == MultiAgentVersion::V2
        && turn_context
            .config
            .features
            .get()
            .enabled(Feature::MultiAgentV2)
}

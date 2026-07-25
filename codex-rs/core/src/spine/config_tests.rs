use super::*;
use crate::context::ContextualUserFragment;
use pretty_assertions::assert_eq;

#[test]
fn feature_off_uses_native_multi_agent_mode_prompts() {
    let config = SpineConfig::v1();
    for mode in [
        MultiAgentMode::ExplicitRequestOnly,
        MultiAgentMode::Proactive,
    ] {
        assert_eq!(
            multi_agent_mode_instructions(&config, mode.clone()).render(),
            MultiAgentModeInstructions::new(mode).render()
        );
    }
}

#[test]
fn spine_spawn_uses_its_configured_multi_agent_mode_prompts() {
    let config = SpineConfig::v1()
        .with_features([spine_core::Feature::Jit, spine_core::Feature::Spawn])
        .unwrap();
    for (mode, prompt) in [
        (
            MultiAgentMode::ExplicitRequestOnly,
            SpawnPromptMode::ExplicitRequestOnly,
        ),
        (MultiAgentMode::Proactive, SpawnPromptMode::Proactive),
    ] {
        assert_eq!(
            multi_agent_mode_instructions(&config, mode).body(),
            config.spawn_prompt(prompt).unwrap()
        );
    }
}

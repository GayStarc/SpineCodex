use super::*;
use crate::context::ContextualUserFragment;
use crate::session::tests::make_session_configuration_for_tests;
use crate::spine::observer::CodexSpineObserverHandler;
use crate::spine::session_runtime::SessionSpineRuntime;
use codex_features::Feature as CodexFeature;
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

#[derive(Debug, PartialEq, Eq)]
struct FeatureMatrixRow {
    bits: u8,
    validation: Result<(), spine_core::InitError>,
    runtime_started: Option<bool>,
    tools: Vec<String>,
    memory_projection: bool,
}

#[tokio::test]
async fn spine_current_feature_matrix_runtime_and_validation() {
    let base_configuration = make_session_configuration_for_tests().await;
    let mut actual = Vec::new();
    for bits in 0u8..16 {
        let jit = bits & 1 != 0;
        let trim = bits & 2 != 0;
        let spawn = bits & 4 != 0;
        let memory_projection = bits & 8 != 0;
        let mut sdk_features = Vec::new();
        if jit {
            sdk_features.push(spine_core::Feature::Jit);
        }
        if trim {
            sdk_features.push(spine_core::Feature::Trim);
        }
        if spawn {
            sdk_features.push(spine_core::Feature::Spawn);
        }

        let mut configuration = base_configuration.clone();
        let host = std::sync::Arc::make_mut(&mut configuration.original_config_do_not_use);
        for feature in [
            CodexFeature::SpineJit,
            CodexFeature::SpineTrim,
            CodexFeature::SpineSpawn,
            CodexFeature::SpinetreeMemoryProjection,
        ] {
            let _ = host.features.disable(feature);
        }
        for (enabled, feature) in [
            (jit, CodexFeature::SpineJit),
            (trim, CodexFeature::SpineTrim),
            (spawn, CodexFeature::SpineSpawn),
            (memory_projection, CodexFeature::SpinetreeMemoryProjection),
        ] {
            if enabled {
                let _ = host.features.enable(feature);
            }
        }

        let configured = SpineConfig::v1().with_features(sdk_features);
        let (validation, runtime_started, tools) = match configured {
            Ok(config) => {
                let tools = ToolCatalog::new(&config).unwrap().names();
                (
                    Ok(()),
                    Some(
                        SessionSpineRuntime::new(
                            &configuration,
                            CodexSpineObserverHandler::default(),
                            std::sync::Arc::new(std::sync::Mutex::new(None)),
                        )
                        .is_some(),
                    ),
                    tools,
                )
            }
            Err(error) => (Err(error), None, Vec::new()),
        };
        actual.push(FeatureMatrixRow {
            bits,
            validation,
            runtime_started,
            tools,
            memory_projection: configuration.spinetree_memory_projection_enabled(),
        });
    }

    let expected = (0u8..16)
        .map(|bits| {
            let jit = bits & 1 != 0;
            let trim = bits & 2 != 0;
            let spawn = bits & 4 != 0;
            let valid = !spawn || jit;
            let mut tools = Vec::new();
            if valid && jit {
                tools.extend(["spine.open", "spine.close", "spine.next"].map(str::to_string));
            }
            if valid && spawn {
                tools.push("spine.spawn".to_string());
            }
            if valid && trim {
                tools.push("spine.trim".to_string());
            }
            FeatureMatrixRow {
                bits,
                validation: valid
                    .then_some(())
                    .ok_or(spine_core::InitError::SpawnRequiresJit),
                runtime_started: valid.then_some(jit || trim),
                tools,
                memory_projection: bits & 8 != 0,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

use crate::session::session::SessionConfiguration;
use codex_features::Feature as CodexFeature;

impl SessionConfiguration {
    #[cfg(test)]
    pub(crate) fn disable_spine_jit_for_test(&mut self) {
        let config = std::sync::Arc::make_mut(&mut self.original_config_do_not_use);
        let _ = config.features.disable(CodexFeature::SpineJit);
    }

    #[cfg(test)]
    pub(crate) fn enable_spine_trim_for_test(&mut self) {
        let config = std::sync::Arc::make_mut(&mut self.original_config_do_not_use);
        let _ = config.features.enable(CodexFeature::SpineTrim);
    }

    pub(crate) fn spine_jit_enabled(&self) -> bool {
        self.original_config_do_not_use
            .features
            .enabled(CodexFeature::SpineJit)
    }

    pub(crate) fn spine_trim_enabled(&self) -> bool {
        self.original_config_do_not_use
            .features
            .enabled(CodexFeature::SpineTrim)
    }

    pub(crate) fn spine_spawn_enabled(&self) -> bool {
        self.original_config_do_not_use
            .features
            .enabled(CodexFeature::SpineSpawn)
    }

    pub(crate) fn spinetree_memory_projection_enabled(&self) -> bool {
        self.original_config_do_not_use
            .features
            .enabled(CodexFeature::SpinetreeMemoryProjection)
    }

    pub(crate) fn spine_sdk_config(&self) -> spine_core::SpineConfig {
        let mut features = Vec::new();
        if self.spine_jit_enabled() {
            features.push(spine_core::Feature::Jit);
        }
        if self.spine_trim_enabled() {
            features.push(spine_core::Feature::Trim);
        }
        if self.spine_spawn_enabled() {
            features.push(spine_core::Feature::Spawn);
        }
        self.original_config_do_not_use
            .spine_config
            .clone()
            .with_features(features)
            .expect("validated session Spine features must remain valid")
    }
}

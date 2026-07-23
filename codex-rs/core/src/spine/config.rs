use crate::config::ManagedFeatures;
use codex_features::Feature as CodexFeature;
use codex_utils_absolute_path::AbsolutePathBuf;
use spine_core::SpineConfig;
use spine_core::ToolCatalog;

pub(crate) fn load(
    path: Option<&AbsolutePathBuf>,
    enabled_features: &ManagedFeatures,
) -> std::io::Result<(SpineConfig, ToolCatalog)> {
    let source = match path {
        Some(path) => std::fs::read_to_string(path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to read spine_config_file {}: {error}",
                    path.display()
                ),
            )
        })?,
        None => spine_core::DEFAULT_CONFIG_TOML.to_string(),
    };
    let mut features = Vec::new();
    if enabled_features.enabled(CodexFeature::SpineJit) {
        features.push(spine_core::Feature::Jit);
    }
    if enabled_features.enabled(CodexFeature::SpineTrim) {
        features.push(spine_core::Feature::Trim);
    }
    if enabled_features.enabled(CodexFeature::SpineSpawn) {
        features.push(spine_core::Feature::Spawn);
    }
    let config = SpineConfig::parse_toml(&source)
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid Spine SDK configuration: {error}"),
            )
        })?
        .with_features(features)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let tools = ToolCatalog::new(&config)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    Ok((config, tools))
}

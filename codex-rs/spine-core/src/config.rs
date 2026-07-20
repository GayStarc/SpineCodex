use serde::Deserialize;
use std::fmt;

const MAX_TRIM_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../config/default.toml");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineConfig {
    schema_version: u32,
    trim_threshold_bytes: usize,
    jit_prompt: String,
    trim_prompt: String,
    spawn_prompt: String,
    tool_descriptions: ToolDescriptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolDescriptions {
    open: Option<String>,
    close: Option<String>,
    next: Option<String>,
    trim: Option<String>,
    spawn: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    schema_version: u32,
    #[serde(default)]
    limits: FileLimits,
    #[serde(default)]
    prompt: FilePrompt,
    #[serde(default)]
    tools: FileTools,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileLimits {
    #[serde(default = "default_trim_threshold")]
    trim_threshold_bytes: u64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FilePrompt {
    #[serde(default)]
    jit: Option<String>,
    #[serde(default)]
    trim: Option<String>,
    #[serde(default)]
    spawn: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileTools {
    #[serde(default)]
    open: Option<FileToolDescription>,
    #[serde(default)]
    close: Option<FileToolDescription>,
    #[serde(default)]
    next: Option<FileToolDescription>,
    #[serde(default)]
    trim: Option<FileToolDescription>,
    #[serde(default)]
    spawn: Option<FileToolDescription>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileToolDescription {
    description: String,
}

const fn default_trim_threshold() -> u64 {
    10_000
}

impl SpineConfig {
    pub fn v1() -> Self {
        Self::parse_toml(DEFAULT_CONFIG_TOML).expect("embedded Spine config is valid")
    }

    pub fn parse_toml(source: &str) -> Result<Self, ConfigError> {
        let parsed: FileConfig =
            toml::from_str(source).map_err(|error| ConfigError::InvalidToml(error.to_string()))?;
        if parsed.schema_version != 1 {
            return Err(ConfigError::UnsupportedSchemaVersion(parsed.schema_version));
        }
        if parsed.limits.trim_threshold_bytes == 0
            || parsed.limits.trim_threshold_bytes > MAX_TRIM_THRESHOLD_BYTES
        {
            return Err(ConfigError::InvalidTrimThreshold(
                parsed.limits.trim_threshold_bytes,
            ));
        }
        Ok(Self {
            schema_version: parsed.schema_version,
            trim_threshold_bytes: parsed.limits.trim_threshold_bytes as usize,
            jit_prompt: parsed.prompt.jit.unwrap_or_default(),
            trim_prompt: parsed.prompt.trim.unwrap_or_default(),
            spawn_prompt: parsed.prompt.spawn.unwrap_or_default(),
            tool_descriptions: ToolDescriptions {
                open: parsed.tools.open.map(|tool| tool.description),
                close: parsed.tools.close.map(|tool| tool.description),
                next: parsed.tools.next.map(|tool| tool.description),
                trim: parsed.tools.trim.map(|tool| tool.description),
                spawn: parsed.tools.spawn.map(|tool| tool.description),
            },
        })
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn trim_threshold_bytes(&self) -> usize {
        self.trim_threshold_bytes
    }

    pub(crate) fn prompt(&self, feature: crate::Feature) -> &str {
        match feature {
            crate::Feature::Jit => &self.jit_prompt,
            crate::Feature::Trim => &self.trim_prompt,
            crate::Feature::Spawn => &self.spawn_prompt,
        }
    }

    pub(crate) fn tool_description(&self, name: &str) -> Option<&str> {
        match name {
            "open" => self.tool_descriptions.open.as_deref(),
            "close" => self.tool_descriptions.close.as_deref(),
            "next" => self.tool_descriptions.next.as_deref(),
            "trim" => self.tool_descriptions.trim.as_deref(),
            "spawn" => self.tool_descriptions.spawn.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn validate_registration(
        &self,
        registration: &crate::SpineRegistration,
    ) -> Result<(), crate::InitError> {
        if registration.is_enabled(crate::Feature::Jit) {
            require_prompt(self.prompt(crate::Feature::Jit), crate::Feature::Jit)?;
            for name in ["open", "close", "next"] {
                require_tool(self.tool_description(name), name)?;
            }
        }
        if registration.is_enabled(crate::Feature::Trim) {
            require_tool(self.tool_description("trim"), "trim")?;
        }
        if registration.is_enabled(crate::Feature::Spawn) {
            require_tool(self.tool_description("spawn"), "spawn")?;
        }
        Ok(())
    }
}

fn require_prompt(value: &str, feature: crate::Feature) -> Result<(), crate::InitError> {
    if value.trim().is_empty() {
        return Err(crate::InitError::MissingPrompt(feature));
    }
    Ok(())
}

fn require_tool(value: Option<&str>, name: &'static str) -> Result<(), crate::InitError> {
    if value.is_none_or(|value| value.trim().is_empty()) {
        return Err(crate::InitError::MissingToolDescription(name));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidToml(String),
    UnsupportedSchemaVersion(u32),
    InvalidTrimThreshold(u64),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToml(error) => write!(formatter, "invalid Spine TOML: {error}"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported Spine config schema version {version}"
                )
            }
            Self::InvalidTrimThreshold(value) => {
                write!(formatter, "invalid Spine trim threshold {value}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema_version = 1
[limits]
trim_threshold_bytes = 2048
[prompt]
jit = "jit prompt"
trim = ""
spawn = "spawn prompt"
[tools.open]
description = "open"
[tools.close]
description = "close"
[tools.next]
description = "next"
[tools.trim]
description = "trim"
[tools.spawn]
description = "spawn"
"#;

    #[test]
    fn parses_and_exposes_typed_config() {
        let config = SpineConfig::parse_toml(VALID).unwrap();
        assert_eq!(config.schema_version(), 1);
        assert_eq!(config.trim_threshold_bytes(), 2048);
        assert_eq!(config.prompt(crate::Feature::Jit), "jit prompt");
        assert_eq!(config.tool_description("open"), Some("open"));
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_limits() {
        assert!(matches!(
            SpineConfig::parse_toml("schema_version = 1\nunknown = true"),
            Err(ConfigError::InvalidToml(_))
        ));
        assert!(matches!(
            SpineConfig::parse_toml("schema_version = 1\n[limits]\ntrim_threshold_bytes = 0"),
            Err(ConfigError::InvalidTrimThreshold(0))
        ));
    }

    #[test]
    fn default_v1_satisfies_jit_registration() {
        let config = SpineConfig::v1();
        let registration = crate::SpineRegistration::builder()
            .enable(crate::Feature::Jit)
            .build()
            .unwrap();
        config.validate_registration(&registration).unwrap();
    }
}

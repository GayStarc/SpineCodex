use crate::Feature;
use crate::SpawnTask;
use crate::SpineConfig;
use crate::SpineRegistration;
use crate::TrimRequest;
use serde::Deserialize;
use serde_json::Value;
use std::fmt;

pub const SPINE_NAMESPACE: &str = "spine";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpineTool {
    Open,
    Close,
    Next,
    Trim,
    Spawn,
}

impl SpineTool {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::Next => "next",
            Self::Trim => "trim",
            Self::Spawn => "spawn",
        }
    }

    pub fn qualified_name(self) -> String {
        format!("{SPINE_NAMESPACE}.{}", self.name())
    }

    pub const fn feature(self) -> Feature {
        match self {
            Self::Open | Self::Close | Self::Next => Feature::Jit,
            Self::Trim => Feature::Trim,
            Self::Spawn => Feature::Spawn,
        }
    }

    pub const fn all() -> [Self; 5] {
        [Self::Open, Self::Close, Self::Next, Self::Trim, Self::Spawn]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub tool: SpineTool,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCatalog {
    definitions: Vec<ToolDefinition>,
}

impl ToolCatalog {
    pub fn new(
        config: &SpineConfig,
        registration: &SpineRegistration,
    ) -> Result<Self, crate::InitError> {
        config.validate_registration(registration)?;
        Ok(Self::from_registration(config, registration))
    }

    pub(crate) fn from_registration(
        config: &SpineConfig,
        registration: &SpineRegistration,
    ) -> Self {
        let definitions = SpineTool::all()
            .into_iter()
            .filter(|tool| registration.is_enabled(tool.feature()))
            .map(|tool| ToolDefinition {
                tool,
                description: config
                    .tool_description(tool.name())
                    .unwrap_or_default()
                    .to_string(),
                parameters: parameters_for(tool),
            })
            .collect();
        Self { definitions }
    }

    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub fn definition(&self, tool: SpineTool) -> Option<&ToolDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.tool == tool)
    }

    pub fn names(&self) -> Vec<String> {
        self.definitions
            .iter()
            .map(|definition| definition.tool.qualified_name())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolValidation {
    Ordinary,
    Transition(ValidatedTransition),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValidatedTransition {
    Open { summary: String },
    Close { memory: String },
    Next { summary: String, memory: String },
    Trim(TrimRequest),
    Spawn { tasks: Vec<SpawnTask> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolValidationError {
    InvalidJson(String),
    UnknownTool(String),
    EmptyField(&'static str),
    InvalidTrim(String),
    InvalidSpawn(String),
}

impl fmt::Display for ToolValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(formatter, "invalid Spine tool arguments: {error}"),
            Self::UnknownTool(name) => write!(formatter, "unknown Spine tool {name}"),
            Self::EmptyField(name) => write!(formatter, "Spine tool field {name} is empty"),
            Self::InvalidTrim(error) => write!(formatter, "invalid spine.trim arguments: {error}"),
            Self::InvalidSpawn(error) => {
                write!(formatter, "invalid spine.spawn arguments: {error}")
            }
        }
    }
}

impl std::error::Error for ToolValidationError {}

pub fn validate_tool(
    tool: SpineTool,
    arguments: &str,
) -> Result<ToolValidation, ToolValidationError> {
    match tool {
        SpineTool::Open => {
            let args: OpenArgs = parse_control(arguments)?;
            Ok(ToolValidation::Transition(ValidatedTransition::Open {
                summary: non_empty(args.summary, "summary")?,
            }))
        }
        SpineTool::Close => {
            let args: CloseArgs = parse_control(arguments)?;
            Ok(ToolValidation::Transition(ValidatedTransition::Close {
                memory: non_empty(args.memory, "memory")?,
            }))
        }
        SpineTool::Next => {
            let args: NextArgs = parse_control(arguments)?;
            Ok(ToolValidation::Transition(ValidatedTransition::Next {
                summary: non_empty(args.summary, "summary")?,
                memory: non_empty(args.memory, "memory")?,
            }))
        }
        SpineTool::Trim => TrimRequest::parse(arguments)
            .map(ValidatedTransition::Trim)
            .map(ToolValidation::Transition)
            .map_err(ToolValidationError::InvalidTrim),
        SpineTool::Spawn => {
            let args: SpawnArgs = parse_control(arguments)?;
            if args.tasks.len() < 2 {
                return Err(ToolValidationError::InvalidSpawn(
                    "spine.spawn requires at least two tasks".to_string(),
                ));
            }
            for task in &args.tasks {
                if task.summary.trim().is_empty() {
                    return Err(ToolValidationError::EmptyField("summary"));
                }
                if task.prompt.trim().is_empty() {
                    return Err(ToolValidationError::EmptyField("prompt"));
                }
            }
            Ok(ToolValidation::Transition(ValidatedTransition::Spawn {
                tasks: args.tasks,
            }))
        }
    }
}

pub const fn success_carrier(tool: SpineTool) -> Option<&'static str> {
    match tool {
        SpineTool::Open => Some("Spine open accepted."),
        SpineTool::Close => Some("Spine close accepted."),
        SpineTool::Next => Some("Spine next accepted."),
        SpineTool::Trim => Some("Spine trim accepted."),
        SpineTool::Spawn => None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseArgs {
    memory: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NextArgs {
    summary: String,
    memory: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    tasks: Vec<SpawnTask>,
}

fn parse_control<T: for<'de> Deserialize<'de>>(arguments: &str) -> Result<T, ToolValidationError> {
    serde_json::from_str(arguments)
        .map_err(|error| ToolValidationError::InvalidJson(error.to_string()))
}

fn non_empty(value: String, field: &'static str) -> Result<String, ToolValidationError> {
    let value = value.trim().to_string();
    (!value.is_empty())
        .then_some(value)
        .ok_or(ToolValidationError::EmptyField(field))
}

fn parameters_for(tool: SpineTool) -> Value {
    match tool {
        SpineTool::Open => serde_json::json!({
            "type": "object",
            "properties": { "summary": { "type": "string", "description": "Concise, actionable, completable goal for the child node being opened. The transition call carrying this goal is retained in the child node's context." } },
            "required": ["summary"],
            "additionalProperties": false
        }),
        SpineTool::Close => serde_json::json!({
            "type": "object",
            "properties": { "memory": { "type": "string", "description": "Compiled continuation state for the node being finalized. This memory replaces the node's local working content for future continuation. Preserve only continuation-relevant state: completed or confirmed progress, key decisions and constraints, confirmed findings, validation results, unresolved factual gaps or risks, remaining work, and the logic linking evidence and findings to decisions and next steps. Use compact supporting evidence or precise, recoverable references wherever they clarify that logic. For source code, cite the precise path and line or line range; for commands or outputs, cite the exact command and decisive output or result, so later work can continue without replaying completed investigation or reloading the same context. Treat inherited ancestor context as already available. Runtime preserves user messages and child memories; use this memory for the additional state required for continuation. Preserve the continuation-relevant evolution of user intent by using [U#] anchors to resolve approvals, corrections, rejections, clarifications, and elliptical replies to their concrete referents, and record the resulting semantic deltas in task scope, decisions, constraints, progress, and remaining obligations." } },
            "required": ["memory"],
            "additionalProperties": false
        }),
        SpineTool::Next => serde_json::json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "Concise goal for the next sibling node. Make it actionable and completable. The transition call carrying this goal is retained in the sibling's context; continuation state from the node being finalized belongs in memory." },
                "memory": { "type": "string", "description": "Compiled continuation state for the node being finalized. This memory replaces the node's local working content for future continuation. Preserve only continuation-relevant state: completed or confirmed progress, key decisions and constraints, confirmed findings, validation results, unresolved factual gaps or risks, remaining work, and the logic linking evidence and findings to decisions and next steps. Use compact supporting evidence or precise, recoverable references wherever they clarify that logic. For source code, cite the precise path and line or line range; for commands or outputs, cite the exact command and decisive output or result, so later work can continue without replaying completed investigation or reloading the same context. Treat inherited ancestor context as already available. Runtime preserves user messages and child memories; use this memory for the additional state required for continuation. Preserve the continuation-relevant evolution of user intent by using [U#] anchors to resolve approvals, corrections, rejections, clarifications, and elliptical replies to their concrete referents, and record the resulting semantic deltas in task scope, decisions, constraints, progress, and remaining obligations." }
            },
            "required": ["summary", "memory"],
            "additionalProperties": false
        }),
        SpineTool::Trim => serde_json::json!({
            "type": "object",
            "properties": {
                "TRIM_ID": { "type": "string", "description": "Trim id attached to a tool response in the immediately previous tool-result batch; it expires after your next assistant tool request." },
                "op": { "type": "string", "enum": ["snip", "slice"], "description": "Use snip only when useful facts are preserved elsewhere; use slice to keep the needed head, tail, or anchor window." },
                "head": { "type": "integer", "description": "For op=\"slice\", keep this many characters from the start of the current visible body. Mutually exclusive with tail and anchor." },
                "tail": { "type": "integer", "description": "For op=\"slice\", keep this many characters from the end of the current visible body. Mutually exclusive with head and anchor." },
                "anchor": { "type": "string", "description": "For op=\"slice\", locate this non-empty text in the current visible body and keep an anchor window. Mutually exclusive with head and tail." },
                "preceding": { "type": "integer", "description": "For anchor slice, keep this many complete lines before the anchor line." },
                "following": { "type": "integer", "description": "For anchor slice, keep this many complete lines after the anchor line." }
            },
            "required": ["TRIM_ID", "op"],
            "additionalProperties": false
        }),
        SpineTool::Spawn => serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "Ordered self-contained child tasks.",
                    "minItems": 2,
                    "items": {
                        "type": "object",
                        "properties": {
                            "summary": { "type": "string", "description": "Concise label for one self-contained child task." },
                            "prompt": { "type": "string", "description": "Complete task instruction solvable from inherited context without parent follow-up." }
                        },
                        "required": ["summary", "prompt"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tasks"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_catalog_is_feature_gated() {
        let config = SpineConfig::v1();
        let registration = SpineRegistration::builder()
            .enable(Feature::Jit)
            .build()
            .unwrap();
        let catalog = ToolCatalog::from_registration(&config, &registration);
        assert_eq!(catalog.names(), ["spine.open", "spine.close", "spine.next"]);
        assert!(catalog.definition(SpineTool::Trim).is_none());
    }

    #[test]
    fn feature_off_catalog_is_empty() {
        let registration = SpineRegistration::builder().build().unwrap();
        let catalog = ToolCatalog::new(&SpineConfig::v1(), &registration).unwrap();
        assert!(catalog.definitions().is_empty());
    }

    #[test]
    fn validators_reject_malformed_controls_and_spawn_vectors() {
        assert!(validate_tool(SpineTool::Open, r#"{"summary":" task "}"#).is_ok());
        assert!(validate_tool(SpineTool::Close, r#"{"memory":" "}"#).is_err());
        assert!(validate_tool(SpineTool::Open, r#"{"summary":"x","extra":1}"#).is_err());
        assert!(validate_tool(SpineTool::Spawn, r#"{"tasks":[]}"#).is_err());
    }
}

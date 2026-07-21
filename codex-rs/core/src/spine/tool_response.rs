use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::spine_sdk_spec::SPINE_CLOSE;
use crate::tools::handlers::spine_sdk_spec::SPINE_NAMESPACE;
use crate::tools::handlers::spine_sdk_spec::SPINE_NEXT;
use crate::tools::handlers::spine_sdk_spec::SPINE_OPEN;
use crate::tools::handlers::spine_sdk_spec::SPINE_TRIM;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use spine_core::SpineTool;
use spine_core::ToolOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpineToolResponse {
    Open,
    Close,
    Next,
    Trim,
}

impl SpineToolResponse {
    pub(crate) fn from_control(tool: SpineTool) -> Self {
        match tool {
            SpineTool::Open => Self::Open,
            SpineTool::Close => Self::Close,
            SpineTool::Next => Self::Next,
            SpineTool::Trim => Self::Trim,
            SpineTool::Spawn => unreachable!("spawn has a structured receipt, not a text carrier"),
        }
    }
    pub(crate) fn success(self) -> FunctionToolOutput {
        FunctionToolOutput::from_text(self.success_carrier(), Some(true))
    }

    pub(crate) fn outcome(tool_name: &str, payload: &FunctionCallOutputPayload) -> ToolOutcome {
        match payload.success {
            Some(true) => ToolOutcome::Succeeded,
            Some(false) => ToolOutcome::Failed,
            None => {
                let Some(tool) = Self::from_qualified_name(tool_name) else {
                    return ToolOutcome::Unknown;
                };
                if matches!(
                    &payload.body,
                    FunctionCallOutputBody::Text(body) if body == &tool.success_carrier()
                ) {
                    ToolOutcome::Succeeded
                } else {
                    ToolOutcome::Unknown
                }
            }
        }
    }

    fn from_qualified_name(name: &str) -> Option<Self> {
        let (namespace, tool_name) = name.split_once('.')?;
        if namespace != SPINE_NAMESPACE {
            return None;
        }
        match tool_name {
            SPINE_OPEN => Some(Self::Open),
            SPINE_CLOSE => Some(Self::Close),
            SPINE_NEXT => Some(Self::Next),
            SPINE_TRIM => Some(Self::Trim),
            _ => None,
        }
    }

    #[cfg(test)]
    fn qualified_name(self) -> String {
        format!("{SPINE_NAMESPACE}.{}", self.tool_name())
    }

    #[cfg(test)]
    fn tool_name(self) -> &'static str {
        match self {
            Self::Open => SPINE_OPEN,
            Self::Close => SPINE_CLOSE,
            Self::Next => SPINE_NEXT,
            Self::Trim => SPINE_TRIM,
        }
    }

    fn success_carrier(self) -> String {
        let tool = match self {
            Self::Open => SpineTool::Open,
            Self::Close => SpineTool::Close,
            Self::Next => SpineTool::Next,
            Self::Trim => SpineTool::Trim,
        };
        spine_core::success_carrier(tool)
            .unwrap_or_else(|| unreachable!("control tools always have a success carrier"))
            .to_string()
    }
}

#[cfg(test)]
#[path = "tool_response_tests.rs"]
mod tests;

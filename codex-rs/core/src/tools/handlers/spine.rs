use crate::function_tool::FunctionCallError;
use crate::spine::tool_response::SpineToolResponse;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::spine_sdk_spec::SPINE_NAMESPACE;
use crate::tools::handlers::spine_sdk_spec::SPINE_SPAWN;
use crate::tools::handlers::spine_sdk_spec::SPINE_TRIM;
use crate::tools::handlers::spine_sdk_spec::create_spine_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::config_types::ModeKind;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use spine_core::SpineOperationFact;
use spine_core::SpineTool;
use spine_core::ToolCatalog;
use spine_core::ToolDefinition;
#[cfg(test)]
use spine_core::TrimOperation;
use spine_core::TrimRequest;
#[cfg(test)]
use spine_core::TrimSlice;

pub(crate) struct SpineHandler {
    kind: SpineHandlerKind,
    definition: ToolDefinition,
}

#[derive(Clone, Copy)]
enum SpineHandlerKind {
    Control(SpineTool),
    Spawn,
    Trim,
}

impl SpineHandler {
    pub(crate) fn add_tools(catalog: &ToolCatalog, mode: ModeKind, mut add: impl FnMut(Self)) {
        for definition in catalog.definitions() {
            if mode == ModeKind::Plan && definition.tool == SpineTool::Spawn {
                continue;
            }
            add(Self::from_definition(definition.clone()));
        }
    }

    fn from_definition(definition: ToolDefinition) -> Self {
        let kind = match definition.tool {
            SpineTool::Open | SpineTool::Close | SpineTool::Next => {
                SpineHandlerKind::Control(definition.tool)
            }
            SpineTool::Spawn => SpineHandlerKind::Spawn,
            SpineTool::Trim => SpineHandlerKind::Trim,
        };
        Self { kind, definition }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            SpineHandlerKind::Control(tool) => tool.name(),
            SpineHandlerKind::Spawn => SPINE_SPAWN,
            SpineHandlerKind::Trim => SPINE_TRIM,
        }
    }
}

#[cfg(test)]
fn validate_arguments(tool: SpineTool, arguments: &str) -> Result<(), FunctionCallError> {
    validate_control_fact(tool, arguments).map(|_| ())
}

fn validate_control_fact(
    tool: SpineTool,
    arguments: &str,
) -> Result<SpineOperationFact, FunctionCallError> {
    crate::spine::validated_control_fact(tool, arguments).map_err(|error| {
        let message = match error {
            spine_core::ToolValidationError::InvalidJson(error) => {
                format!("failed to parse function arguments: {error}")
            }
            spine_core::ToolValidationError::EmptyField(_) => {
                format!("{} requires a non-empty argument", tool.name())
            }
            error => error.to_string(),
        };
        FunctionCallError::RespondToModel(message)
    })
}

impl ToolExecutor<ToolInvocation> for SpineHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(SPINE_NAMESPACE, self.name())
    }

    fn spec(&self) -> ToolSpec {
        create_spine_tool(&self.definition)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectAndCodeMode
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl SpineHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            call_id,
            cancellation_token,
            payload,
            source,
            ..
        } = invocation;
        let (origin, spawn_scope, code_mode_trim) = match source {
            ToolCallSource::Direct => (
                spine_core::ExecutionOrigin::Direct {
                    call_id: call_id.clone(),
                },
                crate::spine::spawn::SpawnExecutionScope::ResponseGroup(step_context),
                false,
            ),
            ToolCallSource::CodeMode {
                cell_id,
                runtime_tool_call_id,
            } => {
                if session.lock_spine_coordinator().is_none() {
                    return Err(FunctionCallError::RespondToModel(
                        "Code Mode Spine calls are unavailable after restoring a legacy Spine rollout"
                            .to_string(),
                    ));
                }
                let cell_id = codex_code_mode::CellId::new(cell_id);
                let outer_call_id = session
                    .services
                    .code_mode_service
                    .outer_call_id(&cell_id)
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(format!(
                            "Code Mode cell `{cell_id}` is missing its outer call identity"
                        ))
                    })?;
                let invocation_ordinal = session
                    .services
                    .code_mode_service
                    .spine_invocation_ordinal(&cell_id, &runtime_tool_call_id)
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(format!(
                            "Code Mode Spine call `{runtime_tool_call_id}` is missing its invocation ordinal"
                        ))
                    })?;
                (
                    spine_core::ExecutionOrigin::CodeMode {
                        outer_call_id: outer_call_id.clone(),
                        invocation_ordinal,
                    },
                    crate::spine::spawn::SpawnExecutionScope::Isolated {
                        fork_parent_call_id: outer_call_id,
                    },
                    true,
                )
            }
        };
        if turn.collaboration_mode.mode == ModeKind::Plan {
            return Err(FunctionCallError::RespondToModel(
                "Spine transitions are not allowed in Plan mode".to_string(),
            ));
        }
        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "Spine handler received unsupported payload".to_string(),
                ));
            }
        };

        let response_tool = match self.kind {
            SpineHandlerKind::Control(tool) => {
                let operation = validate_control_fact(tool, &arguments)?;
                let response = SpineToolResponse::from_control(tool);
                match session.validate_spine_control(tool).await {
                    Ok(()) => {
                        session.stage_spine_fact(&call_id, origin.clone(), operation);
                        response
                    }
                    Err(error) => {
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                }
            }
            SpineHandlerKind::Spawn => {
                let tasks = crate::spine::spawn::parse_tasks(&arguments)
                    .map_err(FunctionCallError::RespondToModel)?;
                let receipt = crate::spine::spawn::execute(
                    session.clone(),
                    turn,
                    spawn_scope,
                    call_id.clone(),
                    cancellation_token,
                    tasks.clone(),
                )
                .await;
                let receipt = match receipt {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                };
                session.stage_spine_fact(
                    &call_id,
                    origin,
                    SpineOperationFact::Spawn {
                        tasks,
                        terminal_results: receipt.results.clone(),
                    },
                );
                let body = receipt.encode_json().map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to encode spine.spawn receipt: {error}"
                    ))
                })?;
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    body,
                    Some(true),
                )));
            }
            SpineHandlerKind::Trim => {
                let request =
                    TrimRequest::parse(&arguments).map_err(FunctionCallError::RespondToModel)?;
                let validation = if code_mode_trim {
                    session.validate_spine_trim_request(&request).await
                } else {
                    session.validate_spine_trim(&call_id, &request).await
                };
                match validation {
                    Ok(()) => {
                        if let Some(operation) = session.validated_spine_trim_fact(&request) {
                            session.stage_spine_fact(&call_id, origin, operation);
                        }
                        SpineToolResponse::Trim
                    }
                    Err(error) => {
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                }
            }
        };

        Ok(boxed_tool_output(response_tool.success()))
    }
}

impl CoreToolRuntime for SpineHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        matches!(self.kind, SpineHandlerKind::Spawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn catalog() -> ToolCatalog {
        let config = spine_core::SpineConfig::v1()
            .with_features([
                spine_core::Feature::Jit,
                spine_core::Feature::Trim,
                spine_core::Feature::Spawn,
            ])
            .unwrap();
        ToolCatalog::new(&config).unwrap()
    }

    fn handlers(mode: ModeKind) -> Vec<SpineHandler> {
        let mut handlers = Vec::new();
        SpineHandler::add_tools(&catalog(), mode, |handler| handlers.push(handler));
        handlers
    }

    #[test]
    fn validates_control_argument_matrix() {
        for (kind, arguments) in [
            (SpineTool::Open, r#"{"summary":"task"}"#),
            (SpineTool::Close, r#"{"memory":"done"}"#),
            (SpineTool::Next, r#"{"summary":"sibling","memory":"done"}"#),
        ] {
            assert!(validate_arguments(kind, arguments).is_ok());
        }

        for (kind, arguments) in [
            (SpineTool::Open, r#"{"summary":" "}"#),
            (SpineTool::Close, r#"{"memory":""}"#),
            (SpineTool::Next, r#"{"summary":"sibling","memory":" "}"#),
            (SpineTool::Open, r#"{"summary":"task","extra":1}"#),
            (SpineTool::Close, "not-json"),
        ] {
            assert!(validate_arguments(kind, arguments).is_err());
        }

        assert!(matches!(
            validate_arguments(SpineTool::Open, r#"{"summary":" "}"#),
            Err(FunctionCallError::RespondToModel(message))
                if message == "open requires a non-empty argument"
        ));
        assert!(matches!(
            validate_arguments(SpineTool::Close, "not-json"),
            Err(FunctionCallError::RespondToModel(message))
                if message.starts_with("failed to parse function arguments:")
        ));
    }

    #[test]
    fn tool_registration_follows_sdk_catalog() {
        let catalog = catalog();
        let mut handlers = Vec::new();
        SpineHandler::add_tools(&catalog, ModeKind::Default, |handler| {
            handlers.push(handler);
        });
        assert_eq!(
            handlers
                .iter()
                .map(codex_tools::ToolExecutor::tool_name)
                .collect::<Vec<_>>(),
            catalog
                .definitions()
                .iter()
                .map(|definition| { ToolName::namespaced(SPINE_NAMESPACE, definition.tool.name()) })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn plan_mode_suppresses_only_spawn() {
        assert_eq!(
            handlers(ModeKind::Plan)
                .iter()
                .map(codex_tools::ToolExecutor::tool_name)
                .collect::<Vec<_>>(),
            [
                SpineTool::Open,
                SpineTool::Close,
                SpineTool::Next,
                SpineTool::Trim,
            ]
            .map(|tool| ToolName::namespaced(SPINE_NAMESPACE, tool.name()))
        );
    }

    #[test]
    fn spine_tools_support_code_mode() {
        assert!(
            handlers(ModeKind::Default)
                .iter()
                .all(|handler| handler.exposure() == ToolExposure::DirectAndCodeMode)
        );
    }

    #[test]
    fn trim_arguments_cover_snip_and_slice_shapes() {
        let snip = TrimRequest::parse(r#"{"TRIM_ID":"trim_4","op":"snip"}"#).unwrap();
        assert_eq!(snip.trim_id, "trim_4");
        assert_eq!(snip.operation, TrimOperation::Snip);
        let slice = TrimRequest::parse(r#"{"TRIM_ID":"trim_4","op":"slice","tail":3}"#).unwrap();
        assert_eq!(
            slice.operation,
            TrimOperation::Slice(TrimSlice::Tail { tail: 3 })
        );
        assert!(TrimRequest::parse(r#"{"TRIM_ID":"trim_4","op":"slice"}"#).is_err());
    }
}

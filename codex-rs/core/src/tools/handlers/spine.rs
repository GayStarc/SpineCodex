use crate::function_tool::FunctionCallError;
use crate::spine::SpineControlKind;
use crate::spine::tool_response::SpineToolResponse;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::spine_sdk_spec::SPINE_CLOSE;
use crate::tools::handlers::spine_sdk_spec::SPINE_NAMESPACE;
use crate::tools::handlers::spine_sdk_spec::SPINE_NEXT;
use crate::tools::handlers::spine_sdk_spec::SPINE_OPEN;
use crate::tools::handlers::spine_sdk_spec::SPINE_SPAWN;
use crate::tools::handlers::spine_sdk_spec::SPINE_TRIM;
use crate::tools::handlers::spine_sdk_spec::create_spine_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::config_types::ModeKind;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
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
    Control(SpineControlKind),
    Spawn,
    Trim,
}

impl SpineHandler {
    pub(crate) fn controls(catalog: &ToolCatalog) -> Vec<Self> {
        [SpineTool::Open, SpineTool::Close, SpineTool::Next]
            .into_iter()
            .filter_map(|tool| catalog.definition(tool).cloned())
            .map(Self::from_definition)
            .collect()
    }

    pub(crate) fn trim(catalog: &ToolCatalog) -> Option<Self> {
        catalog
            .definition(SpineTool::Trim)
            .cloned()
            .map(Self::from_definition)
    }

<<<<<<< HEAD
    pub(crate) fn spawn() -> Self {
        Self {
            kind: SpineHandlerKind::Spawn,
        }
=======
    pub(crate) fn spawn(catalog: &ToolCatalog) -> Option<Self> {
        catalog
            .definition(SpineTool::Spawn)
            .cloned()
            .map(Self::from_definition)
    }

    fn from_definition(definition: ToolDefinition) -> Self {
        let kind = match definition.tool {
            SpineTool::Open => SpineHandlerKind::Control(SpineControlKind::Open),
            SpineTool::Close => SpineHandlerKind::Control(SpineControlKind::Close),
            SpineTool::Next => SpineHandlerKind::Control(SpineControlKind::Next),
            SpineTool::Spawn => SpineHandlerKind::Spawn,
            SpineTool::Trim => SpineHandlerKind::Trim,
        };
        Self { kind, definition }
>>>>>>> refactor(spine): move config and tool contracts into sdk
    }

    fn name(&self) -> &'static str {
        match self.kind {
            SpineHandlerKind::Control(SpineControlKind::Open) => SPINE_OPEN,
            SpineHandlerKind::Control(SpineControlKind::Close) => SPINE_CLOSE,
            SpineHandlerKind::Control(SpineControlKind::Next) => SPINE_NEXT,
            SpineHandlerKind::Spawn => SPINE_SPAWN,
            SpineHandlerKind::Trim => SPINE_TRIM,
        }
    }
}

fn validate_arguments(kind: SpineControlKind, arguments: &str) -> Result<(), FunctionCallError> {
    let tool = match kind {
        SpineControlKind::Open => SpineTool::Open,
        SpineControlKind::Close => SpineTool::Close,
        SpineControlKind::Next => SpineTool::Next,
    };
    spine_core::validate_tool(tool, arguments)
        .map(|_| ())
        .map_err(|error| {
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
<<<<<<< HEAD
        match self.kind {
            SpineHandlerKind::Control(_) => create_spine_tool(self.name()),
            SpineHandlerKind::Spawn => create_spine_spawn_tool(),
            SpineHandlerKind::Trim => create_spine_trim_tool(),
        }
=======
        create_spine_tool(&self.definition)
>>>>>>> refactor(spine): move config and tool contracts into sdk
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
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
        if matches!(source, ToolCallSource::CodeMode { .. }) {
            return Err(FunctionCallError::RespondToModel(
                "Spine is not available as a Code Mode nested tool".to_string(),
            ));
        }
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
            SpineHandlerKind::Control(kind) => {
                validate_arguments(kind, &arguments)?;
                session
                    .validate_spine_control(kind)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                SpineToolResponse::from(kind)
            }
            SpineHandlerKind::Spawn => {
<<<<<<< HEAD
                let tasks = crate::spine::spawn::parse_tasks(&arguments)
                    .map_err(FunctionCallError::RespondToModel)?;
<<<<<<< HEAD
                let admission = self.admit_nested(&session, nested_source, arguments.clone())?;
                let receipt = match admission.as_ref() {
                    Some(admission) => {
                        crate::spine::spawn::execute_nested(
                            session.clone(),
                            turn.clone(),
                            admission.outer_exec_call_id().to_string(),
                            admission.invocation_ordinal(),
                            cancellation_token,
                            tasks,
                        )
                        .await
                    }
                    None => {
                        crate::spine::spawn::execute(
                            session.clone(),
                            turn.clone(),
                            call_id,
                            cancellation_token,
                            tasks,
                        )
=======
                let receipt =
                    crate::spine::spawn::execute(session, turn, call_id, cancellation_token, tasks)
>>>>>>> refactor(spine): move config and tool contracts into sdk
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
=======
                let receipt = crate::spine::spawn::execute(
                    session,
                    turn,
                    step_context,
                    call_id,
                    cancellation_token,
                    arguments,
                )
                .await
                .map_err(FunctionCallError::RespondToModel)?;
>>>>>>> refactor(spine): align spawn lifecycle with sdk contract
                let body = crate::spine::spawn::encode_receipt(&receipt).map_err(|error| {
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
                session
                    .validate_spine_trim(&call_id, &request)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                SpineToolResponse::Trim
            }
        };

        Ok(boxed_tool_output(response_tool.success()))
    }
<<<<<<< HEAD

    fn admit_nested(
        &self,
        session: &crate::session::session::Session,
        source: Option<(CellId, String)>,
        arguments: String,
    ) -> Result<Option<NestedSpineAdmission>, FunctionCallError> {
        let Some((cell_id, runtime_call_id)) = source else {
            return Ok(None);
        };
        session
            .services
            .code_mode_service
            .admit_spine(
                &cell_id,
                runtime_call_id,
                self.nested_tool_name(),
                arguments,
            )
            .map(Some)
            .map_err(FunctionCallError::RespondToModel)
    }

    fn nested_tool_name(&self) -> NestedSpineToolName {
        match self.kind {
            SpineHandlerKind::Control(SpineControlKind::Open) => NestedSpineToolName::Open,
            SpineHandlerKind::Control(SpineControlKind::Close) => NestedSpineToolName::Close,
            SpineHandlerKind::Control(SpineControlKind::Next) => NestedSpineToolName::Next,
            SpineHandlerKind::Spawn => NestedSpineToolName::Spawn,
            SpineHandlerKind::Trim => NestedSpineToolName::Trim,
        }
    }
}

fn complete_nested(
    admission: Option<NestedSpineAdmission>,
    success: bool,
    body: String,
) -> Result<(), FunctionCallError> {
    let Some(admission) = admission else {
        return Ok(());
    };
    admission
        .complete(success, body)
        .map_err(FunctionCallError::RespondToModel)
=======
>>>>>>> refactor(spine): move config and tool contracts into sdk
}

impl CoreToolRuntime for SpineHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        matches!(self.kind, SpineHandlerKind::Spawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ToolCatalog {
        let registration = spine_core::SpineRegistration::builder()
            .enable(spine_core::Feature::Jit)
            .enable(spine_core::Feature::Trim)
            .enable(spine_core::Feature::Spawn)
            .build()
            .unwrap();
        ToolCatalog::new(&spine_core::SpineConfig::v1(), &registration).unwrap()
    }

    #[test]
    fn validates_control_argument_matrix() {
        for (kind, arguments) in [
            (SpineControlKind::Open, r#"{"summary":"task"}"#),
            (SpineControlKind::Close, r#"{"memory":"done"}"#),
            (
                SpineControlKind::Next,
                r#"{"summary":"sibling","memory":"done"}"#,
            ),
        ] {
            assert!(validate_arguments(kind, arguments).is_ok());
        }

        for (kind, arguments) in [
            (SpineControlKind::Open, r#"{"summary":" "}"#),
            (SpineControlKind::Close, r#"{"memory":""}"#),
            (
                SpineControlKind::Next,
                r#"{"summary":"sibling","memory":" "}"#,
            ),
            (SpineControlKind::Open, r#"{"summary":"task","extra":1}"#),
            (SpineControlKind::Close, "not-json"),
        ] {
            assert!(validate_arguments(kind, arguments).is_err());
        }

        assert!(matches!(
            validate_arguments(SpineControlKind::Open, r#"{"summary":" "}"#),
            Err(FunctionCallError::RespondToModel(message))
                if message == "open requires a non-empty argument"
        ));
        assert!(matches!(
            validate_arguments(SpineControlKind::Close, "not-json"),
            Err(FunctionCallError::RespondToModel(message))
                if message.starts_with("failed to parse function arguments:")
        ));
    }

    #[test]
    fn tool_names_are_namespaced() {
        let catalog = catalog();
        let handlers = SpineHandler::controls(&catalog);
        assert_eq!(
            handlers[0].tool_name(),
            ToolName::namespaced(SPINE_NAMESPACE, SPINE_OPEN)
        );
        assert_eq!(
            handlers[1].tool_name(),
            ToolName::namespaced(SPINE_NAMESPACE, SPINE_CLOSE)
        );
        assert_eq!(
            handlers[2].tool_name(),
            ToolName::namespaced(SPINE_NAMESPACE, SPINE_NEXT)
        );
    }

    #[test]
    fn spine_tools_are_direct_model_only() {
        assert!(
            SpineHandler::controls(&catalog())
                .iter()
                .all(|handler| handler.exposure() == ToolExposure::DirectModelOnly)
        );
        assert_eq!(
            SpineHandler::trim(&catalog()).unwrap().exposure(),
            ToolExposure::DirectModelOnly
        );
        assert_eq!(
<<<<<<< HEAD
            SpineHandler::spawn().exposure(),
            ToolExposure::DirectAndCodeMode
=======
            SpineHandler::spawn(&catalog()).unwrap().exposure(),
            ToolExposure::DirectModelOnly
>>>>>>> refactor(spine): move config and tool contracts into sdk
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

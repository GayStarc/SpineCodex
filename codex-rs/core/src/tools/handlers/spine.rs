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
    Spawn { max_tasks: usize },
    Trim,
}

impl SpineHandler {
<<<<<<< HEAD
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
=======
    pub(crate) fn add_tools(catalog: &ToolCatalog, mode: ModeKind, mut add: impl FnMut(Self)) {
        for definition in catalog.definitions() {
            if mode == ModeKind::Plan && definition.tool == SpineTool::Spawn {
                continue;
            }
            add(Self::from_definition(
                definition.clone(),
                catalog.spawn_task_limit(),
            ));
        }
>>>>>>> refactor(spine): let SDK own tool exposure
    }

    fn from_definition(definition: ToolDefinition, spawn_task_limit: usize) -> Self {
        let kind = match definition.tool {
            SpineTool::Open | SpineTool::Close | SpineTool::Next => {
                SpineHandlerKind::Control(definition.tool)
            }
            SpineTool::Spawn => SpineHandlerKind::Spawn {
                max_tasks: spawn_task_limit,
            },
            SpineTool::Trim => SpineHandlerKind::Trim,
        };
        Self { kind, definition }
>>>>>>> refactor(spine): move config and tool contracts into sdk
    }

    fn name(&self) -> &'static str {
        match self.kind {
            SpineHandlerKind::Control(tool) => tool.name(),
            SpineHandlerKind::Spawn { .. } => SPINE_SPAWN,
            SpineHandlerKind::Trim => SPINE_TRIM,
        }
    }
}

fn validate_arguments(tool: SpineTool, arguments: &str) -> Result<(), FunctionCallError> {
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
            SpineHandlerKind::Control(tool) => {
                validate_arguments(tool, &arguments)?;
                session
                    .validate_spine_control(tool)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                SpineToolResponse::from_control(tool)
            }
<<<<<<< HEAD
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
=======
            SpineHandlerKind::Spawn { max_tasks } => {
                let catalog = ToolCatalog::new(&turn.config.spine_config)
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                    .with_spawn_task_limit(max_tasks);
                catalog
                    .validate(SpineTool::Spawn, &arguments)
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
>>>>>>> fix(spine): preserve sdk context guidance on main
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
<<<<<<< HEAD
>>>>>>> refactor(spine): align spawn lifecycle with sdk contract
                let body = crate::spine::spawn::encode_receipt(&receipt).map_err(|error| {
=======
                let body = receipt.encode_json().map_err(|error| {
>>>>>>> refactor(spine): minimize codex adapter boundary
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
        matches!(self.kind, SpineHandlerKind::Spawn { .. })
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
                .map(|handler| handler.tool_name())
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
                .map(|handler| handler.tool_name())
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
    fn spine_tools_are_direct_model_only() {
        assert!(
            handlers(ModeKind::Default)
                .iter()
                .all(|handler| handler.exposure() == ToolExposure::DirectModelOnly)
        );
<<<<<<< HEAD
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
=======
>>>>>>> refactor(spine): let SDK own tool exposure
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

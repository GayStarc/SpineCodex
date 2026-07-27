use crate::function_tool::FunctionCallError;
use crate::spine::SpineControlKind;
use crate::spine::tool_response::SpineToolResponse;
use crate::tools::code_mode::spine_bridge::NestedSpineAdmission;
use crate::tools::code_mode::spine_bridge::NestedSpineToolName;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::spine_spec::SPINE_CLOSE;
use crate::tools::handlers::spine_spec::SPINE_NAMESPACE;
use crate::tools::handlers::spine_spec::SPINE_NEXT;
use crate::tools::handlers::spine_spec::SPINE_OPEN;
use crate::tools::handlers::spine_spec::SPINE_SPAWN;
use crate::tools::handlers::spine_spec::SPINE_TRIM;
use crate::tools::handlers::spine_spec::create_spine_spawn_tool;
use crate::tools::handlers::spine_spec::create_spine_tool;
use crate::tools::handlers::spine_spec::create_spine_trim_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_code_mode::CellId;
use codex_protocol::config_types::ModeKind;
#[cfg(test)]
use codex_spine_core::TrimOperation;
use codex_spine_core::TrimRequest;
#[cfg(test)]
use codex_spine_core::TrimSlice;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

pub(crate) struct SpineHandler {
    kind: SpineHandlerKind,
}

#[derive(Clone, Copy)]
enum SpineHandlerKind {
    Control(SpineControlKind),
    Spawn,
    Trim,
}

impl SpineHandler {
    pub(crate) fn all() -> [Self; 3] {
        [
            Self {
                kind: SpineHandlerKind::Control(SpineControlKind::Open),
            },
            Self {
                kind: SpineHandlerKind::Control(SpineControlKind::Close),
            },
            Self {
                kind: SpineHandlerKind::Control(SpineControlKind::Next),
            },
        ]
    }

    pub(crate) fn trim() -> Self {
        Self {
            kind: SpineHandlerKind::Trim,
        }
    }

    pub(crate) fn spawn() -> Self {
        Self {
            kind: SpineHandlerKind::Spawn,
        }
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

fn non_empty(value: String, name: &str) -> Result<String, FunctionCallError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "{name} requires a non-empty argument"
        )));
    }
    Ok(value)
}

fn validate_arguments(kind: SpineControlKind, arguments: &str) -> Result<(), FunctionCallError> {
    match kind {
        SpineControlKind::Open => {
            let args: OpenArgs = parse_arguments(arguments)?;
            non_empty(args.summary, SPINE_OPEN)?;
        }
        SpineControlKind::Close => {
            let args: CloseArgs = parse_arguments(arguments)?;
            non_empty(args.memory, SPINE_CLOSE)?;
        }
        SpineControlKind::Next => {
            let args: NextArgs = parse_arguments(arguments)?;
            non_empty(args.summary, SPINE_NEXT)?;
            non_empty(args.memory, SPINE_NEXT)?;
        }
    }
    Ok(())
}

impl ToolExecutor<ToolInvocation> for SpineHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(SPINE_NAMESPACE, self.name())
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            SpineHandlerKind::Control(_) => create_spine_tool(self.name()),
            SpineHandlerKind::Spawn => create_spine_spawn_tool(),
            SpineHandlerKind::Trim => create_spine_trim_tool(),
        }
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
            call_id,
            cancellation_token,
            payload,
            source,
            ..
        } = invocation;
        let nested_source = match source {
            ToolCallSource::Direct => None,
            ToolCallSource::CodeMode {
                cell_id,
                runtime_tool_call_id,
            } => Some((CellId::new(cell_id), runtime_tool_call_id)),
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
            SpineHandlerKind::Control(kind) => {
                validate_arguments(kind, &arguments)?;
                let admission = self.admit_nested(&session, nested_source, arguments.clone())?;
                let response = SpineToolResponse::from(kind);
                match session.validate_spine_control(kind).await {
                    Ok(()) => {
                        complete_nested(admission, true, response.success_carrier())?;
                        response
                    }
                    Err(error) => {
                        complete_nested(admission, false, error.clone())?;
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                }
            }
            SpineHandlerKind::Spawn => {
                let tasks = crate::spine::spawn::parse_tasks(&arguments)
                    .map_err(FunctionCallError::RespondToModel)?;
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
                        .await
                    }
                };
                let receipt = match receipt {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        complete_nested(admission, false, error.clone())?;
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                };
                let body = match crate::spine::spawn::encode_receipt(&receipt) {
                    Ok(body) => body,
                    Err(error) => {
                        let error = format!("failed to encode spine.spawn receipt: {error}");
                        complete_nested(admission, false, error.clone())?;
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                };
                if admission.is_some() {
                    complete_nested(admission, true, body)?;
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("Spine {SPINE_SPAWN} accepted."),
                        Some(true),
                    )));
                }
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    body,
                    Some(true),
                )));
            }
            SpineHandlerKind::Trim => {
                let request =
                    TrimRequest::parse(&arguments).map_err(FunctionCallError::RespondToModel)?;
                let admission = self.admit_nested(&session, nested_source, arguments.clone())?;
                let validation = match admission.as_ref() {
                    Some(admission) => {
                        session
                            .validate_nested_spine_trim(admission.outer_exec_call_id(), &request)
                            .await
                    }
                    None => session.validate_spine_trim(&call_id, &request).await,
                };
                match validation {
                    Ok(()) => {
                        complete_nested(
                            admission,
                            true,
                            SpineToolResponse::Trim.success_carrier(),
                        )?;
                        SpineToolResponse::Trim
                    }
                    Err(error) => {
                        complete_nested(admission, false, error.clone())?;
                        return Err(FunctionCallError::RespondToModel(error));
                    }
                }
            }
        };

        Ok(boxed_tool_output(response_tool.success()))
    }

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
}

impl CoreToolRuntime for SpineHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        matches!(self.kind, SpineHandlerKind::Spawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_required_arguments() {
        assert_eq!(non_empty(" task ".to_string(), SPINE_OPEN).unwrap(), "task");
        assert!(non_empty(" \n".to_string(), SPINE_CLOSE).is_err());
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
    }

    #[test]
    fn tool_names_are_namespaced() {
        let handlers = SpineHandler::all();
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
    fn all_spine_tools_are_direct_and_code_mode() {
        let handlers = SpineHandler::all();
        assert!(
            handlers
                .iter()
                .all(|handler| handler.exposure() == ToolExposure::DirectAndCodeMode)
        );
        assert!(
            handlers
                .iter()
                .all(SpineHandler::supports_parallel_tool_calls)
        );
        assert_eq!(
            SpineHandler::trim().exposure(),
            ToolExposure::DirectAndCodeMode
        );
        assert_eq!(
            SpineHandler::spawn().exposure(),
            ToolExposure::DirectAndCodeMode
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

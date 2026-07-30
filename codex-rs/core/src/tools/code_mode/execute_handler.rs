use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::handle_runtime_response;
use super::is_exec_tool_name;

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<ToolSpec>,
    waits_for_spine_cancellation: bool,
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(spec: ToolSpec, nested_tool_specs: Vec<ToolSpec>) -> Self {
        let waits_for_spine_cancellation = nested_tool_specs
            .iter()
            .any(|tool| tool.name() == spine_core::SPINE_NAMESPACE);
        Self {
            spec,
            nested_tool_specs,
            waits_for_spine_cancellation,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        code: String,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let exec = ExecContext { session, turn };
        let enabled_tools =
            codex_tools::collect_code_mode_tool_definitions(&self.nested_tool_specs);
        let started_at = std::time::Instant::now();
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute(codex_code_mode::ExecuteRequest {
                tool_call_id: call_id.clone(),
                enabled_tools,
                source: args.code.clone(),
                yield_time_ms: args.yield_time_ms,
                max_output_tokens: args.max_output_tokens,
            })
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        exec.session
            .services
            .code_mode_service
            .register_cell(&cell_id, &call_id);
        let runtime_cell_id = cell_id.to_string();
        let code_cell_trace = exec
            .session
            .services
            .rollout_thread_trace
            .start_code_cell_trace(
                exec.turn.sub_id.as_str(),
                runtime_cell_id.as_str(),
                call_id.as_str(),
                args.code.as_str(),
            );
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id);
        let response = match tokio::select! {
            response = started_cell.initial_response() => response,
            _ = cancellation_token.cancelled() => {
                let _ = exec
                    .session
                    .services
                    .code_mode_service
                    .terminate(cell_id.clone())
                    .await;
                exec.session
                    .services
                    .code_mode_service
                    .abort_cell_dispatch(&cell_id)
                    .await;
                return Err(FunctionCallError::RespondToModel(
                    "Code Mode exec was cancelled".to_string(),
                ));
            }
        } {
            Ok(response) => response,
            Err(error) => {
                exec.session
                    .services
                    .code_mode_service
                    .abort_cell_dispatch(&cell_id)
                    .await;
                return Err(FunctionCallError::RespondToModel(error));
            }
        };
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        code_cell_trace.record_initial_response(&response);
        if matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            let wait_for_cleanup = exec
                .session
                .services
                .code_mode_service
                .wait_for_cell_cleanup(&cell_id);
            tokio::pin!(wait_for_cleanup);
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    let _ = exec
                        .session
                        .services
                        .code_mode_service
                        .terminate(cell_id.clone())
                        .await;
                    wait_for_cleanup.await;
                    exec.session
                        .services
                        .code_mode_service
                        .abort_cell_dispatch(&cell_id)
                        .await;
                    return Err(FunctionCallError::RespondToModel(
                        "Code Mode exec was cancelled".to_string(),
                    ));
                }
                () = &mut wait_for_cleanup => {}
            }
        }
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        if !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            code_cell_trace.record_ended(&response);
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id)
                .await;
        }
        exec.session.services.elicitations.wait_until_clear().await;
        handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
            .await
            .map_err(FunctionCallError::RespondToModel)
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeExecuteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeExecuteHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            cancellation_token,
            ..
        } = invocation;

        match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(session, turn, call_id, input, cancellation_token)
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        self.waits_for_spine_cancellation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn runtime_cancellation_waiting_is_scoped_to_spine_tools() {
        let exec_spec =
            super::super::execute_spec::create_code_mode_tool(&[], &BTreeMap::new(), true, false);
        let base = CodeModeExecuteHandler::new(exec_spec.clone(), Vec::new());
        assert!(!base.waits_for_runtime_cancellation());

        let with_spine = CodeModeExecuteHandler::new(
            exec_spec,
            vec![crate::tools::handlers::spine_spec::create_spine_tool(
                crate::tools::handlers::spine_spec::SPINE_OPEN,
            )],
        );
        assert!(with_spine.waits_for_runtime_cancellation());
    }
}

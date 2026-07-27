use crate::function_tool::FunctionCallError;
use crate::image_preparation::prepare_function_call_output_body;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
// Spine MODIFIED: Import generic output and carrier protocol model types.
// Reason: Bridged results preserve visible output while embedding nested Spine calls.
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::Value as JsonValue;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::handle_runtime_response;
use super::is_exec_tool_name;
use super::spine_bridge::CODE_MODE_SPINE_CARRIER_MARKER;
use super::spine_bridge::CodeModeOutputCarrierV1;
use super::spine_bridge::encode_carrier;

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<ToolSpec>,
    // Spine MODIFIED: Record whether the nested tool surface exposes Spine.
    // Reason: Ordinary Code Mode must retain native cancellation and output behavior.
    spine_bridge_enabled: bool,
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(spec: ToolSpec, nested_tool_specs: Vec<ToolSpec>) -> Self {
        // Spine MODIFIED: Derive bridge activation from registered nested tools.
        // Reason: Tool exposure is the authoritative feature gate for nested Spine admission.
        let spine_bridge_enabled = nested_tool_specs
            .iter()
            .any(|tool| tool.name() == crate::tools::handlers::spine_spec::SPINE_NAMESPACE);
        Self {
            spec,
            nested_tool_specs,
            spine_bridge_enabled,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        code: String,
        // Spine MODIFIED: Accept runtime cancellation in the execute lifecycle.
        // Reason: A bridged cell must seal or abort pending Spine calls before returning.
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<CodeModeExecuteOutput, FunctionCallError> {
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
        // Spine MODIFIED: Register this cell as a potential outer Spine carrier.
        // Reason: Admission is valid only when the outer exec belongs to active Spine context.
        let spine_bridge_active = self.spine_bridge_enabled
            && exec
                .session
                .validate_code_mode_spine_outer_exec(&call_id)
                .await
                .is_ok();
        exec.session
            .services
            .code_mode_service
            .register_cell(&cell_id, &call_id, spine_bridge_active)
            .map_err(FunctionCallError::RespondToModel)?;
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
        // Spine MODIFIED: Abort bridge state if cancellation wins before first output.
        // Reason: This prevents late nested calls from escaping a cancelled outer execution.
        let response = match tokio::select! {
            response = started_cell.initial_response() => response,
            _ = cancellation_token.cancelled() => {
                let first_output_join = if spine_bridge_active {
                    exec.session
                        .services
                        .code_mode_service
                        .begin_first_output(&cell_id)
                        .ok()
                } else {
                    None
                };
                let _ = exec
                    .session
                    .services
                    .code_mode_service
                    .terminate(cell_id.clone())
                    .await;
                if let Some(first_output_join) = first_output_join {
                    let _ = first_output_join.finish().await;
                }
                exec.session
                    .services
                    .code_mode_service
                    .abort_cell_dispatch(&cell_id);
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
                    .abort_cell_dispatch(&cell_id);
                return Err(FunctionCallError::RespondToModel(error));
            }
        };
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        code_cell_trace.record_initial_response(&response);
        // Spine MODIFIED: Seal admitted calls before exposing the cell's first output.
        // Reason: The carrier must atomically describe every Spine call preceding that output.
        let nested_spine_calls = if spine_bridge_active {
            let first_output_join = exec
                .session
                .services
                .code_mode_service
                .begin_first_output(&cell_id)
                .map_err(FunctionCallError::RespondToModel)?;
            let finish_first_output = first_output_join.finish();
            tokio::pin!(finish_first_output);
            match tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    let _ = exec.session.services.code_mode_service.terminate(cell_id.clone()).await;
                    let settled = finish_first_output.await;
                    exec.session.services.code_mode_service.abort_cell_dispatch(&cell_id);
                    let _ = settled;
                    return Err(FunctionCallError::RespondToModel(
                        "Code Mode exec was cancelled".to_string(),
                    ));
                }
                calls = &mut finish_first_output => calls,
            } {
                Ok(calls) => calls,
                Err(error) => {
                    exec.session
                        .services
                        .code_mode_service
                        .abort_cell_dispatch(&cell_id);
                    return Err(FunctionCallError::RespondToModel(error));
                }
            }
        } else {
            Vec::new()
        };
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        if !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            code_cell_trace.record_ended(&response);
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id);
        }
        exec.session.services.elicitations.wait_until_clear().await;
        // Spine MODIFIED: Wrap output with a carrier only for an active bridge.
        // Reason: Non-Spine Code Mode responses remain byte-for-byte on the native path.
        let visible = handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        CodeModeExecuteOutput::new(
            visible,
            spine_bridge_active.then_some((cell_id, nested_spine_calls)),
        )
        .map_err(FunctionCallError::RespondToModel)
    }
}

// Spine MODIFIED: Wrap visible output with optional nested-call carrier data.
// Reason: The response item is the durable boundary consumed by Spine compilation.
pub(super) struct CodeModeExecuteOutput {
    visible: FunctionToolOutput,
    carrier_body: Option<String>,
}

impl CodeModeExecuteOutput {
    pub(super) fn new(
        visible: FunctionToolOutput,
        carrier: Option<(
            codex_code_mode::CellId,
            Vec<super::spine_bridge::NestedSpineCallV1>,
        )>,
    ) -> Result<Self, String> {
        let carrier_body = carrier
            .map(|(cell_id, nested_spine_calls)| {
                let mut visible_body = function_output_body(&visible.body);
                prepare_function_call_output_body(&mut visible_body);
                let carrier = CodeModeOutputCarrierV1::new(
                    visible_body,
                    visible.success,
                    cell_id.to_string(),
                    nested_spine_calls,
                )?;
                encode_carrier(&carrier)
            })
            .transpose()?;
        Ok(Self {
            visible,
            carrier_body,
        })
    }
}

impl ToolOutput for CodeModeExecuteOutput {
    fn log_preview(&self) -> String {
        self.visible.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.visible.success_for_logging()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let Some(carrier_body) = &self.carrier_body else {
            return self.visible.to_response_item(call_id, payload);
        };
        debug_assert!(matches!(payload, ToolPayload::Custom { .. }));
        ResponseInputItem::CustomToolCallOutput {
            call_id: call_id.to_string(),
            name: Some(CODE_MODE_SPINE_CARRIER_MARKER.to_string()),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(carrier_body.clone()),
                success: self.visible.success,
            },
        }
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<JsonValue> {
        self.visible.post_tool_use_response(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        self.visible.code_mode_result(payload)
    }
}

fn function_output_body(items: &[FunctionCallOutputContentItem]) -> FunctionCallOutputBody {
    match items {
        [FunctionCallOutputContentItem::InputText { text }] => {
            FunctionCallOutputBody::Text(text.clone())
        }
        _ => FunctionCallOutputBody::ContentItems(items.to_vec()),
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
            // Spine MODIFIED: Forward the runtime token into bridged execution.
            // Reason: Cell and Spine cleanup must share the same cancellation signal.
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

    // Spine MODIFIED: Await runtime cancellation only for a Spine bridge.
    // Reason: Ordinary Code Mode keeps the native fast cancellation path.
    fn waits_for_runtime_cancellation(&self) -> bool {
        self.spine_bridge_enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn runtime_cancellation_waiting_is_scoped_to_the_spine_bridge() {
        let exec_spec =
            super::super::execute_spec::create_code_mode_tool(&[], &BTreeMap::new(), true, false);
        let base = CodeModeExecuteHandler::new(exec_spec.clone(), Vec::new());
        assert!(!base.waits_for_runtime_cancellation());

        let bridged = CodeModeExecuteHandler::new(
            exec_spec,
            vec![crate::tools::handlers::spine_spec::create_spine_tool(
                crate::tools::handlers::spine_spec::SPINE_OPEN,
            )],
        );
        assert!(bridged.waits_for_runtime_cancellation());
    }
}

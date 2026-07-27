use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::call_nested_tool;
use super::spine_bridge::CellFirstOutputJoin;
use super::spine_bridge::CellSpineState;
use super::spine_bridge::NestedSpineAdmission;
use super::spine_bridge::NestedSpineCallV1;
use super::spine_bridge::NestedSpineToolName;
use crate::session::step_context::StepContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

#[derive(Default)]
struct DispatchRegistry {
    cells: HashMap<CellId, Arc<CellDispatchState>>,
    closed_cells: HashSet<CellId>,
    retired_order: VecDeque<CellId>,
}

type DispatchGates = Mutex<DispatchRegistry>;
const MAX_RETIRED_CELL_TOMBSTONES: usize = 1024;

struct CellDispatchState {
    ready: watch::Sender<bool>,
    spine: Arc<CellSpineState>,
}

impl CellDispatchState {
    fn new() -> Self {
        Self {
            ready: watch::channel(false).0,
            spine: Arc::new(CellSpineState::default()),
        }
    }
}

pub(crate) struct FirstOutputJoin {
    cell_id: CellId,
    state: Arc<CellDispatchState>,
    dispatch_gates: Arc<DispatchGates>,
    inner: CellFirstOutputJoin,
}

impl FirstOutputJoin {
    pub(crate) async fn finish(self) -> Result<Vec<NestedSpineCallV1>, String> {
        let calls = self.inner.finish().await?;
        remove_dispatch_state_if_complete(&self.dispatch_gates, &self.cell_id, &self.state);
        Ok(calls)
    }
}

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    dispatch_gates: Arc<DispatchGates>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new() -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::unbounded();
        Self {
            dispatch_tx,
            dispatch_rx,
            dispatch_gates: Arc::new(Mutex::new(DispatchRegistry::default())),
        }
    }

    pub(super) fn mark_cell_ready_for_dispatch(&self, cell_id: &CellId) {
        if let Some(state) = dispatch_state(&self.dispatch_gates, cell_id) {
            state.ready.send_replace(true);
        }
    }

    pub(super) fn register_cell(
        &self,
        cell_id: &CellId,
        outer_exec_call_id: &str,
        spine_admission_enabled: bool,
    ) -> Result<(), String> {
        let (state, runtime_closed) = register_dispatch_state(&self.dispatch_gates, cell_id);
        state
            .spine
            .register_outer_exec(outer_exec_call_id, spine_admission_enabled)?;
        if runtime_closed {
            state.spine.mark_runtime_closed();
            remove_dispatch_state_if_complete(&self.dispatch_gates, cell_id, &state);
        }
        Ok(())
    }

    pub(super) fn admit_spine(
        &self,
        cell_id: &CellId,
        runtime_call_id: String,
        name: NestedSpineToolName,
        arguments: String,
    ) -> Result<NestedSpineAdmission, String> {
        let state = dispatch_state(&self.dispatch_gates, cell_id)
            .ok_or_else(|| format!("Code Mode cell `{cell_id}` is closed"))?;
        state.spine.admit(runtime_call_id, name, arguments)
    }

    pub(super) fn begin_first_output(&self, cell_id: &CellId) -> Result<FirstOutputJoin, String> {
        let state = find_dispatch_state(&self.dispatch_gates, cell_id)
            .ok_or_else(|| format!("Code Mode cell `{cell_id}` is not registered"))?;
        let inner = state.spine.begin_first_output()?;
        Ok(FirstOutputJoin {
            cell_id: cell_id.clone(),
            state,
            dispatch_gates: Arc::clone(&self.dispatch_gates),
            inner,
        })
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        let Some(state) = mark_dispatch_state_closed(&self.dispatch_gates, cell_id) else {
            return;
        };
        state.spine.mark_runtime_closed();
        remove_dispatch_state_if_complete(&self.dispatch_gates, cell_id, &state);
    }

    pub(super) fn abort_cell(&self, cell_id: &CellId) {
        let mut registry = lock_dispatch_registry(&self.dispatch_gates);
        registry.closed_cells.insert(cell_id.clone());
        if registry.cells.remove(cell_id).is_some() {
            remember_retired_cell(&mut registry, cell_id.clone());
        }
    }

    pub(super) fn is_waiting_for_first_output(&self, outer_exec_call_id: &str) -> bool {
        let states = lock_dispatch_registry(&self.dispatch_gates)
            .cells
            .values()
            .cloned()
            .collect::<Vec<_>>();
        states
            .iter()
            .any(|state| state.spine.is_waiting_for_first_output(outer_exec_call_id))
    }

    pub(super) fn start_turn_worker(
        &self,
        exec: ExecContext,
        router: Arc<ToolRouter>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> CodeModeDispatchWorker {
        let tool_runtime =
            ToolCallRuntime::new(router, Arc::clone(&exec.session), step_context, tracker);
        let host = Arc::new(CoreTurnHost { exec, tool_runtime });
        let dispatch_rx = self.dispatch_rx.clone();
        let dispatch_gates = Arc::clone(&self.dispatch_gates);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = &mut shutdown_rx => break,
                    message = dispatch_rx.recv() => message.ok(),
                };
                let Some(message) = message else {
                    break;
                };
                match message {
                    DispatchMessage::Notify {
                        call_id,
                        cell_id,
                        text,
                        cancellation_token,
                        response_tx,
                    } => {
                        let response = if wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            host.notify(call_id, cell_id, text).await
                        } else {
                            Err("code mode notification cancelled".to_string())
                        };
                        let _ = response_tx.send(response);
                    }
                    DispatchMessage::InvokeTool {
                        invocation,
                        cancellation_token,
                        response_tx,
                    } => {
                        let cell_id = invocation.cell_id.clone();
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            continue;
                        }
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            // ToolCallRuntime owns cancellation and lets tools such as
                            // spine.spawn finish cooperative teardown before returning.
                            let response = host.invoke_tool(invocation, cancellation_token).await;
                            let _ = response_tx.send(response);
                        });
                    }
                }
            }
        });
        CodeModeDispatchWorker {
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

fn dispatch_state(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
) -> Option<Arc<CellDispatchState>> {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    if registry.closed_cells.contains(cell_id) {
        return None;
    }
    Some(
        registry
            .cells
            .entry(cell_id.clone())
            .or_insert_with(|| Arc::new(CellDispatchState::new()))
            .clone(),
    )
}

fn register_dispatch_state(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
) -> (Arc<CellDispatchState>, bool) {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    let runtime_closed = registry.closed_cells.contains(cell_id);
    let state = registry
        .cells
        .entry(cell_id.clone())
        .or_insert_with(|| Arc::new(CellDispatchState::new()))
        .clone();
    (state, runtime_closed)
}

fn find_dispatch_state(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
) -> Option<Arc<CellDispatchState>> {
    lock_dispatch_registry(dispatch_gates)
        .cells
        .get(cell_id)
        .cloned()
}

fn mark_dispatch_state_closed(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
) -> Option<Arc<CellDispatchState>> {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    registry.closed_cells.insert(cell_id.clone());
    registry.cells.get(cell_id).cloned()
}

fn remove_dispatch_state_if_complete(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
    state: &Arc<CellDispatchState>,
) {
    if !state.spine.lifecycle_complete() {
        return;
    }
    let mut registry = lock_dispatch_registry(dispatch_gates);
    if registry
        .cells
        .get(cell_id)
        .is_some_and(|current| Arc::ptr_eq(current, state))
    {
        registry.cells.remove(cell_id);
        remember_retired_cell(&mut registry, cell_id.clone());
    }
}

fn remember_retired_cell(registry: &mut DispatchRegistry, cell_id: CellId) {
    registry.retired_order.push_back(cell_id);
    while registry.retired_order.len() > MAX_RETIRED_CELL_TOMBSTONES {
        if let Some(expired) = registry.retired_order.pop_front() {
            registry.closed_cells.remove(&expired);
        }
    }
}

fn lock_dispatch_registry(
    dispatch_gates: &DispatchGates,
) -> std::sync::MutexGuard<'_, DispatchRegistry> {
    match dispatch_gates.lock() {
        Ok(registry) => registry,
        Err(poisoned) => poisoned.into_inner(),
    }
}

async fn wait_until_cell_ready_for_dispatch(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let Some(state) = dispatch_state(dispatch_gates, cell_id) else {
        return false;
    };
    let mut ready_rx = state.ready.subscribe();
    loop {
        if *ready_rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = cancellation_token.cancelled() => return false,
        }
    }
}

impl CodeModeSessionDelegate for CodeModeDispatchBroker {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.close_cell(cell_id);
    }
}

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
}

pub(crate) struct CodeModeDispatchWorker {
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for CodeModeDispatchWorker {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

struct CoreTurnHost {
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
}

impl CoreTurnHost {
    async fn invoke_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        call_nested_tool(
            self.exec.clone(),
            self.tool_runtime.clone(),
            invocation,
            cancellation_token,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn notify(&self, call_id: String, cell_id: CellId, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.exec
            .session
            .inject_if_running(vec![ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some(PUBLIC_TOOL_NAME.to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            }])
            .await
            .map_err(|_| {
                format!("failed to inject exec notify message for cell {cell_id}: no active turn")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn early_runtime_close_is_preserved_until_first_output_seals() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-early-close".to_string());

        broker.close_cell(&cell_id);
        broker
            .register_cell(&cell_id, "exec-1", true)
            .expect("register after close");
        assert!(find_dispatch_state(&broker.dispatch_gates, &cell_id).is_some());

        assert!(
            broker
                .begin_first_output(&cell_id)
                .expect("begin first output")
                .finish()
                .await
                .expect("finish first output")
                .is_empty()
        );
        assert!(find_dispatch_state(&broker.dispatch_gates, &cell_id).is_none());
        assert!(
            broker
                .admit_spine(
                    &cell_id,
                    "runtime-1".to_string(),
                    NestedSpineToolName::Trim,
                    "{}".to_string(),
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn completed_cell_tombstones_are_bounded() {
        let broker = CodeModeDispatchBroker::new();
        for ordinal in 0..(MAX_RETIRED_CELL_TOMBSTONES + 16) {
            let cell_id = CellId::new(format!("cell-retired-{ordinal}"));
            broker
                .register_cell(&cell_id, &format!("exec-{ordinal}"), true)
                .expect("register cell");
            broker.close_cell(&cell_id);
            assert!(
                broker
                    .begin_first_output(&cell_id)
                    .expect("begin first output")
                    .finish()
                    .await
                    .expect("finish first output")
                    .is_empty()
            );
        }

        let registry = lock_dispatch_registry(&broker.dispatch_gates);
        assert!(registry.cells.is_empty());
        assert_eq!(registry.closed_cells.len(), MAX_RETIRED_CELL_TOMBSTONES);
        assert_eq!(registry.retired_order.len(), MAX_RETIRED_CELL_TOMBSTONES);
    }

    #[test]
    fn disabled_bridge_cell_closes_without_waiting_for_first_output_seal() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-disabled-bridge".to_string());
        broker
            .register_cell(&cell_id, "exec-1", false)
            .expect("register cell");

        broker.close_cell(&cell_id);

        assert!(find_dispatch_state(&broker.dispatch_gates, &cell_id).is_none());
    }

    #[test]
    fn abort_removes_cell_state_and_rejects_late_dispatch() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-abort".to_string());
        broker
            .register_cell(&cell_id, "exec-1", true)
            .expect("register cell");

        broker.abort_cell(&cell_id);

        assert!(find_dispatch_state(&broker.dispatch_gates, &cell_id).is_none());
        assert!(dispatch_state(&broker.dispatch_gates, &cell_id).is_none());
    }
}

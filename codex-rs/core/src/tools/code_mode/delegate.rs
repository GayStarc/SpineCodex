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
use crate::session::step_context::StepContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

#[derive(Default)]
struct DispatchRegistry {
    cells: HashMap<CellId, CellDispatchState>,
    closed_cells: HashSet<CellId>,
    retired_order: VecDeque<CellId>,
}

type DispatchGates = Mutex<DispatchRegistry>;
const MAX_RETIRED_CELL_TOMBSTONES: usize = 1024;

struct CellDispatchState {
    ready: watch::Sender<bool>,
    outer_call_id: Option<String>,
    invocations: watch::Sender<InvocationCounts>,
    next_spine_invocation: u64,
    spine_invocations: HashMap<String, u64>,
    spine_sealed: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct InvocationCounts {
    pending: usize,
    active: usize,
    cleanup: usize,
    spine: usize,
}

impl InvocationCounts {
    fn is_idle(self) -> bool {
        self.pending == 0 && self.active == 0
    }
}

enum InvocationChange {
    Start { cleanup: bool },
    CancelPending { spine: bool },
    Finish { cleanup: bool, spine: bool },
}

impl CellDispatchState {
    fn new() -> Self {
        Self {
            ready: watch::channel(false).0,
            outer_call_id: None,
            invocations: watch::channel(InvocationCounts::default()).0,
            next_spine_invocation: 0,
            spine_invocations: HashMap::new(),
            spine_sealed: false,
        }
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
        if let Some(ready) = dispatch_state(&self.dispatch_gates, cell_id) {
            ready.send_replace(true);
        }
    }

    pub(super) fn register_cell(&self, cell_id: &CellId, outer_call_id: &str) {
        let mut registry = lock_dispatch_registry(&self.dispatch_gates);
        if registry.closed_cells.contains(cell_id) {
            return;
        }
        let state = registry
            .cells
            .entry(cell_id.clone())
            .or_insert_with(CellDispatchState::new);
        state
            .outer_call_id
            .get_or_insert_with(|| outer_call_id.to_string());
    }

    pub(super) fn outer_call_id(&self, cell_id: &CellId) -> Option<String> {
        lock_dispatch_registry(&self.dispatch_gates)
            .cells
            .get(cell_id)
            .and_then(|state| state.outer_call_id.clone())
    }

    pub(super) fn spine_invocation_ordinal(
        &self,
        cell_id: &CellId,
        runtime_tool_call_id: &str,
    ) -> Option<u64> {
        lock_dispatch_registry(&self.dispatch_gates)
            .cells
            .get(cell_id)
            .and_then(|state| state.spine_invocations.get(runtime_tool_call_id).copied())
    }

    pub(super) async fn close_cell_and_wait(&self, cell_id: &CellId) {
        wait_for_cell_invocations(&self.dispatch_gates, cell_id).await;
    }

    pub(super) async fn close_cell_and_wait_for_spine(&self, cell_id: &CellId) {
        mark_dispatch_state_closed(&self.dispatch_gates, cell_id);
        wait_for_invocations(&self.dispatch_gates, cell_id, |counts| counts.spine == 0).await;
    }

    pub(super) async fn wait_for_cleanup_invocations(&self, cell_id: &CellId) {
        seal_spine_dispatch(&self.dispatch_gates, cell_id);
        wait_for_invocations(&self.dispatch_gates, cell_id, |counts| {
            counts.pending == 0 && counts.cleanup == 0 && counts.spine == 0
        })
        .await;
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
                        let spine = is_spine_invocation(&invocation);
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            update_invocations(
                                &dispatch_gates,
                                &cell_id,
                                InvocationChange::CancelPending { spine },
                            );
                            continue;
                        }
                        let waits_for_cleanup = host
                            .tool_runtime
                            .tool_name_waits_for_runtime_cancellation(&invocation.tool_name);
                        let start = InvocationChange::Start {
                            cleanup: waits_for_cleanup,
                        };
                        if !update_invocations(&dispatch_gates, &cell_id, start) {
                            update_invocations(
                                &dispatch_gates,
                                &cell_id,
                                InvocationChange::CancelPending { spine },
                            );
                            let _ = response_tx
                                .send(Err(format!("Code Mode cell `{cell_id}` is closed")));
                            continue;
                        }
                        let host = Arc::clone(&host);
                        let guard = ActiveInvocationGuard {
                            dispatch_gates: Arc::clone(&dispatch_gates),
                            cell_id,
                            cleanup: waits_for_cleanup,
                            spine,
                        };
                        tokio::spawn(async move {
                            let _guard = guard;
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

fn dispatch_state(dispatch_gates: &DispatchGates, cell_id: &CellId) -> Option<watch::Sender<bool>> {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    if registry.closed_cells.contains(cell_id) {
        return None;
    }
    Some(
        registry
            .cells
            .entry(cell_id.clone())
            .or_insert_with(CellDispatchState::new)
            .ready
            .clone(),
    )
}

fn mark_dispatch_state_closed(dispatch_gates: &DispatchGates, cell_id: &CellId) {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    if !registry.closed_cells.insert(cell_id.clone()) {
        return;
    }
    let is_idle = registry
        .cells
        .get(cell_id)
        .is_none_or(|state| state.invocations.borrow().is_idle());
    retire_if_idle(&mut registry, cell_id, is_idle);
}

fn update_invocations(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
    change: InvocationChange,
) -> bool {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    if matches!(change, InvocationChange::Start { .. }) && registry.closed_cells.contains(cell_id) {
        return false;
    }
    let Some(state) = registry.cells.get(cell_id) else {
        return false;
    };
    let mut counts = *state.invocations.borrow();
    match change {
        InvocationChange::Start { cleanup } => {
            counts.pending = counts.pending.saturating_sub(1);
            counts.active = counts.active.saturating_add(1);
            if cleanup {
                counts.cleanup = counts.cleanup.saturating_add(1);
            }
        }
        InvocationChange::CancelPending { spine } => {
            counts.pending = counts.pending.saturating_sub(1);
            if spine {
                counts.spine = counts.spine.saturating_sub(1);
            }
        }
        InvocationChange::Finish { cleanup, spine } => {
            counts.active = counts.active.saturating_sub(1);
            if cleanup {
                counts.cleanup = counts.cleanup.saturating_sub(1);
            }
            if spine {
                counts.spine = counts.spine.saturating_sub(1);
            }
        }
    }
    state.invocations.send_replace(counts);
    retire_if_idle(&mut registry, cell_id, counts.is_idle());
    true
}

fn queue_invocation(
    dispatch_gates: &DispatchGates,
    invocation: &CodeModeNestedToolCall,
) -> Result<(), String> {
    let mut registry = lock_dispatch_registry(dispatch_gates);
    let cell_id = &invocation.cell_id;
    if registry.closed_cells.contains(cell_id) {
        return Err(format!("Code Mode cell `{cell_id}` is closed"));
    }
    let state = registry
        .cells
        .entry(cell_id.clone())
        .or_insert_with(CellDispatchState::new);
    let spine = is_spine_invocation(invocation);
    if spine {
        if state.spine_sealed {
            return Err(format!(
                "Code Mode cell `{cell_id}` no longer accepts Spine calls"
            ));
        }
        if state
            .spine_invocations
            .contains_key(&invocation.runtime_tool_call_id)
        {
            return Err(format!(
                "Code Mode runtime tool call `{}` was queued more than once",
                invocation.runtime_tool_call_id
            ));
        }
        let next = state
            .next_spine_invocation
            .checked_add(1)
            .ok_or_else(|| "Code Mode Spine invocation ordinal overflow".to_string())?;
        state.spine_invocations.insert(
            invocation.runtime_tool_call_id.clone(),
            state.next_spine_invocation,
        );
        state.next_spine_invocation = next;
    }
    let mut counts = *state.invocations.borrow();
    counts.pending = counts.pending.saturating_add(1);
    if spine {
        counts.spine = counts.spine.saturating_add(1);
    }
    state.invocations.send_replace(counts);
    Ok(())
}

fn seal_spine_dispatch(dispatch_gates: &DispatchGates, cell_id: &CellId) {
    if let Some(state) = lock_dispatch_registry(dispatch_gates)
        .cells
        .get_mut(cell_id)
    {
        state.spine_sealed = true;
    }
}

fn is_spine_invocation(invocation: &CodeModeNestedToolCall) -> bool {
    invocation.tool_name.namespace.as_deref() == Some(spine_core::SPINE_NAMESPACE)
        && spine_core::SpineTool::all()
            .iter()
            .any(|tool| tool.name() == invocation.tool_name.name)
}

struct ActiveInvocationGuard {
    dispatch_gates: Arc<DispatchGates>,
    cell_id: CellId,
    cleanup: bool,
    spine: bool,
}

impl Drop for ActiveInvocationGuard {
    fn drop(&mut self) {
        update_invocations(
            &self.dispatch_gates,
            &self.cell_id,
            InvocationChange::Finish {
                cleanup: self.cleanup,
                spine: self.spine,
            },
        );
    }
}

async fn wait_for_cell_invocations(dispatch_gates: &DispatchGates, cell_id: &CellId) {
    mark_dispatch_state_closed(dispatch_gates, cell_id);
    wait_for_invocations(dispatch_gates, cell_id, InvocationCounts::is_idle).await;
    let mut registry = lock_dispatch_registry(dispatch_gates);
    let is_idle = registry
        .cells
        .get(cell_id)
        .is_none_or(|state| state.invocations.borrow().is_idle());
    retire_if_idle(&mut registry, cell_id, is_idle);
}

async fn wait_for_invocations(
    dispatch_gates: &DispatchGates,
    cell_id: &CellId,
    complete: impl Fn(InvocationCounts) -> bool,
) {
    let Some(mut invocations_rx) = ({
        let registry = lock_dispatch_registry(dispatch_gates);
        registry
            .cells
            .get(cell_id)
            .map(|state| state.invocations.subscribe())
    }) else {
        return;
    };
    while !complete(*invocations_rx.borrow_and_update()) {
        if invocations_rx.changed().await.is_err() {
            break;
        }
    }
}

fn retire_if_idle(registry: &mut DispatchRegistry, cell_id: &CellId, is_idle: bool) {
    if is_idle && registry.closed_cells.contains(cell_id) {
        registry.cells.remove(cell_id);
        remember_retired_cell(registry, cell_id.clone());
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
    let Some(ready) = dispatch_state(dispatch_gates, cell_id) else {
        return false;
    };
    let mut ready_rx = ready.subscribe();
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
            queue_invocation(&self.dispatch_gates, &invocation)?;
            let spine = is_spine_invocation(&invocation);
            let cell_id = invocation.cell_id.clone();
            let (response_tx, response_rx) = oneshot::channel();
            if self
                .dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .is_err()
            {
                update_invocations(
                    &self.dispatch_gates,
                    &cell_id,
                    InvocationChange::CancelPending { spine },
                );
                return Err("code mode nested tool dispatcher is unavailable".to_string());
            }
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
        mark_dispatch_state_closed(&self.dispatch_gates, cell_id);
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

    #[test]
    fn early_runtime_close_rejects_late_registration() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-early-close".to_string());

        mark_dispatch_state_closed(&broker.dispatch_gates, &cell_id);
        broker.register_cell(&cell_id, "exec-early-close");

        let registry = lock_dispatch_registry(&broker.dispatch_gates);
        assert!(!registry.cells.contains_key(&cell_id));
        assert!(registry.closed_cells.contains(&cell_id));
    }

    #[test]
    fn completed_cell_tombstones_are_bounded() {
        let broker = CodeModeDispatchBroker::new();
        for ordinal in 0..(MAX_RETIRED_CELL_TOMBSTONES + 16) {
            let cell_id = CellId::new(format!("cell-retired-{ordinal}"));
            broker.register_cell(&cell_id, &format!("exec-{ordinal}"));
            mark_dispatch_state_closed(&broker.dispatch_gates, &cell_id);
        }

        let registry = lock_dispatch_registry(&broker.dispatch_gates);
        assert!(registry.cells.is_empty());
        assert_eq!(registry.closed_cells.len(), MAX_RETIRED_CELL_TOMBSTONES);
        assert_eq!(registry.retired_order.len(), MAX_RETIRED_CELL_TOMBSTONES);
    }

    #[test]
    fn completed_cell_closes_dispatch_immediately() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-complete".to_string());
        broker.register_cell(&cell_id, "exec-complete");

        mark_dispatch_state_closed(&broker.dispatch_gates, &cell_id);

        let registry = lock_dispatch_registry(&broker.dispatch_gates);
        assert!(!registry.cells.contains_key(&cell_id));
    }

    #[test]
    fn spine_invocation_ordinals_follow_queue_order() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-ordinals".to_string());
        broker.register_cell(&cell_id, "exec-ordinals");
        let invocation = |runtime_tool_call_id: &str| CodeModeNestedToolCall {
            cell_id: cell_id.clone(),
            runtime_tool_call_id: runtime_tool_call_id.to_string(),
            tool_name: codex_protocol::ToolName::namespaced("spine", "trim"),
            tool_kind: codex_code_mode::CodeModeToolKind::Function,
            input: None,
        };

        queue_invocation(&broker.dispatch_gates, &invocation("tool-1")).unwrap();
        queue_invocation(&broker.dispatch_gates, &invocation("tool-2")).unwrap();

        assert_eq!(broker.spine_invocation_ordinal(&cell_id, "tool-1"), Some(0));
        assert_eq!(broker.spine_invocation_ordinal(&cell_id, "tool-2"), Some(1));
    }

    #[test]
    fn spine_invocation_can_arrive_before_cell_registration() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-early-invocation".to_string());
        let invocation = CodeModeNestedToolCall {
            cell_id: cell_id.clone(),
            runtime_tool_call_id: "tool-early".to_string(),
            tool_name: codex_protocol::ToolName::namespaced("spine", "open"),
            tool_kind: codex_code_mode::CodeModeToolKind::Function,
            input: None,
        };

        queue_invocation(&broker.dispatch_gates, &invocation).unwrap();
        broker.register_cell(&cell_id, "exec-early-invocation");

        assert_eq!(
            broker.spine_invocation_ordinal(&cell_id, "tool-early"),
            Some(0)
        );
        assert_eq!(
            broker.outer_call_id(&cell_id).as_deref(),
            Some("exec-early-invocation")
        );
    }

    #[test]
    fn sealed_cell_rejects_late_spine_invocation() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-sealed-spine".to_string());
        broker.register_cell(&cell_id, "exec-sealed-spine");
        seal_spine_dispatch(&broker.dispatch_gates, &cell_id);
        let invocation = CodeModeNestedToolCall {
            cell_id,
            runtime_tool_call_id: "tool-late".to_string(),
            tool_name: codex_protocol::ToolName::namespaced("spine", "open"),
            tool_kind: codex_code_mode::CodeModeToolKind::Function,
            input: None,
        };

        assert!(queue_invocation(&broker.dispatch_gates, &invocation).is_err());
    }

    #[tokio::test]
    async fn cleanup_wait_includes_active_spine_invocations() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-spine-wait".to_string());
        broker.register_cell(&cell_id, "exec-spine-wait");
        let invocation = CodeModeNestedToolCall {
            cell_id: cell_id.clone(),
            runtime_tool_call_id: "tool-1".to_string(),
            tool_name: codex_protocol::ToolName::namespaced("spine", "open"),
            tool_kind: codex_code_mode::CodeModeToolKind::Function,
            input: None,
        };
        queue_invocation(&broker.dispatch_gates, &invocation).unwrap();
        assert!(update_invocations(
            &broker.dispatch_gates,
            &cell_id,
            InvocationChange::Start { cleanup: false },
        ));
        let wait = broker.wait_for_cleanup_invocations(&cell_id);
        tokio::pin!(wait);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut wait)
                .await
                .is_err()
        );
        assert!(update_invocations(
            &broker.dispatch_gates,
            &cell_id,
            InvocationChange::Finish {
                cleanup: false,
                spine: true,
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut wait)
            .await
            .expect("Spine invocation completion must release cleanup wait");
    }

    #[test]
    fn closed_cell_state_rejects_late_dispatch() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-abort".to_string());
        broker.register_cell(&cell_id, "exec-abort");

        mark_dispatch_state_closed(&broker.dispatch_gates, &cell_id);

        assert!(
            !lock_dispatch_registry(&broker.dispatch_gates)
                .cells
                .contains_key(&cell_id)
        );
        assert!(dispatch_state(&broker.dispatch_gates, &cell_id).is_none());
    }
}

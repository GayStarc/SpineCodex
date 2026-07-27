use std::sync::Arc;

use crate::agents_md::LoadedAgentsMd;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::McpRuntimeSnapshot;
use crate::session::turn_context::TurnContext;
// Spine MODIFIED: Import the per-response Spine spawn admission group.
// Reason: Tool calls from one model response must be validated as one transaction.
use crate::spine::spawn::SpineSpawnGroup;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::ToolInfo;
use tokio::sync::OnceCell;

/// Request-scoped state that may change between model sampling requests.
#[derive(Debug)]
pub(crate) struct StepContext {
    pub(crate) turn: Arc<TurnContext>,
    pub(crate) environments: TurnEnvironmentSnapshot,
    /// Capability roots bound to ready environments in this exact step.
    pub(crate) selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
    /// The exact MCP config and manager used to advertise and execute tools for this step.
    pub(crate) mcp: Arc<McpRuntimeSnapshot>,
    /// The fixed MCP tool list used for this exact sampling request.
    mcp_tool_snapshot: OnceCell<Vec<ToolInfo>>,
    /// The canonical AGENTS.md value observed with this environment snapshot.
    pub(crate) loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
    // Spine MODIFIED: Share one spawn admission boundary across all calls in a response.
    // Reason: Spine must see sibling open/close/next calls before admitting any spawn.
    /// All model tool calls in this sampling response share one admission boundary.
    pub(crate) spine_spawn_group: Arc<SpineSpawnGroup>,
}

impl StepContext {
    pub(crate) fn new(
        turn: Arc<TurnContext>,
        environments: TurnEnvironmentSnapshot,
        selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
        mcp: Arc<McpRuntimeSnapshot>,
        loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
    ) -> Self {
        Self {
            turn,
            environments,
            selected_capability_roots,
            mcp,
            mcp_tool_snapshot: OnceCell::new(),
            loaded_agents_md,
            // Spine MODIFIED: Start each response step with an isolated admission group.
            // Reason: Call discovery and completion synchronization must not cross sampling steps.
            spine_spawn_group: Arc::new(SpineSpawnGroup::default()),
        }
    }

    pub(crate) async fn mcp_tools(&self) -> &[ToolInfo] {
        self.mcp_tool_snapshot
            .get_or_init(|| self.mcp.manager().list_all_tools())
            .await
    }
}

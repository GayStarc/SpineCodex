use super::*;
use std::collections::HashSet;

impl AgentControl {
    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread.codex.session.ensure_rollout_materialized().await;
            thread.codex.session.flush_rollout().await?;
            let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state.send_op(agent_id, Op::Shutdown {}).await
            };
            thread.wait_until_terminated().await;
            result
        } else {
            state.send_op(agent_id, Op::Shutdown {}).await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.forget_v2_residency(agent_id);
        self.state.release_spawned_thread(agent_id);
        result
    }

    async fn shutdown_live_agent_for_spine_spawn(
        &self,
        state: &Arc<ThreadManagerState>,
        agent_id: ThreadId,
    ) -> CodexResult<()> {
        let mut failures = Vec::new();
        match state.get_thread(agent_id).await {
            Ok(thread) => {
                thread.codex.session.ensure_rollout_materialized().await;
                if let Err(error) = thread.codex.session.flush_rollout().await {
                    failures.push(error.to_string());
                }
                if !matches!(thread.agent_status().await, AgentStatus::Shutdown)
                    && let Err(error) = state.send_op(agent_id, Op::Shutdown {}).await
                {
                    failures.push(error.to_string());
                }
                thread.wait_until_terminated().await;
            }
            Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {}
            Err(error) => failures.push(error.to_string()),
        }
        let _ = state.remove_thread(&agent_id).await;
        self.forget_v2_residency(agent_id);
        self.state.release_spawned_thread(agent_id);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CodexErr::Fatal(format!(
                "agent {agent_id} shutdown completed with errors: {}",
                failures.join("; ")
            )))
        }
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let known_agent = self.state.agent_metadata_for_thread(agent_id).is_some();
        match state.get_thread(agent_id).await {
            Ok(thread) => {
                if !thread.config_snapshot().await.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
            }
            Err(CodexErr::ThreadNotFound(_)) if known_agent => {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
            }
            Err(CodexErr::ThreadNotFound(_)) => {}
            Err(err) => {
                warn!("failed to inspect agent before close {agent_id}: {err}");
            }
        }
        match Box::pin(self.shutdown_agent_tree(agent_id)).await {
            Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) if known_agent => {
                Ok(String::new())
            }
            result => result,
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }

    /// Stop every live agent in the supplied Spawn thread subtrees, including pathless Native
    /// descendants and agents created while teardown is already in progress.
    pub(crate) async fn shutdown_spine_spawn_subtrees(
        &self,
        roots: &[ThreadId],
    ) -> CodexResult<()> {
        // Linearize transaction settlement after every in-flight native spawn/resume topology
        // writer and before any later writer. Explicit Native resume remains available after this
        // guard is released.
        let _settlement = self.state.begin_spine_spawn_settlement().await;
        let state = self.upgrade()?;
        let mut failures = Vec::new();
        let mut transaction_threads = HashSet::new();
        loop {
            for root in roots {
                transaction_threads.insert(*root);
                if let Some(agent_graph_store) = state.agent_graph_store() {
                    match agent_graph_store
                        .list_thread_spawn_descendants(*root, /*status_filter*/ None)
                        .await
                    {
                        Ok(descendants) => transaction_threads.extend(descendants),
                        Err(error) => failures.push(format!(
                            "{root}: failed to load persisted spawn descendants: {error}"
                        )),
                    }
                }
                transaction_threads.extend(self.live_thread_spawn_descendants(*root).await?);
            }

            // Closing is deliberately repeated on every fixed-point round. A descendant can
            // become visible in the live tree before its Open edge finishes persisting, so a
            // one-shot status update could be overwritten while teardown is still draining it.
            let mut edge_close_failed = false;
            if let Some(agent_graph_store) = state.agent_graph_store() {
                let mut current_threads = transaction_threads.iter().copied().collect::<Vec<_>>();
                current_threads.sort_by_key(std::string::ToString::to_string);
                for thread_id in current_threads {
                    if let Err(error) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            thread_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                    {
                        edge_close_failed = true;
                        failures.push(format!(
                            "{thread_id}: failed to close persisted spawn edge: {error}"
                        ));
                    }
                }
            }

            let mut live = Vec::new();
            for thread_id in &transaction_threads {
                if state.get_thread(*thread_id).await.is_ok() {
                    live.push(*thread_id);
                }
            }
            let had_live_threads = !live.is_empty();
            live.sort_by_key(std::string::ToString::to_string);
            for thread_id in live {
                match self
                    .shutdown_live_agent_for_spine_spawn(&state, thread_id)
                    .await
                {
                    Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {
                    }
                    Err(error) => failures.push(format!("{thread_id}: {error}")),
                }
            }
            if had_live_threads {
                continue;
            }

            // The live tree is quiescent. Require the persisted transaction subtree to be
            // quiescent as well before the Spawn receipt can return.
            if let Some(agent_graph_store) = state.agent_graph_store() {
                let mut open_descendants = HashSet::new();
                let mut current_threads = transaction_threads.iter().copied().collect::<Vec<_>>();
                current_threads.sort_by_key(std::string::ToString::to_string);
                for thread_id in current_threads {
                    match agent_graph_store
                        .list_thread_spawn_children(
                            thread_id,
                            Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                        )
                        .await
                    {
                        Ok(children) => open_descendants.extend(children),
                        Err(error) => failures.push(format!(
                            "{thread_id}: failed to verify persisted spawn children: {error}"
                        )),
                    }
                }
                if !open_descendants.is_empty() {
                    transaction_threads.extend(open_descendants);
                    if edge_close_failed {
                        break;
                    }
                    continue;
                }
            }
            break;
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CodexErr::Fatal(format!(
                "spine.spawn subtree shutdown completed with errors: {}",
                failures.join("; ")
            )))
        }
    }
}

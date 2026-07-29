use super::*;

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

    /// Stop every live agent in the supplied Spawn path subtrees, including agents created
    /// while teardown is already in progress.
    pub(crate) async fn shutdown_spine_spawn_subtrees(
        &self,
        roots: &[AgentPath],
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let mut failures = Vec::new();
        loop {
            let mut live = self
                .state
                .live_agents()
                .into_iter()
                .filter_map(|metadata| {
                    let path = metadata.agent_path?;
                    let thread_id = metadata.agent_id?;
                    roots
                        .iter()
                        .any(|root| path_is_in_subtree(&path, root))
                        .then_some((thread_id, path))
                })
                .collect::<Vec<_>>();
            if live.is_empty() {
                break;
            }
            live.sort_by_key(|(_, path)| {
                path.as_str().bytes().filter(|byte| *byte == b'/').count()
            });
            for (thread_id, _) in live {
                match self
                    .shutdown_live_agent_for_spine_spawn(&state, thread_id)
                    .await
                {
                    Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {
                    }
                    Err(error) => failures.push(format!("{thread_id}: {error}")),
                }
            }
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

fn path_is_in_subtree(candidate: &AgentPath, root: &AgentPath) -> bool {
    candidate == root
        || candidate
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtree_membership_requires_a_segment_boundary() {
        let root = AgentPath::try_from("/root/branch").unwrap();
        assert!(path_is_in_subtree(&root, &root));
        assert!(path_is_in_subtree(
            &AgentPath::try_from("/root/branch/worker").unwrap(),
            &root,
        ));
        assert!(!path_is_in_subtree(
            &AgentPath::try_from("/root/branch_other").unwrap(),
            &root,
        ));
    }
}

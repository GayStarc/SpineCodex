use super::*;

impl App {
    pub(super) fn upsert_spine_tree_cell(
        &mut self,
        tui: &mut tui::Tui,
        turn_id: String,
        snapshot: codex_app_server_protocol::SpineTreeUpdatedNotification,
    ) -> Result<()> {
        match spine_tree_upsert_action(
            self.transcript_cells.last(),
            &turn_id,
            snapshot.snapshot_seq,
        ) {
            SpineTreeUpsertAction::Replace => {
                let cell: Arc<dyn HistoryCell> =
                    Arc::new(history_cell::new_spine_tree_update(turn_id, snapshot));
                if let Some(last) = self.transcript_cells.last_mut() {
                    *last = cell;
                }
                if let Some(Overlay::Transcript(transcript)) = &mut self.overlay {
                    transcript.replace_cells(self.transcript_cells.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.finish_required_stream_reflow(tui)?;
            }
            SpineTreeUpsertAction::Insert => {
                self.insert_history_cell(
                    tui,
                    Box::new(history_cell::new_spine_tree_update(turn_id, snapshot)),
                );
            }
            SpineTreeUpsertAction::Ignore => {}
        }
        Ok(())
    }

    pub(super) fn upsert_spine_spawn_progress_cell(
        &mut self,
        tui: &mut tui::Tui,
        notification: codex_app_server_protocol::SpineSpawnProgressUpdatedNotification,
    ) -> Result<()> {
        let cell: Arc<dyn HistoryCell> = Arc::new(history_cell::SpineSpawnProgressCell::new(
            notification.clone(),
        ));
        let replace = self.transcript_cells.last().is_some_and(|last| {
            last.as_any()
                .downcast_ref::<history_cell::SpineSpawnProgressCell>()
                .is_some_and(|previous| {
                    previous.turn_id() == notification.turn_id
                        && previous.call_id() == notification.call_id
                })
        });
        if replace {
            if let Some(last) = self.transcript_cells.last_mut() {
                *last = cell;
            }
        } else {
            self.insert_history_cell(
                tui,
                Box::new(history_cell::SpineSpawnProgressCell::new(notification)),
            );
        }
        if let Some(Overlay::Transcript(transcript)) = &mut self.overlay {
            transcript.replace_cells(self.transcript_cells.clone());
            tui.frame_requester().schedule_frame();
        }
        self.finish_required_stream_reflow(tui)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpineTreeUpsertAction {
    Replace,
    Insert,
    Ignore,
}

fn spine_tree_upsert_action(
    last_cell: Option<&Arc<dyn HistoryCell>>,
    turn_id: &str,
    snapshot_seq: u64,
) -> SpineTreeUpsertAction {
    let Some(last_spine_tree) = last_cell.and_then(|cell| {
        cell.as_any()
            .downcast_ref::<history_cell::SpineTreeUpdateCell>()
    }) else {
        if let Some(last_spawn) = last_cell.and_then(|cell| {
            cell.as_any()
                .downcast_ref::<history_cell::SpineSpawnProgressCell>()
        }) {
            return (last_spawn.turn_id() == turn_id)
                .then_some(SpineTreeUpsertAction::Replace)
                .unwrap_or(SpineTreeUpsertAction::Insert);
        }
        return SpineTreeUpsertAction::Insert;
    };
    if !last_spine_tree.is_live_update() || last_spine_tree.turn_id() != turn_id {
        return SpineTreeUpsertAction::Insert;
    }
    if last_spine_tree.snapshot_seq() <= snapshot_seq {
        SpineTreeUpsertAction::Replace
    } else {
        SpineTreeUpsertAction::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::SpineTreeUpdatedNotification;

    fn live_cell(turn_id: &str, snapshot_seq: u64) -> Arc<dyn HistoryCell> {
        Arc::new(history_cell::new_spine_tree_update(
            turn_id.to_string(),
            snapshot(turn_id, snapshot_seq),
        ))
    }

    fn snapshot(turn_id: &str, snapshot_seq: u64) -> SpineTreeUpdatedNotification {
        SpineTreeUpdatedNotification {
            thread_id: "thread".to_string(),
            turn_id: turn_id.to_string(),
            snapshot_seq,
            active_node_id: "1".to_string(),
            nodes: Vec::new(),
        }
    }

    #[test]
    fn live_spine_tree_upsert_replaces_only_non_stale_same_turn_snapshot() {
        let last = live_cell("turn-1", 4);

        assert_eq!(
            spine_tree_upsert_action(Some(&last), "turn-1", 5),
            SpineTreeUpsertAction::Replace
        );
        assert_eq!(
            spine_tree_upsert_action(Some(&last), "turn-1", 3),
            SpineTreeUpsertAction::Ignore
        );
        assert_eq!(
            spine_tree_upsert_action(Some(&last), "turn-2", 5),
            SpineTreeUpsertAction::Insert
        );

        let manual: Arc<dyn HistoryCell> =
            Arc::new(history_cell::new_spine_tree_snapshot(snapshot("turn-1", 4)));
        assert_eq!(
            spine_tree_upsert_action(Some(&manual), "turn-1", 5),
            SpineTreeUpsertAction::Insert
        );
    }
}

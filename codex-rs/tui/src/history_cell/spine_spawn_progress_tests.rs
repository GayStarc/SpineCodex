use super::plain_lines;
use super::spine_spawn_progress::SPAWN_ACTIVITY_WORDS;
use super::spine_spawn_progress::SpineSpawnOverlay;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SpineSpawnProgressUpdatedNotification;
use codex_app_server_protocol::SpineSpawnTaskProgress;
use codex_app_server_protocol::ThreadItem;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use std::collections::HashSet;

#[test]
fn renders_live_mixed_child_statuses() {
    let cell = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: vec![
            SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "inspect native events".to_string(),
                thread_id: "child-0".to_string(),
                agent_path: Some("/root/inspector".to_string()),
                status: CollabAgentStatus::Completed,
            },
            SpineSpawnTaskProgress {
                ordinal: 1,
                summary: "verify cancellation".to_string(),
                thread_id: "child-1".to_string(),
                agent_path: Some("/root/verifier".to_string()),
                status: CollabAgentStatus::Running,
            },
        ],
    });

    let completed_word = cell
        .activity_word("child-0")
        .expect("completed child should have an activity word");
    let running_word = cell
        .activity_word("child-1")
        .expect("running child should have an activity word");
    assert_ne!(completed_word, running_word);
    let rendered = plain_lines(cell.display_lines("  │  ", true, 80, false))
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains("spine.spawn"), "{rendered}");
    assert!(rendered.contains(&format!("├ ✓ {completed_word} inspect native events")));
    assert!(rendered.contains(&format!("└ • {running_word} verify cancellation")));
    assert!(rendered.contains("Waiting for activity..."));
    assert_eq!(cell.display_lines("  │  ", true, 80, false).len(), 7);

    let lines = cell.display_lines("  │  ", true, 80, false);
    for task_line in [&lines[0], &lines[1]] {
        assert!(
            task_line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM),
            "task branch should use the tree prefix style: {task_line:?}"
        );
        let summary = task_line
            .spans
            .last()
            .expect("task line should end with its summary");
        assert!(
            !summary.style.add_modifier.contains(Modifier::DIM),
            "task summary should use the normal foreground: {task_line:?}"
        );
    }
    for span in &lines[1].spans[1..4] {
        if !span.content.trim().is_empty() {
            assert_eq!(
                span.style.fg,
                Some(Color::Green),
                "running marker and activity word should be green: {lines:?}"
            );
        }
    }
    for activity_line in &lines[2..6] {
        assert!(
            activity_line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM),
            "activity branch should use the tree prefix style: {activity_line:?}"
        );
    }
}

#[test]
fn activity_refresh_keeps_the_newest_four_lines() {
    let mut overlay = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: vec![SpineSpawnTaskProgress {
            ordinal: 0,
            summary: "inspect events".to_string(),
            thread_id: "child".to_string(),
            agent_path: Some("/root/inspector".to_string()),
            status: CollabAgentStatus::Running,
        }],
    });
    let notifications = (1..=5).map(|index| {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: format!("message-{index}"),
                text: format!("activity {index}"),
                phase: None,
                memory_citation: None,
            },
            thread_id: "child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: index,
        })
    });
    assert!(overlay.seed_activity("child", notifications));

    let rendered = plain_lines(overlay.display_lines("  ", true, 80, false))
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains("activity 1"));
    assert!(rendered.contains("activity 2\n"));
    assert!(rendered.contains("activity 3\n"));
    assert!(rendered.contains("activity 4\n"));
    assert!(rendered.contains("activity 5\n"));
    assert_eq!(overlay.display_lines("  ", true, 80, false).len(), 6);
}

#[test]
fn terminal_tasks_render_without_an_aggregate_row() {
    let cell = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: vec![
            SpineSpawnTaskProgress {
                ordinal: 0,
                summary: "completed".to_string(),
                thread_id: "child-0".to_string(),
                agent_path: None,
                status: CollabAgentStatus::Completed,
            },
            SpineSpawnTaskProgress {
                ordinal: 1,
                summary: "interrupted".to_string(),
                thread_id: "child-1".to_string(),
                agent_path: None,
                status: CollabAgentStatus::Interrupted,
            },
            SpineSpawnTaskProgress {
                ordinal: 2,
                summary: "failed".to_string(),
                thread_id: "child-2".to_string(),
                agent_path: None,
                status: CollabAgentStatus::Errored,
            },
            SpineSpawnTaskProgress {
                ordinal: 3,
                summary: "stopped".to_string(),
                thread_id: "child-3".to_string(),
                agent_path: None,
                status: CollabAgentStatus::Shutdown,
            },
        ],
    });
    let rendered = plain_lines(cell.display_lines("  ", true, 80, false))
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains("spine.spawn"), "{rendered}");
    for (thread_id, marker, summary) in [
        ("child-0", "✓", "completed"),
        ("child-1", "!", "interrupted"),
        ("child-2", "×", "failed"),
        ("child-3", "×", "stopped"),
    ] {
        let word = cell
            .activity_word(thread_id)
            .expect("terminal child should retain its activity word");
        assert!(
            rendered.contains(&format!("{marker} {word} {summary}")),
            "{rendered}"
        );
    }
}

#[test]
fn pending_task_uses_a_pending_specific_empty_state() {
    let cell = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: vec![SpineSpawnTaskProgress {
            ordinal: 0,
            summary: "waiting child".to_string(),
            thread_id: "child".to_string(),
            agent_path: None,
            status: CollabAgentStatus::PendingInit,
        }],
    });

    let rendered = plain_lines(cell.display_lines("  ", true, 80, false))
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Waiting to start..."), "{rendered}");
    assert!(!rendered.contains("Waiting for activity..."), "{rendered}");
}

#[test]
fn narrow_width_preserves_tree_prefixes_and_fixed_activity_rows() {
    let mut overlay = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: vec![SpineSpawnTaskProgress {
            ordinal: 0,
            summary: "a deliberately long task summary that needs wrapping".to_string(),
            thread_id: "child".to_string(),
            agent_path: Some("/root/worker".to_string()),
            status: CollabAgentStatus::Running,
        }],
    });
    let notifications = (1..=4).map(|index| {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: format!("message-{index}"),
                text: format!("activity {index} with a long description"),
                phase: None,
                memory_citation: None,
            },
            thread_id: "child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: index,
        })
    });
    overlay.seed_activity("child", notifications);
    let lines = overlay.display_lines("  ", false, 36, false);
    assert!(lines.iter().all(|line| line.width() <= 36));
    let activity_rows = &lines[lines.len() - 5..lines.len() - 1];
    assert_eq!(activity_rows.len(), 4);
    assert!(
        activity_rows
            .iter()
            .all(|line| line.to_string().starts_with("  │    "))
    );
    assert_eq!(lines.last().map(Line::to_string).as_deref(), Some("  │"));
}

#[test]
fn random_activity_words_are_unique_within_a_spawn_and_stable_across_refresh() {
    let tasks = (0..6)
        .map(|ordinal| SpineSpawnTaskProgress {
            ordinal,
            summary: format!("task {ordinal}"),
            thread_id: format!("child-{ordinal}"),
            agent_path: None,
            status: CollabAgentStatus::Running,
        })
        .collect::<Vec<_>>();
    let mut overlay = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: tasks.clone(),
    });
    let before = tasks
        .iter()
        .map(|task| {
            overlay
                .activity_word(&task.thread_id)
                .expect("each child should receive an activity word")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        before.iter().cloned().collect::<HashSet<_>>().len(),
        tasks.len()
    );
    assert!(
        before
            .iter()
            .all(|word| SPAWN_ACTIVITY_WORDS.contains(&word.as_str()))
    );

    let mut refreshed_tasks = tasks;
    refreshed_tasks.reverse();
    refreshed_tasks[0].status = CollabAgentStatus::Completed;
    overlay.replace_notification(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: refreshed_tasks,
    });
    for (ordinal, word) in before.into_iter().enumerate() {
        assert_eq!(
            overlay.activity_word(&format!("child-{ordinal}")),
            Some(word.as_str())
        );
    }
}

#[test]
fn activity_words_remain_unique_beyond_the_base_pool() {
    let task_count = SPAWN_ACTIVITY_WORDS.len() + 4;
    let tasks = (0..task_count)
        .map(|ordinal| SpineSpawnTaskProgress {
            ordinal: ordinal as u32,
            summary: format!("task {ordinal}"),
            thread_id: format!("child-{ordinal}"),
            agent_path: None,
            status: CollabAgentStatus::Running,
        })
        .collect::<Vec<_>>();
    let overlay = SpineSpawnOverlay::new(SpineSpawnProgressUpdatedNotification {
        thread_id: "parent".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "spawn-1".to_string(),
        tasks: tasks.clone(),
    });

    let words = tasks
        .iter()
        .map(|task| {
            overlay
                .activity_word(&task.thread_id)
                .expect("each child should receive an activity word")
        })
        .collect::<HashSet<_>>();
    assert_eq!(words.len(), task_count);
    assert!(words.iter().any(|word| word.starts_with("Further ")));
}

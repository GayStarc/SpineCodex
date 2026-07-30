use super::*;
use pretty_assertions::assert_eq;
use spine_core::SPINE_SPAWN_RESULT_SCHEMA;
use spine_core::SpawnOutcome;
use spine_core::SpawnResult;

#[test]
fn task_arguments_require_two_exact_non_empty_tasks() {
    let tasks = parse_tasks(
        r#"{"tasks":[{"summary":"one","prompt":"first"},{"summary":" two ","prompt":" second "}]}"#,
    )
    .unwrap();
    assert_eq!(
        tasks,
        vec![
            spine_core::SpawnTask {
                summary: "one".to_string(),
                prompt: "first".to_string(),
            },
            spine_core::SpawnTask {
                summary: " two ".to_string(),
                prompt: " second ".to_string(),
            },
        ]
    );

    for arguments in [
        r#"{"tasks":[]}"#,
        r#"{"tasks":[{"summary":"one","prompt":"first"}]}"#,
        r#"{"tasks":[{"summary":" ","prompt":"first"},{"summary":"two","prompt":"second"}]}"#,
        r#"{"tasks":[{"summary":"one","prompt":""},{"summary":"two","prompt":"second"}]}"#,
        r#"{"tasks":[{"summary":"one","prompt":"first","extra":true},{"summary":"two","prompt":"second"}]}"#,
        r#"{"tasks":[{"summary":"one","prompt":"first"},{"summary":"two","prompt":"second"}],"extra":true}"#,
        r#"{"tasks":[{"summary":"one","prompt":"first"},{"summary":" one ","prompt":"second"}]}"#,
    ] {
        assert!(parse_tasks(arguments).is_err(), "accepted {arguments}");
    }
}

#[test]
fn task_envelope_injects_identity_and_same_call_peer_roster() {
    let tasks = vec![
        SpawnTask {
            summary: "parser".to_string(),
            prompt: concat!(
                "Shared collaboration directory: tasks/trial/collab\n",
                "My collaboration file: tasks/trial/collab/parser.md\n",
                "Implement parser."
            )
            .to_string(),
        },
        SpawnTask {
            summary: "compatibility tests".to_string(),
            prompt: concat!(
                "Shared collaboration directory: tasks/trial/collab\n",
                "My collaboration file: tasks/trial/collab/compatibility-tests.md\n",
                "Test compatibility."
            )
            .to_string(),
        },
        SpawnTask {
            summary: "interface review".to_string(),
            prompt: concat!(
                "Shared collaboration directory: tasks/trial/collab\n",
                "My collaboration file: tasks/trial/collab/interface-review.md\n",
                "Review the interface."
            )
            .to_string(),
        },
    ];

    let envelope = task_envelope(&tasks[0], &tasks);

    assert!(envelope.contains("You are: parser"));
    assert!(envelope.contains("- compatibility tests\n- interface review"));
    assert!(envelope.contains("tasks/trial/collab/parser.md"));
    assert!(envelope.ends_with(&format!("Assignment:\n{}", tasks[0].prompt)));
}

fn register_spawn(group: &SpineSpawnGroup, call_id: &str) {
    group.register(
        call_id,
        "spine.spawn",
        r#"{"tasks":[{"summary":"one","prompt":"first"},{"summary":"two","prompt":"second"}]}"#,
    );
}

#[test]
fn exact_receipt_codec_preserves_all_semantic_fields() {
    let receipt = SpawnReceipt {
        schema: SPINE_SPAWN_RESULT_SCHEMA.to_string(),
        results: vec![SpawnResult {
            ordinal: 0,
            outcome: SpawnOutcome::Errored,
            memory_body: "truthful memory".to_string(),
            diagnostic: Some("child error".to_string()),
            execution_ref: Some("child-ref".to_string()),
        }],
    };

    assert_eq!(
        SpawnReceipt::decode_json(&receipt.encode_json().unwrap()).unwrap(),
        receipt
    );
    assert!(
        SpawnReceipt::decode_json(
            r#"{"schema":"spine.spawn.result.v1","results":[],"extra":true}"#
        )
        .is_err()
    );
}

#[test]
fn coordinator_helpers_keep_safe_names_and_truthful_terminal_results() {
    assert_eq!(transaction_task_name("Call-ID.42", 3), "spawn_callid42_3");

    let thread_id = codex_protocol::ThreadId::new();
    let completed = result_from_status(
        0,
        thread_id,
        AgentStatus::Completed(Some("final memory".to_string())),
        None,
    );
    assert_eq!(completed.outcome, SpawnOutcome::Completed);
    assert_eq!(completed.memory_body, "final memory");
    assert_eq!(completed.diagnostic, None);

    let missing = result_from_status(1, thread_id, AgentStatus::Completed(None), None);
    assert_eq!(missing.outcome, SpawnOutcome::Errored);
    assert!(missing.diagnostic.is_some());
    assert!(!missing.memory_body.trim().is_empty());

    assert!(is_spawn_terminal(&AgentStatus::Interrupted));
    let interrupted = result_from_status(2, thread_id, AgentStatus::Interrupted, None);
    assert_eq!(interrupted.outcome, SpawnOutcome::Aborted);

    let salvaged = result_from_status(
        3,
        thread_id,
        AgentStatus::Completed(None),
        Some(crate::spine::spawn_salvage::SpawnFailureRecord {
            diagnostic: "upstream 503".to_string(),
            salvaged_memory: Some("progress before failure".to_string()),
        }),
    );
    assert_eq!(salvaged.outcome, SpawnOutcome::Errored);
    assert_eq!(salvaged.memory_body, "progress before failure");
    assert_eq!(
        salvaged.diagnostic.as_deref(),
        Some("child errored: upstream 503")
    );

    let salvage_failed = result_from_status(
        4,
        thread_id,
        AgentStatus::Errored("upstream 503".to_string()),
        Some(crate::spine::spawn_salvage::SpawnFailureRecord {
            diagnostic: "upstream 503".to_string(),
            salvaged_memory: None,
        }),
    );
    assert_eq!(salvage_failed.outcome, SpawnOutcome::Errored);
    assert_eq!(
        salvage_failed.memory_body,
        salvage_failed.diagnostic.expect("original diagnostic")
    );
}

#[test]
fn subtree_membership_uses_agent_path_segment_boundaries() {
    let root = AgentPath::try_from("/root/spawn_a").unwrap();
    assert!(path_is_in_subtree(&root, &root));
    assert!(path_is_in_subtree(
        &AgentPath::try_from("/root/spawn_a/worker").unwrap(),
        &root,
    ));
    assert!(path_is_in_subtree(
        &AgentPath::try_from("/root/spawn_a/worker/deep").unwrap(),
        &root,
    ));
    assert!(!path_is_in_subtree(
        &AgentPath::try_from("/root/spawn_a2").unwrap(),
        &root,
    ));
    assert!(!path_is_in_subtree(
        &AgentPath::try_from("/root/other/spawn_a").unwrap(),
        &root,
    ));
}

#[test]
fn abort_barrier_blocks_new_admission_without_owning_transaction_cleanup() {
    let lifecycle = SpawnLifecycle::default();
    let transaction = lifecycle
        .try_enter(CancellationToken::new())
        .expect("first Spawn may enter");
    let abort_barrier = lifecycle.begin_abort();

    assert!(abort_barrier.had_active_transactions());
    assert!(lifecycle.try_enter(CancellationToken::new()).is_none());
    drop(transaction);
    assert!(lifecycle.try_enter(CancellationToken::new()).is_none());

    drop(abort_barrier);
    assert!(lifecycle.try_enter(CancellationToken::new()).is_some());
}

#[test]
fn initial_progress_normalizes_fast_terminal_statuses() {
    let thread_id = codex_protocol::ThreadId::new();
    assert_eq!(
        normalized_progress_status(0, thread_id, AgentStatus::Running),
        AgentStatus::Running
    );
    for status in [
        AgentStatus::Completed(None),
        AgentStatus::Completed(Some("  ".to_string())),
    ] {
        assert!(matches!(
            normalized_progress_status(0, thread_id, status),
            AgentStatus::Errored(message)
                if message.contains("non-empty final memory")
        ));
    }
    assert!(matches!(
        normalized_progress_status(
            0,
            thread_id,
            AgentStatus::Completed(Some("memory".to_string())),
        ),
        AgentStatus::Completed(None)
    ));
}

#[test]
fn terminal_status_matrix_produces_one_total_ordered_receipt() {
    let tasks = (0..4)
        .map(|ordinal| spine_core::SpawnTask {
            summary: format!("task {ordinal}"),
            prompt: format!("prompt {ordinal}"),
        })
        .collect::<Vec<_>>();
    let statuses = [
        AgentStatus::Completed(Some("completed memory".to_string())),
        AgentStatus::Completed(None),
        AgentStatus::Errored("provider failure".to_string()),
        AgentStatus::Shutdown,
    ];
    let normalized = statuses
        .into_iter()
        .enumerate()
        .map(|(ordinal, status)| {
            let result = result_from_status(ordinal, codex_protocol::ThreadId::new(), status, None);
            let progress_status = result_status(&result);
            (Some(result), progress_status)
        })
        .collect::<Vec<_>>();
    let progress_statuses = normalized
        .iter()
        .map(|(_, status)| status.clone())
        .collect::<Vec<_>>();
    let results = normalized.into_iter().map(|(result, _)| result).collect();

    let receipt = finish_receipt(&tasks, results).expect("terminal matrix must be total");
    assert_eq!(
        receipt
            .results
            .iter()
            .map(|result| (result.ordinal, result.outcome))
            .collect::<Vec<_>>(),
        vec![
            (0, SpawnOutcome::Completed),
            (1, SpawnOutcome::Errored),
            (2, SpawnOutcome::Errored),
            (3, SpawnOutcome::Aborted),
        ]
    );
    assert_eq!(receipt.results[0].diagnostic, None);
    assert!(matches!(
        progress_statuses.as_slice(),
        [
            AgentStatus::Completed(None),
            AgentStatus::Errored(missing),
            AgentStatus::Errored(error),
            AgentStatus::Shutdown,
        ] if missing.contains("non-empty final memory")
            && error.contains("provider failure")
    ));
    assert!(receipt.results[1..].iter().all(|result| {
        !result.memory_body.trim().is_empty()
            && result
                .diagnostic
                .as_deref()
                .is_some_and(|diagnostic| !diagnostic.trim().is_empty())
    }));
}

#[test]
fn partial_start_failure_is_total_and_keeps_input_ordinals() {
    let paths = vec![
        codex_protocol::AgentPath::try_from("/root/spawn_0").unwrap(),
        codex_protocol::AgentPath::try_from("/root/spawn_1").unwrap(),
        codex_protocol::AgentPath::try_from("/root/spawn_2").unwrap(),
    ];
    let first = codex_protocol::ThreadId::new();
    let third = codex_protocol::ThreadId::new();
    let StartPhase {
        live,
        mut results,
        failed,
    } = classify_start_results(
        &paths,
        [Ok(first), Err("injected start failure"), Ok(third)],
    );

    assert!(failed);
    assert_eq!(
        live.iter()
            .map(|(ordinal, thread_id, _)| (*ordinal, *thread_id))
            .collect::<Vec<_>>(),
        vec![(0, first), (2, third)]
    );
    for (ordinal, thread_id, _) in live {
        results[ordinal] = Some(error_result(
            ordinal,
            SpawnOutcome::Aborted,
            "child aborted because another transaction child failed to start".to_string(),
            Some(thread_id.to_string()),
        ));
    }
    let tasks = vec![
        spine_core::SpawnTask {
            summary: "zero".to_string(),
            prompt: "zero task".to_string(),
        },
        spine_core::SpawnTask {
            summary: "one".to_string(),
            prompt: "one task".to_string(),
        },
        spine_core::SpawnTask {
            summary: "two".to_string(),
            prompt: "two task".to_string(),
        },
    ];
    let receipt = finish_receipt(&tasks, results).unwrap();
    assert_eq!(
        receipt
            .results
            .iter()
            .map(|result| (result.ordinal, result.outcome))
            .collect::<Vec<_>>(),
        vec![
            (0, SpawnOutcome::Aborted),
            (1, SpawnOutcome::Errored),
            (2, SpawnOutcome::Aborted),
        ]
    );
    assert!(
        receipt.results[1]
            .diagnostic
            .as_deref()
            .is_some_and(|text| text.contains("injected start failure"))
    );
}

#[test]
fn batch_receipts_partition_flat_results_and_restore_task_ordinals() {
    let calls = vec![
        SpawnBatchCall {
            call_id: "spawn-1".to_string(),
            fork_parent_call_id: "spawn-1".to_string(),
            tasks: parse_tasks(
                r#"{"tasks":[{"summary":"a","prompt":"pa"},{"summary":"b","prompt":"pb"}]}"#,
            )
            .unwrap(),
        },
        SpawnBatchCall {
            call_id: "spawn-2".to_string(),
            fork_parent_call_id: "spawn-2".to_string(),
            tasks: parse_tasks(
                r#"{"tasks":[{"summary":"c","prompt":"pc"},{"summary":"d","prompt":"pd"}]}"#,
            )
            .unwrap(),
        },
    ];
    let results = (0..4)
        .map(|ordinal| {
            Some(SpawnResult {
                ordinal,
                outcome: SpawnOutcome::Completed,
                memory_body: format!("memory-{ordinal}"),
                diagnostic: None,
                execution_ref: None,
            })
        })
        .collect();

    let receipts = finish_batch_receipts(&calls, results).unwrap();
    assert_eq!(
        receipts["spawn-1"]
            .results
            .iter()
            .map(|result| result.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        receipts["spawn-2"]
            .results
            .iter()
            .map(|result| (result.ordinal, result.memory_body.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "memory-2"), (1, "memory-3")]
    );
}

#[test]
fn capacity_rejection_partitions_multiple_calls_without_losing_task_identity() {
    let calls = vec![
        SpawnBatchCall {
            call_id: "spawn-1".to_string(),
            fork_parent_call_id: "spawn-1".to_string(),
            tasks: parse_tasks(
                r#"{"tasks":[{"summary":"a","prompt":"pa"},{"summary":"b","prompt":"pb"}]}"#,
            )
            .unwrap(),
        },
        SpawnBatchCall {
            call_id: "spawn-2".to_string(),
            fork_parent_call_id: "spawn-2".to_string(),
            tasks: parse_tasks(
                r#"{"tasks":[{"summary":"c","prompt":"pc"},{"summary":"d","prompt":"pd"}]}"#,
            )
            .unwrap(),
        },
    ];

    let receipts =
        capacity_rejection_receipts(&calls, /*task_count*/ 4, /*max_threads*/ 3)
            .expect("capacity rejection must produce complete receipts");

    for (call_ordinal, call) in calls.iter().enumerate() {
        let receipt = &receipts[&call.call_id];
        assert_eq!(receipt.results.len(), call.tasks.len());
        for (task_ordinal, (result, task)) in receipt.results.iter().zip(&call.tasks).enumerate() {
            let batch_ordinal = call_ordinal * call.tasks.len() + task_ordinal + 1;
            assert_eq!(result.ordinal, task_ordinal as u32);
            assert_eq!(result.outcome, SpawnOutcome::Errored);
            assert_eq!(result.execution_ref, None);
            let diagnostic = result.diagnostic.as_deref().unwrap();
            assert_eq!(result.memory_body, diagnostic);
            assert!(diagnostic.contains(&format!("task {batch_ordinal}/4")));
            assert!(diagnostic.contains(&format!("(`{}`)", task.summary)));
            assert!(diagnostic.contains("configured limit of 3"));
        }
    }
}

#[tokio::test]
async fn response_group_admission_accepts_spawn_call() {
    let group = SpineSpawnGroup::default();
    register_spawn(&group, "spawn");
    group.finish();

    let call = group
        .spawn_call("spawn", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(call.tasks.len(), 2);
}

#[tokio::test]
async fn response_group_admission_accepts_ordinary_sibling_calls() {
    let group = SpineSpawnGroup::default();
    register_spawn(&group, "spawn");
    group.register("shell", "shell", "{}");
    group.finish();

    group
        .spawn_call("spawn", &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn response_group_admission_rejects_multiple_spawn_calls() {
    let group = SpineSpawnGroup::default();
    register_spawn(&group, "spawn-1");
    register_spawn(&group, "spawn-2");
    group.finish();
    let error = group
        .spawn_call("spawn-2", &CancellationToken::new())
        .await
        .expect_err("multiple spine.spawn calls must be rejected before execution");
    assert_eq!(
        error,
        "spine.spawn may be called at most once in one model response"
    );
}

#[tokio::test]
async fn response_group_admission_rejects_conflicting_spine_controls() {
    for control in ["spine.open", "spine.close", "spine.next"] {
        let group = SpineSpawnGroup::default();
        register_spawn(&group, "spawn");
        group.register("control", control, "{}");
        group.finish();
        assert!(
            group
                .spawn_call("spawn", &CancellationToken::new())
                .await
                .is_err()
        );
    }
}

#[test]
fn progress_event_carries_the_exact_child_thread_id_for_each_task() {
    let tasks = vec![
        SpawnTask {
            summary: "first".to_string(),
            prompt: "one".to_string(),
        },
        SpawnTask {
            summary: "second".to_string(),
            prompt: "two".to_string(),
        },
    ];
    let thread_ids = [
        codex_protocol::ThreadId::new(),
        codex_protocol::ThreadId::new(),
    ];
    let paths = [
        AgentPath::root().join("first").unwrap(),
        AgentPath::root().join("second").unwrap(),
    ];
    let statuses = [AgentStatus::Running, AgentStatus::PendingInit];

    let event = spawn_progress_event("spawn-1", &tasks, &thread_ids, &paths, &statuses);

    assert_eq!(
        event
            .tasks
            .iter()
            .map(|task| task.thread_id)
            .collect::<Vec<_>>(),
        thread_ids
    );
}

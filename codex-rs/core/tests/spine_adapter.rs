#![allow(clippy::expect_used)]

#[path = "suite/compact_resume_fork.rs"]
mod compact_resume_fork;
#[path = "suite/spine_remote_compact.rs"]
mod spine_remote_compact;
#[path = "suite/spine_responses_lite.rs"]
mod spine_responses_lite;
#[path = "suite/spine_spawn.rs"]
mod spine_spawn;
#[path = "suite/spine_world_state.rs"]
mod spine_world_state;

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::spine_test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use spine_core::SamplingArchiveRecord;
use spine_core::SpineConfig;
use spine_core::SpineOperationFact;
use std::fs;
#[cfg(not(target_os = "windows"))]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[tokio::test]
async fn canonical_records_one_start_and_commit_per_real_stream() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("canonical-cardinality-open"),
                ev_completed("canonical-cardinality-open"),
            ]),
            sse(vec![
                ev_response_created("canonical-cardinality-final"),
                ev_completed("canonical-cardinality-final"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex().build(&server).await?;

    test.submit_turn("record the first sampling boundary")
        .await?;
    test.submit_turn("record the second sampling boundary")
        .await?;

    let requests = response_mock.requests();
    let records = load_sampling_records(&test)?;
    assert_eq!(requests.len(), 2, "sampling records: {records:#?}");
    assert_eq!(records.len(), requests.len() * 2);
    for pair in records.chunks_exact(2) {
        let SamplingArchiveRecord::SamplingStarted(started) = &pair[0] else {
            anyhow::bail!("sampling record pair must start with SamplingStarted");
        };
        let SamplingArchiveRecord::SamplingCommit(commit) = &pair[1] else {
            anyhow::bail!("sampling record pair must end with SamplingCommit");
        };
        assert_eq!(commit.attempt_id, started.attempt_id);
        assert_eq!(commit.started_record_digest, started.record_digest);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_without_effect_leaves_an_orphan_start_without_a_synthetic_commit() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let incomplete = sse(vec![ev_response_created("effect-free-incomplete")]);
    let completed = sse(vec![
        ev_response_created("effect-free-retry"),
        ev_completed("effect-free-retry"),
    ]);
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: incomplete,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: completed,
        }],
    ])
    .await;
    let mut builder = spine_test_codex().with_config(|config| {
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(1);
    });
    let test = builder.build_with_streaming_server(&server).await?;

    test.submit_turn("retry an effect-free stream").await?;

    let records = load_sampling_records(&test)?;
    let starts = records
        .iter()
        .filter_map(|record| match record {
            SamplingArchiveRecord::SamplingStarted(started) => Some(started),
            SamplingArchiveRecord::SamplingCommit(_) => None,
        })
        .collect::<Vec<_>>();
    let commits = records
        .iter()
        .filter_map(|record| match record {
            SamplingArchiveRecord::SamplingStarted(_) => None,
            SamplingArchiveRecord::SamplingCommit(commit) => Some(commit),
        })
        .collect::<Vec<_>>();
    assert_eq!(server.requests().await.len(), 2);
    assert_eq!(starts.len(), 2);
    assert_eq!(commits.len(), 1);
    assert_ne!(commits[0].attempt_id, starts[0].attempt_id);
    assert_eq!(commits[0].attempt_id, starts[1].attempt_id);

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_spine_effect_commits_the_failed_attempt_before_retrying() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let failed_after_open = sse(vec![
        ev_response_created("failed-after-open"),
        ev_function_call_with_namespace(
            "failed-after-open-call",
            "spine",
            "open",
            r#"{"summary":"durable failed-stream child"}"#,
        ),
    ]);
    let completed = sse(vec![
        ev_response_created("failed-after-open-retry"),
        ev_completed("failed-after-open-retry"),
    ]);
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: failed_after_open,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: completed,
        }],
    ])
    .await;
    let mut builder = spine_test_codex().with_config(|config| {
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(1);
    });
    let test = builder.build_with_streaming_server(&server).await?;

    test.submit_turn("preserve a Spine effect across retry")
        .await?;

    let requests = server.requests().await;
    let records = load_sampling_records(&test)?;
    let rollout = fs::read_to_string(test.codex.rollout_path().context("rollout path")?)?;
    let commits = records
        .iter()
        .filter_map(|record| match record {
            SamplingArchiveRecord::SamplingStarted(_) => None,
            SamplingArchiveRecord::SamplingCommit(commit) => Some(commit),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        2,
        "sampling records: {records:#?}\nrollout:\n{rollout}"
    );
    assert_eq!(
        commits.len(),
        2,
        "sampling records: {records:#?}\nrollout:\n{rollout}"
    );
    assert_eq!(commits[0].executions.len(), 1);
    assert!(matches!(
        commits[0].executions[0].operation,
        SpineOperationFact::Open { .. }
    ));
    assert!(commits[1].executions.is_empty());
    let retry_request: Value = serde_json::from_slice(&requests[1])?;
    assert!(
        retry_request["input"].to_string().contains("<spine_node"),
        "retry must observe the installed failed-attempt transition"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_after_spine_effect_commits_the_cancelled_attempt() -> Result<()> {
    let server = start_mock_server().await;
    #[cfg(not(target_os = "windows"))]
    let command = "sleep 60";
    #[cfg(target_os = "windows")]
    let command = "Start-Sleep -Seconds 60";
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("cancelled-after-open"),
            ev_function_call_with_namespace(
                "cancelled-open-call",
                "spine",
                "open",
                r#"{"summary":"durable cancelled child"}"#,
            ),
            ev_function_call(
                "cancelled-blocking-call",
                "shell_command",
                &json!({
                    "command": command,
                    "timeout_ms": 60_000,
                })
                .to_string(),
            ),
            ev_completed("cancelled-after-open"),
        ]),
    )
    .await;
    let test = spine_test_codex().build(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "commit the Spine effect before interrupting".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::RawResponseItem(raw)
                if matches!(
                    &raw.item,
                    codex_protocol::models::ResponseItem::FunctionCallOutput { call_id, .. }
                        if call_id == "cancelled-open-call"
                )
        )
    })
    .await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;

    let records = load_sampling_records(&test)?;
    let commits = records
        .iter()
        .filter_map(|record| match record {
            SamplingArchiveRecord::SamplingStarted(_) => None,
            SamplingArchiveRecord::SamplingCommit(commit) => Some(commit),
        })
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 1, "sampling records: {records:#?}");
    assert_eq!(commits[0].executions.len(), 1);
    assert!(matches!(
        commits[0].executions[0].operation,
        SpineOperationFact::Open { .. }
    ));

    Ok(())
}

#[tokio::test]
async fn spine_tree_delivery_precedes_corresponding_protocol_events() -> Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("spine-durable-order"),
            ev_completed_with_tokens("spine-durable-order", 100),
        ]),
    )
    .await;
    let test = spine_test_codex().build(&server).await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "durable before tree".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::RawResponseItem(raw)
                if matches!(
                    &raw.item,
                    codex_protocol::models::ResponseItem::Message { content, .. }
                        if content.iter().any(|item| {
                            matches!(item, ContentItem::InputText { text } if text == "durable before tree")
                        })
                )
        )
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::SpineTreeUpdate(_))
    })
    .await;
    let rollout = fs::read_to_string(test.codex.rollout_path().context("rollout path")?)?;
    let persisted = rollout.lines().any(|line| {
        let Ok(line) = serde_json::from_str::<RolloutLine>(line) else {
            return false;
        };
        matches!(
            line.item,
            RolloutItem::ResponseItem(codex_protocol::models::ResponseItem::Message {
                role,
                content,
                ..
            }) if role == "user" && content.iter().any(|item| {
                matches!(item, ContentItem::InputText { text } if text == "durable before tree")
            })
        )
    });
    assert!(
        persisted,
        "user input should eventually be persisted; rollout:\n{rollout}"
    );

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TokenCount(_))
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::SpineTreeUpdate(_))
    })
    .await;
    let rollout = fs::read_to_string(test.codex.rollout_path().context("rollout path")?)?;
    let token_count_persisted = rollout.lines().any(|line| {
        serde_json::from_str::<RolloutLine>(line)
            .is_ok_and(|line| matches!(line.item, RolloutItem::EventMsg(EventMsg::TokenCount(_))))
    });
    assert!(
        token_count_persisted,
        "token count should eventually be persisted; rollout:\n{rollout}"
    );

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

#[tokio::test]
async fn spine_adapter_profile_projects_anchored_input_without_status_tail() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("spine-adapter-profile"),
            ev_completed("spine-adapter-profile"),
        ]),
    )
    .await;
    let mut builder = spine_test_codex();
    let test = builder.build(&server).await?;

    assert!(test.config.features.enabled(Feature::SpineJit));
    assert!(!test.config.features.enabled(Feature::SpineTrim));
    assert!(!test.config.features.enabled(Feature::SpineSpawn));

    test.submit_turn("adapter profile probe").await?;

    let input = response_mock.single_request().input();
    let user_text = message_text(&input, "user").context("missing projected user input")?;
    assert_anchored_user_text(user_text, "adapter profile probe")?;
    assert!(
        input
            .iter()
            .all(|item| !item.to_string().contains("<spine_status ")),
        "request must not contain a Spine status developer tail"
    );

    Ok(())
}

#[tokio::test]
async fn spine_adapter_omits_status_when_feature_is_disabled() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("spine-status-disabled"),
            ev_completed("spine-status-disabled"),
        ]),
    )
    .await;
    let mut builder = spine_test_codex().with_config(|config| {
        config
            .features
            .disable(Feature::SpineStatus)
            .expect("SpineStatus should be configurable in tests");
    });
    let test = builder.build(&server).await?;

    test.submit_turn("status-off probe").await?;

    let input = response_mock.single_request().input();
    assert!(
        input
            .iter()
            .all(|item| !item.to_string().contains("<spine_status ")),
        "status-off request must not contain a Spine status developer tail"
    );

    Ok(())
}

#[tokio::test]
async fn configured_node_prompt_marks_the_open_scope_boundary() -> Result<()> {
    const NODE_PROMPT: &str = "CUSTOM NODE SCOPE GUIDANCE";
    let config = SpineConfig::parse_toml(&format!(
        r#"
schema_version = 1
[limits]
trim_threshold_bytes = 10000
[prompt]
jit = "<spine_view>test</spine_view>"
node = "{NODE_PROMPT}"
[tools.open]
description = "open"
[tools.close]
description = "close"
[tools.next]
description = "next"
"#
    ))?
    .with_feature(spine_core::Feature::Jit)?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("configured-node-open"),
                ev_function_call_with_namespace(
                    "configured-node-call",
                    "spine",
                    "open",
                    r#"{"summary":"configured child"}"#,
                ),
                ev_completed("configured-node-open"),
            ]),
            sse(vec![
                ev_response_created("configured-node-done"),
                ev_completed("configured-node-done"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex()
        .with_config(move |test_config| {
            test_config.spine_tools =
                spine_core::ToolCatalog::new(&config).expect("configured Spine tool catalog");
            test_config.spine_config = config;
        })
        .build(&server)
        .await?;

    test.submit_turn("open configured scope").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let input = requests[1].input();
    let node_index = input
        .iter()
        .position(|item| item.to_string().contains("<spine_node"))
        .context("missing projected Spine node marker")?;
    let transition_index = input
        .iter()
        .position(|item| {
            item.get("call_id").and_then(Value::as_str) == Some("configured-node-call")
        })
        .context("missing spine.open transition")?;
    let node = input[node_index].to_string();
    assert!(node.contains(NODE_PROMPT), "{node}");
    assert!(node.contains("</spine_node>"), "{node}");
    assert!(node_index < transition_index);

    Ok(())
}

#[tokio::test]
async fn spine_transition_baseline() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("baseline-open"),
                ev_function_call_with_namespace(
                    "baseline-open-call",
                    "spine",
                    "open",
                    r#"{"summary":"baseline child"}"#,
                ),
                ev_completed("baseline-open"),
            ]),
            sse(vec![
                ev_response_created("baseline-child-work"),
                ev_assistant_message("baseline-child-reaction", "baseline child-only reaction"),
                ev_function_call(
                    "baseline-child-work-call",
                    "shell_command",
                    &json!({"command": "echo baseline-child-local-tool-output"}).to_string(),
                ),
                ev_completed("baseline-child-work"),
            ]),
            sse(vec![
                ev_response_created("baseline-close"),
                ev_function_call_with_namespace(
                    "baseline-close-call",
                    "spine",
                    "close",
                    r#"{"memory":"baseline memory"}"#,
                ),
                ev_completed("baseline-close"),
            ]),
            sse(vec![
                ev_response_created("baseline-final"),
                ev_completed("baseline-final"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex().build(&server).await?;
    test.submit_turn("baseline user evidence").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    let open_input = requests[1].input();
    let node_index = open_input
        .iter()
        .position(|item| item.to_string().contains("<spine_node"))
        .context("missing node guidance")?;
    let open_index = open_input
        .iter()
        .position(|item| item.get("call_id").and_then(Value::as_str) == Some("baseline-open-call"))
        .context("missing open call")?;
    assert!(node_index < open_index);

    let final_input = requests[3].input();
    let rendered = serde_json::to_string(&final_input)?;
    assert!(rendered.contains("<spine_memory"));
    assert!(rendered.contains("baseline memory"));
    assert!(rendered.contains("[U1]"));
    assert!(!rendered.contains("baseline child-only reaction"));
    assert!(!rendered.contains("baseline-child-work-call"));
    assert!(!rendered.contains("baseline-child-local-tool-output"));
    assert!(!rendered.contains("baseline-open-call"));
    assert!(!rendered.contains("<spine_status "));
    assert!(!rendered.contains("<spine_tran_status>"));
    Ok(())
}

#[tokio::test]
async fn close_replaces_nested_sibling_and_keeps_current_sampling_in_parent() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("close-replacement-open"),
                ev_function_call_with_namespace(
                    "close-replacement-open-call",
                    "spine",
                    "open",
                    r#"{"summary":"first child"}"#,
                ),
                ev_completed("close-replacement-open"),
            ]),
            sse(vec![
                ev_response_created("close-replacement-next"),
                ev_function_call_with_namespace(
                    "close-replacement-next-call",
                    "spine",
                    "next",
                    r#"{"memory":"first child memory","summary":"second child"}"#,
                ),
                ev_completed("close-replacement-next"),
            ]),
            sse(vec![
                ev_response_created("close-replacement-close"),
                ev_function_call_with_namespace(
                    "close-replacement-close-call",
                    "spine",
                    "close",
                    r#"{"memory":"second child memory"}"#,
                ),
                ev_completed("close-replacement-close"),
            ]),
            sse(vec![
                ev_response_created("close-replacement-parent"),
                ev_completed("close-replacement-parent"),
            ]),
            sse(vec![
                ev_response_created("close-replacement-later"),
                ev_completed("close-replacement-later"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex().build(&server).await?;

    test.submit_turn("close replacement parent evidence")
        .await?;
    test.submit_turn("later parent sampling").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    let close_producing_input = requests[2].input();
    assert!(close_producing_input.iter().any(|item| {
        item.get("call_id").and_then(Value::as_str) == Some("close-replacement-next-call")
    }));

    let immediate_parent_input = requests[3].input();
    let first_memory = immediate_parent_input
        .iter()
        .position(|item| item.to_string().contains("first child memory"))
        .context("missing first closed sibling memory")?;
    let second_memory = immediate_parent_input
        .iter()
        .position(|item| item.to_string().contains("second child memory"))
        .context("missing second closed sibling memory")?;
    assert_eq!(second_memory, first_memory + 1);
    assert_eq!(second_memory + 3, immediate_parent_input.len());
    assert_eq!(
        immediate_parent_input[first_memory]
            .pointer("/content/0/text")
            .and_then(Value::as_str),
        Some("<spine_memory node_id=\"1.1\">\nfirst child memory\n</spine_memory>")
    );
    assert_eq!(
        immediate_parent_input[second_memory]
            .pointer("/content/0/text")
            .and_then(Value::as_str),
        Some("<spine_memory node_id=\"1.2\">\nsecond child memory\n</spine_memory>")
    );
    for input in [immediate_parent_input, requests[4].input()] {
        let close_request = input
            .iter()
            .position(|item| {
                item.get("call_id").and_then(Value::as_str) == Some("close-replacement-close-call")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
            })
            .context("parent context must retain the Close request")?;
        let close_output = input
            .iter()
            .position(|item| {
                item.get("call_id").and_then(Value::as_str) == Some("close-replacement-close-call")
                    && item.get("type").and_then(Value::as_str) == Some("function_call_output")
            })
            .context("parent context must retain the Close output")?;
        assert_eq!(close_request, second_memory + 1);
        assert_eq!(close_output, close_request + 1);
    }

    let rollout = fs::read_to_string(test.codex.rollout_path().context("rollout path")?)?;
    let rollout_lines = rollout
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(rollout_lines.iter().any(|line| {
        matches!(
            &line.item,
            RolloutItem::ResponseItem(codex_protocol::models::ResponseItem::FunctionCall {
                call_id,
                ..
            }) if call_id == "close-replacement-close-call"
        )
    }));
    assert!(rollout_lines.iter().any(|line| {
        matches!(
            &line.item,
            RolloutItem::ResponseItem(
                codex_protocol::models::ResponseItem::FunctionCallOutput { call_id, .. }
            ) if call_id == "close-replacement-close-call"
        )
    }));

    let records = load_sampling_records(&test)?;
    let close_commit_index = records
        .iter()
        .position(|record| {
            matches!(
                record,
                SamplingArchiveRecord::SamplingCommit(commit)
                    if commit.executions.iter().any(|execution| {
                        matches!(
                            (&execution.origin, &execution.operation),
                            (
                                spine_core::ExecutionOrigin::Direct { call_id },
                                SpineOperationFact::Close { memory }
                            ) if call_id == "close-replacement-close-call"
                                && memory == "second child memory"
                        )
                    })
            )
        })
        .context("missing durable close commit")?;
    let SamplingArchiveRecord::SamplingCommit(close_commit) = &records[close_commit_index] else {
        unreachable!("close commit index must identify a commit");
    };
    let close_execution = close_commit
        .executions
        .iter()
        .find(|execution| {
            matches!(
                &execution.origin,
                spine_core::ExecutionOrigin::Direct { call_id }
                    if call_id == "close-replacement-close-call"
            )
        })
        .context("close commit must retain its execution")?;
    assert!(
        close_execution.source_span.start.ordinal() < close_execution.source_span.end.ordinal()
    );
    let SamplingArchiveRecord::SamplingStarted(next_started) = &records[close_commit_index + 1]
    else {
        anyhow::bail!("close commit must be followed by the parent sampling start");
    };
    assert_eq!(next_started.pre_boundary, close_commit.post_boundary);
    assert_eq!(
        next_started.previous_commit_id.as_ref(),
        Some(&close_commit.commit_id)
    );

    Ok(())
}

#[tokio::test]
async fn spine_next_replaces_closed_child_local_context_and_opens_sibling() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("next-open"),
                ev_function_call_with_namespace(
                    "next-open-call",
                    "spine",
                    "open",
                    r#"{"summary":"first child"}"#,
                ),
                ev_completed("next-open"),
            ]),
            sse(vec![
                ev_response_created("next-child-work"),
                ev_assistant_message("next-child-reaction", "first child-only reaction"),
                ev_function_call(
                    "next-child-work-call",
                    "shell_command",
                    &json!({"command": "echo next-child-local-tool-output"}).to_string(),
                ),
                ev_completed("next-child-work"),
            ]),
            sse(vec![
                ev_response_created("next-transition"),
                ev_function_call_with_namespace(
                    "next-transition-call",
                    "spine",
                    "next",
                    r#"{"memory":"first child memory","summary":"second child"}"#,
                ),
                ev_completed("next-transition"),
            ]),
            sse(vec![
                ev_response_created("next-final"),
                ev_completed("next-final"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex().build(&server).await?;
    test.submit_turn("parent evidence for next").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    let rendered = serde_json::to_string(&requests[3].input())?;
    assert!(rendered.contains("parent evidence for next"));
    assert!(rendered.contains("first child memory"));
    assert!(rendered.contains("second child"));
    assert!(!rendered.contains("first child-only reaction"));
    assert!(!rendered.contains("next-child-work-call"));
    assert!(!rendered.contains("next-child-local-tool-output"));
    Ok(())
}

#[tokio::test]
async fn spine_adapter_usage_samples_do_not_append_status_tail() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spine-pressure-open"),
                ev_function_call_with_namespace(
                    "pressure-open",
                    "spine",
                    "open",
                    r#"{"summary":"pressure child"}"#,
                ),
                ev_completed_with_tokens("spine-pressure-open", 100),
            ]),
            sse(vec![
                ev_response_created("spine-pressure-update"),
                ev_completed_with_tokens("spine-pressure-update", 180),
            ]),
            sse(vec![
                ev_response_created("spine-pressure-done"),
                ev_completed("spine-pressure-done"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex().build(&server).await?;

    test.submit_turn("open a pressure scope").await?;
    test.submit_turn("measure pressure after another request")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let final_input = requests[2].input();
    assert!(
        final_input
            .iter()
            .all(|item| !item.to_string().contains("<spine_status ")),
        "usage samples must not append a Spine status developer tail"
    );

    Ok(())
}

#[tokio::test]
async fn spine_projected_items_receive_ids_at_prompt_boundary() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("spine-adapter-item-ids"),
            ev_completed("spine-adapter-item-ids"),
        ]),
    )
    .await;
    let mut builder = spine_test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config
                .features
                .enable(Feature::ItemIds)
                .expect("ItemIds should be configurable in tests");
        });
    let test = builder.build(&server).await?;

    test.submit_turn("item identity probe").await?;

    let input = response_mock.single_request().input();
    assert!(
        input
            .iter()
            .all(|item| !item.to_string().contains("<spine_status ")),
        "item ID assignment must not append a Spine status developer tail"
    );
    for item in input {
        assert!(
            item.get("id").and_then(Value::as_str).is_some(),
            "model-visible input item is missing an ID: {item:#?}"
        );
    }

    Ok(())
}

#[cfg_attr(windows, ignore = "no exec_command on Windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spine_adapter_preserves_host_tool_output_truncation() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spine-truncation-1"),
                ev_function_call(
                    "large-output",
                    "exec_command",
                    &json!({
                        "cmd": "python3 -c \"import sys; sys.stdout.write('x' * 50000)\""
                    })
                    .to_string(),
                ),
                ev_completed("spine-truncation-1"),
            ]),
            sse(vec![
                ev_response_created("spine-truncation-2"),
                ev_completed("spine-truncation-2"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex().with_config(|config| {
        config.tool_output_token_limit = Some(50);
    });
    let test = builder.build(&server).await?;

    test.submit_turn("produce a large tool result").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .function_call_output_text("large-output")
        .context("missing exec_command output")?;
    assert!(
        output.len() < 2_000,
        "Spine projection restored an untruncated tool output: {} bytes",
        output.len()
    );
    assert!(output.contains("tokens truncated"));

    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spine_adapter_reprojects_trimmed_tool_output_for_next_request() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spine-trim-1"),
                ev_function_call(
                    "large-output",
                    "exec_command",
                    &json!({
                        "cmd": "python3 -c \"import sys; sys.stdout.write('x' * 50000)\""
                    })
                    .to_string(),
                ),
                ev_completed("spine-trim-1"),
            ]),
            sse(vec![
                ev_response_created("spine-trim-2"),
                ev_function_call_with_namespace(
                    "trim-call",
                    "spine",
                    "trim",
                    r#"{"TRIM_ID":"trim_4","op":"snip"}"#,
                ),
                ev_completed("spine-trim-2"),
            ]),
            sse(vec![
                ev_response_created("spine-trim-3"),
                ev_completed("spine-trim-3"),
            ]),
        ],
    )
    .await;
    let test = spine_test_codex()
        .with_spine_trim()
        .with_config(|config| {
            config.tool_output_token_limit = Some(20_000);
        })
        .build(&server)
        .await?;

    test.submit_turn("produce and trim a large tool result")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let tagged = requests[1]
        .function_call_output_text("large-output")
        .context("missing tagged tool output before trim")?;
    assert!(tagged.starts_with("[TRIM_ID: trim_4]\n"));
    assert!(tagged.contains('x'));
    assert!(requests[2].has_function_call("trim-call"));

    assert_eq!(
        requests[2].function_call_output_text("large-output"),
        Some("[Old tool result content cleared]".to_string())
    );

    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spine_adapter_legacy_notify_uses_first_sampling_input_after_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let opened = sse(vec![
        ev_response_created("spine-notify-open"),
        ev_function_call_with_namespace(
            "spine-notify-open-call",
            "spine",
            "open",
            r#"{"summary":"notify retry child"}"#,
        ),
        ev_completed("spine-notify-open"),
    ]);
    let opened_done = sse(vec![
        ev_assistant_message("spine-notify-open-done", "child ready"),
        ev_completed("spine-notify-open-done"),
    ]);
    let close_incomplete = sse(vec![
        ev_response_created("spine-notify-close-incomplete"),
        ev_function_call_with_namespace(
            "spine-notify-close-call",
            "spine",
            "close",
            r#"{"memory":"closed child memory"}"#,
        ),
    ]);
    let retry_done = sse(vec![
        ev_assistant_message("spine-notify-retry-done", "done"),
        ev_completed("spine-notify-retry-done"),
    ]);
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: opened,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: opened_done,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: close_incomplete,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: retry_done,
        }],
    ])
    .await;
    let notify_dir = TempDir::new()?;
    let notify_script = notify_dir.path().join("notify.sh");
    fs::write(
        &notify_script,
        r#"#!/bin/bash
set -e
payload_path="$(dirname "${0}")/notify.jsonl"
printf '%s\n' "${@: -1}" >> "${payload_path}""#,
    )?;
    fs::set_permissions(&notify_script, fs::Permissions::from_mode(0o755))?;
    let notify_file = notify_dir.path().join("notify.jsonl");
    let notify_script_str = notify_script.to_str().context("notify path")?.to_string();
    let mut builder = spine_test_codex().with_config(move |config| {
        config.notify = Some(vec![notify_script_str]);
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(1);
    });
    let test = builder.build_with_streaming_server(&server).await?;

    test.submit_turn("open a child for notify retry").await?;
    test.submit_turn("child evidence removed by close").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fs::read_to_string(&notify_file)
            .map(|contents| contents.lines().count() < 2)
            .unwrap_or(true)
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("timed out waiting for legacy notify payload")?;
    let notify_payloads = fs::read_to_string(&notify_file)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);
    let failed_input: Value = serde_json::from_slice(&requests[2])?;
    let retry_input: Value = serde_json::from_slice(&requests[3])?;
    assert!(
        !failed_input["input"]
            .to_string()
            .contains("closed child memory"),
        "failed attempt must use the pre-transition projection"
    );
    assert!(
        retry_input["input"]
            .to_string()
            .contains("closed child memory"),
        "successful retry must use the projection installed by the failed attempt"
    );
    let first_input_user_messages = failed_input["input"]
        .as_array()
        .context("first attempt input should be an array")?
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .filter_map(|item| match codex_core::parse_turn_item(&item) {
            Some(codex_protocol::items::TurnItem::UserMessage(message)) => Some(message.message()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notify_payloads
            .last()
            .context("missing final legacy notify payload")?["input-messages"],
        json!(first_input_user_messages)
    );

    server.shutdown().await;
    Ok(())
}

fn assert_anchored_user_text(actual: &str, expected_body: &str) -> Result<()> {
    let anchored = actual
        .strip_prefix("[U")
        .and_then(|text| text.split_once("]\n"))
        .context("projected user input must have a [U#] anchor")?;
    let (ordinal, body) = anchored;
    anyhow::ensure!(
        !ordinal.is_empty() && ordinal.chars().all(|ch| ch.is_ascii_digit()),
        "projected user anchor ordinal must be numeric"
    );
    anyhow::ensure!(body == expected_body, "projected user body changed");
    Ok(())
}

fn message_text<'a>(input: &'a [Value], role: &str) -> Option<&'a str> {
    input.iter().rev().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some(role))
        .then(|| {
            item.get("content")
                .and_then(Value::as_array)
                .and_then(|content| content.first())
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
        })
        .flatten()
    })
}

fn load_sampling_records(test: &TestCodex) -> Result<Vec<SamplingArchiveRecord>> {
    let rollout = fs::read_to_string(test.codex.rollout_path().context("rollout path")?)?;
    rollout
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::SpineSamplingStarted(item) => Some(item.payload),
            RolloutItem::SpineTransition(item) => Some(item.payload),
            _ => None,
        })
        .map(|payload| {
            SamplingArchiveRecord::decode(&serde_json::to_vec(&payload)?)
                .map_err(anyhow::Error::new)
        })
        .collect()
}

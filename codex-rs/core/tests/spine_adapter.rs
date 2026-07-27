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
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::spine_test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use spine_core::SpineConfig;
use std::fs;
#[cfg(not(target_os = "windows"))]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

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
        matches!(event, EventMsg::SpineTreeUpdate(_))
    })
    .await;
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
        matches!(event, EventMsg::SpineTreeUpdate(_))
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TokenCount(_))
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
    ))?;
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
        .with_config(move |test_config| test_config.spine_config = config)
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
async fn spine_adapter_legacy_notify_uses_sampling_user_input() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("spine-notify"),
            ev_completed("spine-notify"),
        ]),
    )
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
    });
    let test = builder.build(&server).await?;

    test.submit_turn("native notify probe").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !notify_file.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("timed out waiting for legacy notify payload")?;
    let payload: Value = serde_json::from_str(&fs::read_to_string(notify_file)?)?;
    let request_input = response_mock.single_request().input();
    let projected_user_input =
        message_text(&request_input, "user").context("missing projected user input")?;
    assert_anchored_user_text(projected_user_input, "native notify probe")?;
    assert_eq!(payload["input-messages"], json!([projected_user_input]));

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

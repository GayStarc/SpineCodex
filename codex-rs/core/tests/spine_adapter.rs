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
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::spine_test_codex;
use serde_json::Value;
use serde_json::json;
use std::fs;
#[cfg(not(target_os = "windows"))]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[tokio::test]
async fn spine_adapter_profile_has_no_status_without_transition() -> Result<()> {
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
    assert!(!test.config.features.enabled(Feature::SpineStatus));

    test.submit_turn("adapter profile probe").await?;

    let input = response_mock.single_request().input();
    let user_text = message_text(&input, "user").context("missing projected user input")?;
    assert_anchored_user_text(user_text, "adapter profile probe")?;
    assert!(
        input.iter().all(|item| {
            let item = item.to_string();
            !item.contains("<spine_status ") && !item.contains("<spine_tran_status ")
        }),
        "a request without a completed Spine control must not synthesize any status"
    );

    Ok(())
}

#[tokio::test]
async fn legacy_spine_status_feature_does_not_gate_transition_status() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spine-status-disabled-open"),
                ev_function_call_with_namespace(
                    "spine-status-disabled-open",
                    "spine",
                    "open",
                    r#"{"summary":"gate independent"}"#,
                ),
                ev_completed("spine-status-disabled-open"),
            ]),
            sse(vec![
                ev_response_created("spine-status-disabled-done"),
                ev_completed("spine-status-disabled-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex().with_config(|config| {
        config
            .features
            .disable(Feature::SpineStatus)
            .expect("SpineStatus should be configurable in tests");
    });
    let test = builder.build(&server).await?;

    test.submit_turn("legacy status gate probe").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].input().iter().all(|item| {
            let item = item.to_string();
            !item.contains("<spine_status ") && !item.contains("<spine_tran_status ")
        }),
        "initial request must not synthesize any status"
    );
    let input = requests[1].input();
    let status = input
        .iter()
        .find(|item| item.to_string().contains("<spine_tran_status "))
        .context("completed Spine control must persist transition status")?;
    assert_eq!(status["role"], "developer");
    assert_eq!(
        status["content"][0]["text"]
            .as_str()
            .map(|text| text.contains(r#"cursor="1.1""#)),
        Some(true)
    );
    let status_text = status["content"][0]["text"]
        .as_str()
        .context("transition status text")?
        .to_string();
    test.codex.flush_rollout().await?;
    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let persisted = fs::read_to_string(rollout_path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|line| {
            matches!(
                line.item,
                RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                    if role == "developer"
                        && matches!(
                            content.as_slice(),
                            [ContentItem::InputText { text }] if text == &status_text
                        )
            )
        });
    assert!(
        persisted,
        "legacy SpineStatus=false must not suppress transition-status persistence"
    );

    Ok(())
}

#[tokio::test]
async fn spine_adapter_item_ids_cover_persisted_transition_status() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("spine-adapter-item-ids-open"),
                ev_function_call_with_namespace(
                    "spine-adapter-item-ids-open",
                    "spine",
                    "open",
                    r#"{"summary":"item identity"}"#,
                ),
                ev_completed("spine-adapter-item-ids-open"),
            ]),
            sse(vec![
                ev_response_created("spine-adapter-item-ids-done"),
                ev_completed("spine-adapter-item-ids-done"),
            ]),
        ],
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
    test.codex.flush_rollout().await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let input = requests[1].input();
    let status = input
        .iter()
        .find(|item| {
            item.get("role").and_then(Value::as_str) == Some("developer")
                && item.to_string().contains("<spine_tran_status ")
        })
        .context("missing persisted transition status")?;
    let request_status_id = status
        .get("id")
        .and_then(Value::as_str)
        .context("persisted transition status request item is missing an ID")?
        .to_string();
    for item in &input {
        assert!(
            item.get("id").and_then(Value::as_str).is_some(),
            "model-visible input item is missing an ID: {item:#?}"
        );
    }
    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let rollout = fs::read_to_string(rollout_path)?;
    let disk_status_id = rollout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find_map(|line| match line.item {
            RolloutItem::ResponseItem(ResponseItem::Message {
                id, role, content, ..
            }) if role == "developer"
                && matches!(
                    content.as_slice(),
                    [ContentItem::InputText { text }] if text.starts_with("<spine_tran_status ")
                ) =>
            {
                id
            }
            _ => None,
        })
        .context("persisted rollout transition status is missing an ID")?;
    assert_eq!(disk_status_id, request_status_id);

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
async fn spine_adapter_legacy_notify_uses_native_user_evidence() -> Result<()> {
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
    assert_eq!(payload["input-messages"], json!(["native notify probe"]));
    assert_eq!(response_mock.requests().len(), 1);

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

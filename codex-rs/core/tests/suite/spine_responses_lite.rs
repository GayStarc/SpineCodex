use anyhow::Context;
use anyhow::Result;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_features::Feature;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
#[cfg(not(target_os = "windows"))]
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
#[cfg(not(target_os = "windows"))]
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::spine_test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(target_os = "windows"))]
use std::time::Duration;
use tempfile::TempDir;

const CODE_MODE_SPINE_CARRIER_MARKER: &str = "spine.code_mode.output.v1";

fn write_first_spine_open_blocking_post_hook(home: &std::path::Path) -> Result<()> {
    let script_path = home.join("post_tool_use_spine_open.py");
    let log_path = home.join("post_tool_use_spine_open.jsonl");
    let script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{log_path}")
payload = json.load(sys.stdin)
invocation_index = 0
if log_path.exists():
    invocation_index = len(log_path.read_text(encoding="utf-8").splitlines())
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
if invocation_index == 0:
    print(json.dumps({{"decision": "block", "reason": "first open blocked by test hook"}}))
"#,
        log_path = log_path.display(),
    );
    let hooks = serde_json::json!({
        "hooks": {
            "PostToolUse": [{
                "matcher": "^spineopen$",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", script_path.display()),
                }]
            }]
        }
    });
    std::fs::write(&script_path, script).context("write Spine PostToolUse hook script")?;
    std::fs::write(home.join("hooks.json"), hooks.to_string())
        .context("write Spine PostToolUse hook config")?;
    Ok(())
}

fn has_namespaced_tool(tools: &[Value], namespace: &str, tool_name: &str) -> bool {
    tools.iter().any(|tool| {
        tool.get("type").and_then(Value::as_str) == Some("namespace")
            && tool.get("name").and_then(Value::as_str) == Some(namespace)
            && tool["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
            })
    })
}

fn additional_tools(body: &Value) -> Result<&[Value]> {
    body["input"]
        .as_array()
        .context("Responses request input should be an array")?
        .first()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .context("Responses request should start with additional_tools")?["tools"]
        .as_array()
        .map(Vec::as_slice)
        .context("additional_tools tools should be an array")
}

fn request_spine_transition_statuses(body: &Value) -> Vec<&str> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("developer"))
            .then_some(item)
        })
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .filter(|text| text.starts_with("<spine_tran_status "))
        .collect()
}

fn completed_without_usage(id: &str) -> Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {"id": id}
    })
}

fn completed_with_usage(id: &str, input_tokens: i64, output_tokens: i64) -> Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": null,
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": input_tokens + output_tokens
            }
        }
    })
}

fn ev_compaction_item(id: &str, encrypted_content: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "compaction",
            "id": id,
            "encrypted_content": encrypted_content,
        }
    })
}

fn status_kilotokens(status: &str, field: &str) -> Result<f64> {
    let marker = format!(r#"{field}=""#);
    let value = status
        .split_once(&marker)
        .with_context(|| format!("status is missing {field}: {status}"))?
        .1
        .split_once('"')
        .with_context(|| format!("status has malformed {field}: {status}"))?
        .0
        .strip_suffix('K')
        .with_context(|| format!("status {field} is not expressed in kilotokens: {status}"))?;
    value
        .parse()
        .with_context(|| format!("status has non-numeric {field}: {status}"))
}

fn persisted_spine_transition_statuses(test: &TestCodex) -> Result<Vec<String>> {
    let path = test
        .codex
        .rollout_path()
        .context("test thread is missing its rollout path")?;
    let rollout = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read rollout {}", path.display()))?;
    Ok(rollout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                if role == "developer" =>
            {
                content.into_iter().find_map(|item| match item {
                    codex_protocol::models::ContentItem::InputText { text }
                        if text.starts_with("<spine_tran_status ") =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>())
}

async fn persist_spine_transition_status_and_shutdown(
    server: &wiremock::MockServer,
) -> Result<(Arc<TempDir>, PathBuf, String)> {
    let mut builder = spine_test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
    });
    let test = builder.build(server).await?;

    test.submit_turn("persist status").await?;
    test.codex.flush_rollout().await?;
    let persisted = persisted_spine_transition_statuses(&test)?;
    let [persisted_status] = persisted.as_slice() else {
        panic!("expected exactly one persisted transition status, got {persisted:#?}");
    };
    let home = test.home.clone();
    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    Ok((home, rollout_path, persisted_status.clone()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_direct_controls_admit_first_valid_native_ordinal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-direct-control-batch"),
                responses::ev_function_call_with_namespace(
                    "direct-open-first",
                    "spine",
                    "open",
                    r#"{"goal":"first child"}"#,
                ),
                responses::ev_function_call_with_namespace(
                    "direct-open-second",
                    "spine",
                    "open",
                    r#"{"goal":"second child"}"#,
                ),
                responses::ev_completed("resp-direct-control-batch"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-direct-control-batch", "done"),
                responses::ev_completed("resp-direct-control-batch-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
    });
    let test = builder.build(&server).await?;

    test.submit_turn("run two direct Spine controls").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let followup = &requests[1];
    let first = followup
        .function_call_output_text("direct-open-first")
        .context("first direct open output")?;
    assert_eq!(first, "Spine open accepted.");
    let second = followup
        .function_call_output_text("direct-open-second")
        .context("second direct open output")?;
    assert!(
        second.contains("already has a validated Spine control"),
        "unexpected second control output: {second}"
    );
    let followup_body = followup.body_json();
    let statuses = request_spine_transition_statuses(&followup_body);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].contains(r#"cursor="1.1""#), "{}", statuses[0]);
    assert!(
        statuses[0].contains(r#"summary="first child""#),
        "{}",
        statuses[0]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_direct_controls_skip_runtime_invalid_earlier_call() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-direct-control-invalid-first"),
                responses::ev_function_call_with_namespace(
                    "direct-close-root",
                    "spine",
                    "close",
                    r#"{"memory":"root cannot close"}"#,
                ),
                responses::ev_function_call_with_namespace(
                    "direct-open-after-invalid",
                    "spine",
                    "open",
                    r#"{"goal":"valid child"}"#,
                ),
                responses::ev_completed("resp-direct-control-invalid-first"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-direct-control-invalid-first", "done"),
                responses::ev_completed("resp-direct-control-invalid-first-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
    });
    let test = builder.build(&server).await?;

    test.submit_turn("skip invalid direct Spine control")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let followup = &requests[1];
    let close_root = followup
        .function_call_output_text("direct-close-root")
        .context("invalid root close output")?;
    assert!(
        close_root.contains("no open Spine node is available to close"),
        "unexpected invalid close output: {close_root}"
    );
    let open = followup
        .function_call_output_text("direct-open-after-invalid")
        .context("valid open output")?;
    assert_eq!(open, "Spine open accepted.");
    let followup_body = followup.body_json();
    let statuses = request_spine_transition_statuses(&followup_body);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].contains(r#"cursor="1.1""#), "{}", statuses[0]);
    assert!(
        statuses[0].contains(r#"summary="valid child""#),
        "{}",
        statuses[0]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_direct_control_post_hook_failure_releases_next_ordinal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-direct-control-post-hook"),
                responses::ev_function_call_with_namespace(
                    "direct-open-blocked",
                    "spine",
                    "open",
                    r#"{"goal":"blocked child"}"#,
                ),
                responses::ev_function_call_with_namespace(
                    "direct-open-after-hook",
                    "spine",
                    "open",
                    r#"{"goal":"hook survivor"}"#,
                ),
                responses::ev_completed("resp-direct-control-post-hook"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-direct-control-post-hook", "done"),
                responses::ev_completed("resp-direct-control-post-hook-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_pre_build_hook(|home| {
            write_first_spine_open_blocking_post_hook(home)
                .expect("write blocking Spine PostToolUse hook fixture");
        })
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    test.submit_turn("run direct Spine controls through a blocking post hook")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let followup = &requests[1];
    assert_eq!(
        followup
            .function_call_output_text("direct-open-blocked")
            .as_deref(),
        Some("first open blocked by test hook")
    );
    assert_eq!(
        followup
            .function_call_output_text("direct-open-after-hook")
            .as_deref(),
        Some("Spine open accepted.")
    );
    let followup_body = followup.body_json();
    let statuses = request_spine_transition_statuses(&followup_body);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].contains(r#"cursor="1.1""#), "{}", statuses[0]);
    assert!(
        statuses[0].contains(r#"summary="hook survivor""#),
        "{}",
        statuses[0]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_spine_transition_status_follows_tool_output() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-status-open"),
                responses::ev_function_call_with_namespace(
                    "status-open",
                    "spine",
                    "open",
                    r#"{"goal":"status child"}"#,
                ),
                responses::ev_completed("resp-status-open"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-status-done"),
                responses::ev_completed("resp-status-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
    });
    let test = builder.build(&server).await?;

    test.submit_turn("transition status").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].input().iter().all(|item| {
            let item = item.to_string();
            !item.contains("<spine_status ") && !item.contains("<spine_tran_status ")
        }),
        "initial request must not contain dynamic or transition status"
    );
    let input = requests[1].input();
    let last = input.last().context("request input should not be empty")?;
    assert_eq!(last["type"], "message");
    assert_eq!(last["role"], "developer");
    let text = last["content"][0]["text"]
        .as_str()
        .context("status input should contain text")?;
    assert!(text.starts_with("<spine_tran_status "), "{text}");
    assert!(text.contains(r#"cursor="1.1""#), "{text}");
    for field in [
        "cursor=",
        "summary=",
        "parent=",
        "parent_summary=",
        "cursor_context=",
        "context_left=",
    ] {
        assert!(text.contains(field), "missing {field} in {text}");
    }
    test.codex.flush_rollout().await?;
    assert_eq!(
        persisted_spine_transition_statuses(&test)?,
        vec![text.to_string()]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_spine_transition_status_uses_body_after_prefix_context_left() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let sampled_output = "s".repeat(40_000);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-status-body-window-open"),
                responses::ev_assistant_message("resp-status-body-window-output", &sampled_output),
                responses::ev_function_call_with_namespace(
                    "status-body-window-open",
                    "spine",
                    "open",
                    r#"{"goal":"body window child"}"#,
                ),
                responses::ev_completed_with_tokens("resp-status-body-window-open", 100_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-status-body-window-done"),
                responses::ev_completed("resp-status-body-window-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.model_context_window = Some(200_000);
            config.model_auto_compact_token_limit = Some(80_000);
            config.model_auto_compact_token_limit_scope =
                AutoCompactTokenLimitScope::BodyAfterPrefix;
        });
    let test = builder.build(&server).await?;

    test.submit_turn("body-after-prefix transition status")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let follow_up = requests[1].body_json();
    let statuses = request_spine_transition_statuses(&follow_up);
    assert_eq!(statuses.len(), 1);
    let context_left = status_kilotokens(statuses[0], "context_left")?;
    assert!(
        (50.0..80.0).contains(&context_left),
        "sampled growth must reduce BodyAfterPrefix context_left without resetting the window: {}",
        statuses[0],
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_open_rewrite_preserves_body_after_prefix_growth() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let large_sampled_output = "x".repeat(80_000);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("body-prefix-open"),
                responses::ev_assistant_message("body-prefix-large-output", &large_sampled_output),
                responses::ev_function_call_with_namespace(
                    "body-prefix-open-call",
                    "spine",
                    "open",
                    r#"{"goal":"body budget child"}"#,
                ),
                completed_with_usage(
                    "body-prefix-open",
                    /*input_tokens*/ 5_000,
                    /*output_tokens*/ 20_000,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("body-prefix-compact"),
                ev_compaction_item("body-prefix-compact-item", "body-prefix-compact-summary"),
                responses::ev_completed_with_tokens("body-prefix-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("body-prefix-done"),
                responses::ev_assistant_message("body-prefix-done-message", "done"),
                responses::ev_completed_with_tokens("body-prefix-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.model_context_window = Some(200_000);
            config.model_auto_compact_token_limit = Some(5_000);
            config.model_auto_compact_token_limit_scope =
                AutoCompactTokenLimitScope::BodyAfterPrefix;
        });
    let test = builder.build(&server).await?;

    test.submit_turn("open after producing a large sampled output")
        .await?;

    let requests = response_mock.requests();
    assert!(
        requests.len() >= 2,
        "the open request must be followed by either compact or normal sampling"
    );
    assert_eq!(
        requests[1]
            .input()
            .last()
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str),
        Some("compaction_trigger"),
        "spine.open is not a new auto-compact window and must not reset the body budget"
    );
    assert_eq!(
        requests.len(),
        3,
        "sampled growth beyond the BodyAfterPrefix limit must compact before the normal follow-up"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_later_usage_cannot_absorb_no_usage_first_request_growth() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let first_growth = "a".repeat(40_000);
    let second_growth = "b".repeat(40_000);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("body-prefix-no-usage-first"),
                responses::ev_assistant_message("body-prefix-no-usage-first-output", &first_growth),
                responses::ev_function_call(
                    "body-prefix-no-usage-first-tool",
                    "missing_tool",
                    "{}",
                ),
                completed_without_usage("body-prefix-no-usage-first"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("body-prefix-later-usage"),
                responses::ev_assistant_message("body-prefix-later-usage-output", &second_growth),
                responses::ev_function_call("body-prefix-later-usage-tool", "missing_tool", "{}"),
                completed_with_usage(
                    "body-prefix-later-usage",
                    /*input_tokens*/ 100_000,
                    /*output_tokens*/ 8_000,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("body-prefix-no-usage-compact"),
                ev_compaction_item(
                    "body-prefix-no-usage-compact-item",
                    "body-prefix-no-usage-summary",
                ),
                responses::ev_completed_with_tokens("body-prefix-no-usage-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("body-prefix-no-usage-done"),
                responses::ev_assistant_message("body-prefix-no-usage-done-message", "done"),
                responses::ev_completed_with_tokens("body-prefix-no-usage-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.model_context_window = Some(200_000);
            config.model_auto_compact_token_limit = Some(15_000);
            config.model_auto_compact_token_limit_scope =
                AutoCompactTokenLimitScope::BodyAfterPrefix;
        });
    let test = builder.build(&server).await?;

    test.submit_turn("accumulate body growth across a no-usage first request")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[2]
            .input()
            .last()
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str),
        Some("compaction_trigger"),
        "usage from a later request must not become U0 and absorb earlier no-usage growth"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_spine_transition_status_survives_shutdown_and_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-status-resume-open"),
                responses::ev_function_call_with_namespace(
                    "status-resume-open",
                    "spine",
                    "open",
                    r#"{"goal":"resumed child"}"#,
                ),
                responses::ev_completed("resp-status-resume-open"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-status-resume-done"),
                responses::ev_completed("resp-status-resume-done"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-status-after-resume"),
                responses::ev_completed("resp-status-after-resume"),
            ]),
        ],
    )
    .await;
    let (home, rollout_path, persisted_status) =
        persist_spine_transition_status_and_shutdown(&server).await?;

    let mut resume_builder = spine_test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn("resume status").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let resumed_input = requests[2].input();
    let status = resumed_input
        .iter()
        .find(|item| item.to_string().contains("<spine_tran_status "))
        .context("resumed request must contain persisted transition status")?;
    assert_eq!(
        status["content"][0]["text"].as_str(),
        Some(persisted_status.as_str())
    );
    let status_index = resumed_input
        .iter()
        .position(|item| item == status)
        .context("transition status index")?;
    let resumed_user_index = resumed_input
        .iter()
        .position(|item| {
            item.get("role").and_then(Value::as_str) == Some("user")
                && item.to_string().contains("resume status")
        })
        .context("resumed user input")?;
    assert!(status_index < resumed_user_index);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_spine_memory_slots_precede_the_transition_status() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-memory-open"),
                responses::ev_function_call_with_namespace(
                    "memory-open",
                    "spine",
                    "open",
                    r#"{"goal":"memory child"}"#,
                ),
                responses::ev_completed_with_tokens("resp-memory-open", 10_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-memory-opened"),
                responses::ev_completed_with_tokens("resp-memory-opened", 42_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-memory-close"),
                responses::ev_function_call_with_namespace(
                    "memory-close",
                    "spine",
                    "close",
                    r#"{"memory":"child complete"}"#,
                ),
                responses::ev_completed_with_tokens("resp-memory-close", 55_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-memory-done"),
                responses::ev_completed("resp-memory-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex().with_model_info_override("gpt-5.4", |model_info| {
        model_info.use_responses_lite = true;
    });
    let test = builder.build(&server).await?;

    test.submit_turn("root request").await?;
    test.submit_turn("child request").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    let close_body = requests[3].body_json();
    let close_statuses = request_spine_transition_statuses(&close_body);
    assert_eq!(close_statuses.len(), 1);
    assert!(
        close_statuses[0].contains(r#"cursor_context="45.0K""#),
        "{}",
        close_statuses[0]
    );
    let input = requests[3].input();
    let user_texts = input
        .iter()
        .enumerate()
        .filter(|(_, item)| item.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|(index, item)| {
            item["content"][0]["text"]
                .as_str()
                .map(|text| (index, text))
        })
        .collect::<Vec<_>>();
    let child_user = user_texts
        .iter()
        .find(|(_, text)| text.starts_with("[U") && text.ends_with("\nchild request"))
        .with_context(|| format!("closed child user slot should be present: {user_texts:#?}"))?;
    let child_summary = user_texts
        .iter()
        .find(|(_, text)| {
            *text == "<spine_memory node_id=\"1.1\">\nchild complete\n</spine_memory>"
        })
        .context("closed child summary slot should be present")?;
    let status_index = input.len() - 1;
    assert!(child_user.0 < child_summary.0);
    assert!(child_summary.0 < status_index);
    let status = &input[status_index];
    assert_eq!(status["role"], "developer");
    let status_text = status["content"][0]["text"]
        .as_str()
        .context("status input should contain text")?;
    assert!(
        status_text.starts_with("<spine_tran_status "),
        "{status_text}"
    );
    for field in [
        "cursor=",
        "summary=",
        "parent=",
        "parent_summary=",
        "cursor_context=",
        "context_left=",
    ] {
        assert!(
            status_text.contains(field),
            "missing {field} in {status_text}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_spine_close_rebases_provider_usage_before_auto_compact() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut old_reasoning =
        responses::ev_reasoning_item("closed-child-reasoning", &["old"], &[&"r".repeat(360_000)]);
    old_reasoning["item"]
        .as_object_mut()
        .context("reasoning fixture item should be an object")?
        .remove("content");
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-open"),
                responses::ev_function_call_with_namespace(
                    "rebase-open",
                    "spine",
                    "open",
                    r#"{"goal":"large child"}"#,
                ),
                responses::ev_completed_with_tokens("resp-rebase-open", 10_000),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-opened"),
                responses::ev_assistant_message("msg-rebase-opened", "child ready"),
                responses::ev_completed_with_tokens("resp-rebase-opened", 10_000),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-child-work"),
                old_reasoning,
                responses::ev_assistant_message(
                    "msg-rebase-child-work",
                    "large child work complete",
                ),
                responses::ev_completed_with_tokens("resp-rebase-child-work", 100_000),
            ]))
            .insert_header("X-Reasoning-Included", "true"),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-close"),
                responses::ev_function_call_with_namespace(
                    "rebase-close",
                    "spine",
                    "close",
                    r#"{"memory":"short child memory"}"#,
                ),
                responses::ev_completed_with_tokens("resp-rebase-close", 250_000),
            ]))
            .insert_header("X-Reasoning-Included", "true"),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-done"),
                responses::ev_assistant_message("msg-rebase-done", "done"),
                responses::ev_completed_with_tokens("resp-rebase-done", 159_837),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-next-turn"),
                responses::ev_assistant_message("msg-rebase-next-turn", "next turn done"),
                responses::ev_completed_with_tokens("resp-rebase-next-turn", 10_000),
            ])),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.model_context_window = Some(272_000);
            config.model_auto_compact_token_limit = Some(244_800);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("exercise projection-aware usage rebase")
        .await?;
    test.submit_turn("perform large child work").await?;
    test.submit_turn("close the large child").await?;

    let requests = response_mock.requests();
    assert_eq!(
        requests.len(),
        5,
        "closing the large child should continue normally without auto compact"
    );
    let follow_up = &requests[4];
    let follow_up_body = follow_up.body_json().to_string();
    assert!(
        !follow_up.body_contains_text(SUMMARIZATION_PROMPT),
        "stale provider usage must not trigger a compaction request"
    );
    assert!(
        !follow_up
            .input()
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning")),
        "closed child reasoning must not remain model-visible"
    );
    assert!(
        follow_up_body.contains("short child memory"),
        "closed child memory must replace the removed suffix"
    );
    let follow_up_json = follow_up.body_json();
    let statuses = request_spine_transition_statuses(&follow_up_json);
    assert_eq!(statuses.len(), 1);
    assert!(
        status_kilotokens(statuses[0], "context_left")? > 100.0,
        "status must use the same rebased pressure as compact admission: {}",
        statuses[0]
    );
    let metadata: Value = serde_json::from_str(
        &follow_up
            .header("x-codex-turn-metadata")
            .context("follow-up request should include turn metadata")?,
    )
    .context("follow-up turn metadata should be valid json")?;
    assert_eq!(
        metadata["request_kind"].as_str(),
        Some("turn"),
        "unexpected follow-up metadata: {metadata:#}"
    );
    assert!(
        metadata.get("compaction").is_none(),
        "normal follow-up must not carry compaction metadata"
    );

    test.submit_turn("verify provider-valid projected reasoning accounting")
        .await?;
    let requests = response_mock.requests();
    assert_eq!(
        requests.len(),
        6,
        "closed reasoning must not trigger pre-turn compact after fresh provider usage"
    );
    assert!(
        !requests[5].body_contains_text(SUMMARIZATION_PROMPT),
        "provider-valid accounting must scan h(PS), not hidden native reasoning"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_body_after_prefix_rewrite_keeps_full_window_hard_stop() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let close_arguments = serde_json::json!({
        "memory": "m".repeat(400_000),
    })
    .to_string();
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-body-prefix-open"),
                responses::ev_function_call_with_namespace(
                    "body-prefix-open",
                    "spine",
                    "open",
                    r#"{"goal":"body prefix child"}"#,
                ),
                responses::ev_completed_with_tokens("resp-body-prefix-open", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-body-prefix-opened"),
                responses::ev_assistant_message("msg-body-prefix-opened", "child ready"),
                responses::ev_completed_with_tokens("resp-body-prefix-opened", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-body-prefix-close"),
                responses::ev_function_call_with_namespace(
                    "body-prefix-close",
                    "spine",
                    "close",
                    &close_arguments,
                ),
                responses::ev_completed_with_tokens("resp-body-prefix-close", 10_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-body-prefix-compact"),
                ev_compaction_item("compact-body-prefix", "compact-body-prefix-summary"),
                responses::ev_completed_with_tokens("resp-body-prefix-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-body-prefix-done"),
                responses::ev_assistant_message("msg-body-prefix-done", "done"),
                responses::ev_completed_with_tokens("resp-body-prefix-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(500_000);
            config.model_auto_compact_token_limit_scope =
                AutoCompactTokenLimitScope::BodyAfterPrefix;
        });
    let test = builder.build(&server).await?;

    test.submit_turn("open a body-prefix child").await?;
    test.submit_turn("close with a large memory").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    assert!(
        requests[3]
            .input()
            .last()
            .and_then(|item| item.get("type").and_then(Value::as_str))
            == Some("compaction_trigger"),
        "BodyAfterPrefix accounting must preserve the independent full-window hard stop"
    );
    assert!(
        requests[4]
            .input()
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction")),
        "the hard-stop compact result should be installed before the follow-up request"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_model_output_without_usage_estimates_current_projection() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let large_reasoning = "n".repeat(180_000);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-no-usage-baseline"),
                responses::ev_assistant_message("msg-no-usage-baseline", "baseline"),
                responses::ev_completed_with_tokens("resp-no-usage-baseline", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-no-usage-large"),
                responses::ev_reasoning_item(
                    "reasoning-no-usage-large",
                    &["large"],
                    &[&large_reasoning],
                ),
                responses::ev_function_call("no-usage-tool", "missing_tool", "{}"),
                completed_without_usage("resp-no-usage-large"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-no-usage-compact"),
                ev_compaction_item("compact-no-usage", "compact-no-usage-summary"),
                responses::ev_completed_with_tokens("resp-no-usage-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-no-usage-done"),
                responses::ev_assistant_message("msg-no-usage-done", "done"),
                responses::ev_completed_with_tokens("resp-no-usage-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(40_000);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("establish a provider usage baseline")
        .await?;
    test.submit_turn("record large model output without usage")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .input()
            .last()
            .and_then(|item| item.get("type").and_then(Value::as_str))
            == Some("compaction_trigger"),
        "stale projected pressure should invoke the normal BaseCodex compact path"
    );
    assert!(
        requests[3]
            .input()
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction")),
        "the post-compact follow-up should use the provider's compacted history"
    );
    assert!(
        !requests[2].body_contains_text(SUMMARIZATION_PROMPT),
        "model output without usage must invalidate the old provider baseline"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_forced_full_usage_triggers_base_pre_turn_compact() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-forced-full-seed"),
                responses::ev_assistant_message("msg-forced-full-seed", "seeded"),
                responses::ev_completed_with_tokens("resp-forced-full-seed", 5_000),
            ]),
            responses::sse_failed(
                "resp-forced-full-error",
                "context_length_exceeded",
                "input exceeds the context window",
            ),
            responses::sse(vec![
                responses::ev_response_created("resp-forced-full-compact"),
                ev_compaction_item("compact-forced-full", "compact-forced-full-summary"),
                responses::ev_completed_with_tokens("resp-forced-full-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-forced-full-done"),
                responses::ev_assistant_message("msg-forced-full-done", "done"),
                responses::ev_completed_with_tokens("resp-forced-full-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(40_000);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("seed forced-full state").await?;
    test.submit_turn("trigger forced-full state").await?;
    test.submit_turn("compact from forced-full state").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .input()
            .last()
            .and_then(|item| item.get("type").and_then(Value::as_str))
            == Some("compaction_trigger"),
        "set_token_usage_full must restore the deliberate BaseCodex scalar for the next pre-turn admission"
    );
    assert!(
        requests[3]
            .input()
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction")),
        "forced-full pre-turn compact should install the compact result before normal sampling"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(target_os = "windows"))]
async fn responses_lite_large_tool_output_preserves_provider_valid_base_pressure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let output_args = serde_json::json!({
        "command": "awk 'BEGIN { for (i = 0; i < 60000; i++) printf \"x\" }'",
        "timeout_ms": 5_000,
    })
    .to_string();
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-large-tool"),
                responses::ev_function_call(
                    "provider-valid-large-output",
                    "shell_command",
                    &output_args,
                ),
                responses::ev_completed_with_tokens("resp-large-tool", 35_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-large-tool-compact"),
                ev_compaction_item("compact-large-tool", "compact-large-tool-summary"),
                responses::ev_completed_with_tokens("resp-large-tool-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-large-tool-done"),
                responses::ev_assistant_message("msg-large-tool-done", "done"),
                responses::ev_completed_with_tokens("resp-large-tool-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            model_info.truncation_policy = TruncationPolicyConfig::bytes(100_000);
        })
        .with_config(|config| {
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(40_000);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("count a large local tool output from the provider baseline")
        .await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1].input().iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str)
                    == Some("provider-valid-large-output")
        }),
        "the compact request must contain the pending local tool output"
    );
    assert!(
        requests[1]
            .input()
            .last()
            .and_then(|item| item.get("type").and_then(Value::as_str))
            == Some("compaction_trigger"),
        "provider usage plus the pending tool tail should cross the BaseCodex limit"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_retry_preserves_stale_projection_until_real_usage() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let partial_reasoning = "r".repeat(180_000);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-retry-baseline"),
                responses::ev_assistant_message("msg-retry-baseline", "baseline"),
                responses::ev_completed_with_tokens("resp-retry-baseline", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-retry-partial"),
                responses::ev_reasoning_item(
                    "reasoning-retry-partial",
                    &["partial"],
                    &[&partial_reasoning],
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-retry-no-usage"),
                responses::ev_function_call("retry-tool", "missing_tool", "{}"),
                completed_without_usage("resp-retry-no-usage"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-retry-compact"),
                ev_compaction_item("compact-retry", "compact-retry-summary"),
                responses::ev_completed_with_tokens("resp-retry-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-retry-done"),
                responses::ev_assistant_message("msg-retry-done", "done"),
                responses::ev_completed_with_tokens("resp-retry-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(40_000);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("establish a retry baseline").await?;
    test.submit_turn("retry after partial model output").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 5);
    assert!(
        requests[3]
            .input()
            .last()
            .and_then(|item| item.get("type").and_then(Value::as_str))
            == Some("compaction_trigger"),
        "a retry without usage must not restore the pre-attempt provider baseline"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(target_os = "windows"))]
async fn responses_lite_cancel_keeps_projection_stale_for_next_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let cancelled_reasoning = "c".repeat(180_000);
    let sleep_args = serde_json::json!({
        "command": "sleep 60",
        "timeout_ms": 60_000
    })
    .to_string();
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-cancel-baseline"),
                responses::ev_assistant_message("msg-cancel-baseline", "baseline"),
                responses::ev_completed_with_tokens("resp-cancel-baseline", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-cancel-large"),
                responses::ev_reasoning_item(
                    "reasoning-cancel-large",
                    &["cancelled"],
                    &[&cancelled_reasoning],
                ),
                responses::ev_function_call("cancel-sleep", "shell_command", &sleep_args),
                completed_without_usage("resp-cancel-large"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-cancel-compact"),
                ev_compaction_item("compact-cancel", "compact-cancel-summary"),
                responses::ev_completed_with_tokens("resp-cancel-compact", 5_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-cancel-done"),
                responses::ev_assistant_message("msg-cancel-done", "done"),
                responses::ev_completed_with_tokens("resp-cancel-done", 5_000),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(40_000);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("establish a cancellation baseline")
        .await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "cancel after model output".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ExecCommandBegin(_))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;

    test.submit_turn("continue after cancellation").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .input()
            .last()
            .and_then(|item| item.get("type").and_then(Value::as_str))
            == Some("compaction_trigger"),
        "cancellation must not restore provider usage for the next turn"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_exposes_spine_tools_as_a_native_namespace() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-tools"),
            responses::ev_completed("resp-tools"),
        ]),
    )
    .await;
    let mut builder = spine_test_codex()
        .with_spine_trim()
        .with_spine_spawn()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable CodeMode");
        });
    let test = builder.build(&server).await?;

    test.submit_turn("inspect Spine tools").await?;

    let body = response_mock.single_request().body_json();
    let tools = additional_tools(&body)?;
    for tool_name in ["open", "close", "next", "trim", "spawn"] {
        assert!(
            has_namespaced_tool(tools, "spine", tool_name),
            "missing spine.{tool_name} native namespace tool"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_exec_batches_ordinary_tools_with_nested_spine_open() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let code = r#"// @exec: {"yield_time_ms": 30000}
const args = () => ({
  sleep_after_ms: 50,
  barrier: {
    id: "spine-code-mode-parallel-open",
    participants: 2,
    timeout_ms: 10_000,
  },
});
const [left, right, opened] = await Promise.all([
  tools.test_sync_tool(args()),
  tools.test_sync_tool(args()),
  tools.spine__open({goal: "nested child"}),
]);
text(JSON.stringify({left, right, opened}));
"#;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-nested-open"),
                responses::ev_custom_tool_call("exec-nested-open", "exec", code),
                responses::ev_completed("resp-nested-open"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-nested-open", "done"),
                responses::ev_completed("resp-nested-open-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
            model_info
                .experimental_supported_tools
                .push("test_sync_tool".to_string());
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable CodeMode");
            config
                .features
                .disable(Feature::SpineStatus)
                .expect("disable legacy SpineStatus");
        });
    let test = builder.build(&server).await?;

    test.submit_turn("batch ordinary tools with Spine open")
        .await?;
    test.codex.flush_rollout().await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let followup = requests[1].body_json().to_string();
    let visible_output = requests[1]
        .custom_tool_call_output("exec-nested-open")
        .get("output")
        .and_then(Value::as_array)
        .context("exec output should preserve content items")?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<String>();
    assert!(
        visible_output.contains(r#""left":"ok""#),
        "{visible_output}"
    );
    assert!(
        visible_output.contains(r#""right":"ok""#),
        "{visible_output}"
    );
    assert!(
        visible_output.contains(r#""opened":"Spine open accepted.""#),
        "{visible_output}"
    );
    assert!(!followup.contains(CODE_MODE_SPINE_CARRIER_MARKER));
    assert!(followup.contains("nested child"), "{followup}");
    let followup_body = requests[1].body_json();
    let statuses = request_spine_transition_statuses(&followup_body);
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].contains(r#"cursor="1.1""#), "{}", statuses[0]);

    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let rollout = std::fs::read_to_string(&rollout_path)?;
    assert!(rollout.contains(CODE_MODE_SPINE_CARRIER_MARKER));
    let carrier = rollout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find_map(|line| match line.item {
            RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput {
                name, output, ..
            }) if name.as_deref() == Some(CODE_MODE_SPINE_CARRIER_MARKER) => output.body.to_text(),
            _ => None,
        })
        .context("raw rollout should contain marked carrier")?;
    let carrier: Value = serde_json::from_str(&carrier)?;
    assert_eq!(carrier["nested_spine_calls"][0]["name"], "open");
    let rollout_lines = rollout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?;
    let carrier_index = rollout_lines
        .iter()
        .position(|line| {
            matches!(
                &line.item,
                RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput { name, .. })
                    if name.as_deref() == Some(CODE_MODE_SPINE_CARRIER_MARKER)
            )
        })
        .context("carrier rollout index")?;
    assert!(matches!(
        rollout_lines.get(carrier_index + 1).map(|line| &line.item),
        Some(RolloutItem::ResponseItem(ResponseItem::Message {
            role,
            content,
            ..
        })) if role == "developer"
            && content.iter().any(|item| matches!(
                item,
                ContentItem::InputText { text }
                    if text.starts_with("<spine_tran_status ")
                        && text.contains(r#"cursor="1.1""#)
            ))
    ));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_exec_runs_nested_open_next_close_lifecycle() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-nested-lifecycle-open"),
                responses::ev_custom_tool_call(
                    "exec-lifecycle-open",
                    "exec",
                    r#"text(await tools.spine__open({goal: "first nested task"}));"#,
                ),
                responses::ev_completed_with_tokens("resp-nested-lifecycle-open", 10_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-nested-lifecycle-next"),
                responses::ev_custom_tool_call(
                    "exec-lifecycle-next",
                    "exec",
                    r#"text(await tools.spine__next({
  goal: "second nested task",
  memory: "first nested task complete"
}));"#,
                ),
                responses::ev_completed_with_tokens("resp-nested-lifecycle-next", 42_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-nested-lifecycle-close"),
                responses::ev_custom_tool_call(
                    "exec-lifecycle-close",
                    "exec",
                    r#"text(await tools.spine__close({memory: "second nested task complete"}));"#,
                ),
                responses::ev_completed_with_tokens("resp-nested-lifecycle-close", 55_000),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-nested-lifecycle", "done"),
                responses::ev_completed("resp-nested-lifecycle-done"),
            ]),
        ],
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable CodeMode");
        });
    let test = builder.build(&server).await?;

    test.submit_turn("run nested Spine lifecycle").await?;
    test.codex.flush_rollout().await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 4);
    let after_open_body = requests[1].body_json();
    let after_open = after_open_body.to_string();
    assert!(after_open.contains("first nested task"), "{after_open}");
    let after_open_statuses = request_spine_transition_statuses(&after_open_body);
    assert_eq!(after_open_statuses.len(), 1);
    assert!(
        after_open_statuses[0].contains(r#"cursor="1.1""#),
        "{}",
        after_open_statuses[0]
    );

    let after_next_body = requests[2].body_json();
    let after_next = after_next_body.to_string();
    assert!(
        after_next.contains("first nested task complete"),
        "{after_next}"
    );
    assert!(after_next.contains("second nested task"), "{after_next}");
    let after_next_statuses = request_spine_transition_statuses(&after_next_body);
    assert_eq!(after_next_statuses.len(), 1);
    assert!(
        after_next_statuses[0].contains(r#"cursor="1.2""#),
        "{}",
        after_next_statuses[0]
    );

    let after_close_body = requests[3].body_json();
    let after_close = after_close_body.to_string();
    assert!(
        after_close.contains("second nested task complete"),
        "{after_close}"
    );
    assert!(!after_close.contains(CODE_MODE_SPINE_CARRIER_MARKER));
    let after_close_statuses = request_spine_transition_statuses(&after_close_body);
    assert_eq!(after_close_statuses.len(), 1);
    assert!(
        after_close_statuses[0].contains(r#"cursor="1""#),
        "{}",
        after_close_statuses[0]
    );
    assert!(
        after_close_statuses[0].contains(r#"cursor_context="45.0K""#),
        "{}",
        after_close_statuses[0]
    );

    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let rollout = std::fs::read_to_string(&rollout_path)?;
    let nested_names = rollout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput {
                name, output, ..
            }) if name.as_deref() == Some(CODE_MODE_SPINE_CARRIER_MARKER) => output.body.to_text(),
            _ => None,
        })
        .map(|body| serde_json::from_str::<Value>(&body))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|carrier| {
            carrier["nested_spine_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|call| call["name"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(nested_names, ["open", "next", "close"]);
    let persisted_statuses = persisted_spine_transition_statuses(&test)?;
    assert_eq!(persisted_statuses.len(), 3);
    for (status, cursor) in persisted_statuses.iter().zip(["1.1", "1.2", "1"]) {
        assert!(
            status.contains(&format!(r#"cursor="{cursor}""#)),
            "{status}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_first_yield_commits_nested_open_and_wait_stays_ordinary() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-nested-yield-open"),
            responses::ev_custom_tool_call(
                "exec-yield-open",
                "exec",
                r#"// @exec: {"yield_time_ms": 30000}
text(await tools.spine__open({goal: "yielded nested task"}));
yield_control();
while (true) {}
"#,
            ),
            responses::ev_completed("resp-nested-yield-open"),
        ]),
    )
    .await;
    let first_completion = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-nested-yield-open", "waiting"),
            responses::ev_completed("resp-nested-yield-open-done"),
        ]),
    )
    .await;
    let mut builder = spine_test_codex()
        .with_model_info_override("gpt-5.4", |model_info| {
            model_info.use_responses_lite = true;
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable CodeMode");
        });
    let test = builder.build(&server).await?;

    test.submit_turn("open then yield").await?;

    let first_request = first_completion.single_request();
    let first_output = first_request.custom_tool_call_output("exec-yield-open");
    let visible_text = match first_output.get("output") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(text)) => text.clone(),
        other => anyhow::bail!("unexpected yielded exec output: {other:?}"),
    };
    assert!(
        visible_text.contains("Spine open accepted."),
        "{visible_text}"
    );
    let cell_id = visible_text
        .split("Script running with cell ID ")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .context("yielded exec must expose its cell id")?
        .to_string();
    let first_body = first_request.body_json().to_string();
    assert!(first_body.contains("yielded nested task"), "{first_body}");
    assert!(!first_body.contains(CODE_MODE_SPINE_CARRIER_MARKER));

    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-nested-yield-wait"),
            responses::ev_function_call(
                "wait-yield-open",
                "wait",
                &serde_json::json!({
                    "cell_id": cell_id,
                    "terminate": true,
                })
                .to_string(),
            ),
            responses::ev_completed("resp-nested-yield-wait"),
        ]),
    )
    .await;
    let second_completion = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-nested-yield-wait", "terminated"),
            responses::ev_completed("resp-nested-yield-wait-done"),
        ]),
    )
    .await;
    test.submit_turn("terminate yielded cell").await?;
    test.codex.flush_rollout().await?;

    let second_request = second_completion.single_request();
    let second_body = second_request.body_json().to_string();
    assert!(second_body.contains("Script terminated"), "{second_body}");
    assert!(!second_body.contains(CODE_MODE_SPINE_CARRIER_MARKER));

    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let rollout = std::fs::read_to_string(&rollout_path)?;
    assert_eq!(
        rollout
            .lines()
            .filter(|line| line.contains(CODE_MODE_SPINE_CARRIER_MARKER))
            .count(),
        1
    );

    Ok(())
}

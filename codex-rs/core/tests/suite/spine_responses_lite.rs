use anyhow::Context;
use anyhow::Result;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_features::Feature;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
#[cfg(not(target_os = "windows"))]
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
#[cfg(not(target_os = "windows"))]
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::spine_test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
#[cfg(not(target_os = "windows"))]
use std::time::Duration;

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
                    r#"{"summary":"first child"}"#,
                ),
                responses::ev_function_call_with_namespace(
                    "direct-open-second",
                    "spine",
                    "open",
                    r#"{"summary":"second child"}"#,
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
                    r#"{"summary":"valid child"}"#,
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
                    r#"{"summary":"blocked child"}"#,
                ),
                responses::ev_function_call_with_namespace(
                    "direct-open-after-hook",
                    "spine",
                    "open",
                    r#"{"summary":"hook survivor"}"#,
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
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_omits_spine_status_tail() -> Result<()> {
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
                    r#"{"summary":"status child"}"#,
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

    test.submit_turn("status tail").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let input = request.input();
        assert!(
            input
                .iter()
                .all(|item| !item.to_string().contains("<spine_status ")),
            "Responses Lite request must not contain a Spine status tail"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_open_rewrite_preserves_body_after_prefix_growth() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let large_sampled_output = "x".repeat(30_000);
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
                    r#"{"summary":"body budget child"}"#,
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
    let first_growth = "a".repeat(30_000);
    let second_growth = "b".repeat(30_000);
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
async fn responses_lite_spine_memory_slots_preserve_context_order() -> Result<()> {
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
                    r#"{"summary":"memory child"}"#,
                ),
                responses::ev_completed("resp-memory-open"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-memory-opened"),
                responses::ev_completed("resp-memory-opened"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-memory-close"),
                responses::ev_function_call_with_namespace(
                    "memory-close",
                    "spine",
                    "close",
                    r#"{"memory":"child complete"}"#,
                ),
                responses::ev_completed("resp-memory-close"),
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
    assert!(child_user.0 < child_summary.0);
    assert!(
        input
            .iter()
            .all(|item| !item.to_string().contains("<spine_status ")),
        "Responses Lite request must not contain a Spine status tail"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_lite_spine_close_rebases_provider_usage_before_auto_compact() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut old_reasoning =
        responses::ev_reasoning_item("closed-child-reasoning", &["old"], &[&"r".repeat(25_000)]);
    old_reasoning["item"]
        .as_object_mut()
        .context("reasoning fixture item should be an object")?
        .remove("content");
    let child_work = vec![
        responses::ev_response_created("resp-rebase-child-work"),
        old_reasoning,
        responses::ev_assistant_message("msg-rebase-child-work", "large child work complete"),
        responses::ev_completed_with_tokens("resp-rebase-child-work", 30_000),
    ];
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-open"),
                responses::ev_function_call_with_namespace(
                    "rebase-open",
                    "spine",
                    "open",
                    r#"{"summary":"large child"}"#,
                ),
                responses::ev_completed_with_tokens("resp-rebase-open", 10_000),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-opened"),
                responses::ev_assistant_message("msg-rebase-opened", "child ready"),
                responses::ev_completed_with_tokens("resp-rebase-opened", 10_000),
            ])),
            responses::sse_response(responses::sse(child_work))
                .insert_header("X-Reasoning-Included", "true"),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-close"),
                responses::ev_function_call_with_namespace(
                    "rebase-close",
                    "spine",
                    "close",
                    r#"{"memory":"short child memory"}"#,
                ),
                responses::ev_completed_with_tokens("resp-rebase-close", 52_000),
            ]))
            .insert_header("X-Reasoning-Included", "true"),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resp-rebase-done"),
                responses::ev_assistant_message("msg-rebase-done", "done"),
                responses::ev_completed_with_tokens("resp-rebase-done", 45_000),
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
            config.model_context_window = Some(80_000);
            config.model_auto_compact_token_limit = Some(50_000);
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
    let bounded_large_user = "u".repeat(30_000);
    let close_arguments = serde_json::json!({
        "memory": "m".repeat(30_000),
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
                    r#"{"summary":"body prefix child"}"#,
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
            config.model_context_window = Some(18_000);
            config.model_auto_compact_token_limit = Some(500_000);
            config.model_auto_compact_token_limit_scope =
                AutoCompactTokenLimitScope::BodyAfterPrefix;
        });
    let test = builder.build(&server).await?;

    test.submit_turn(&bounded_large_user).await?;
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
    let large_reasoning = "n".repeat(30_000);
    let mut large_response = vec![responses::ev_response_created("resp-no-usage-large")];
    large_response.extend((0..6).map(|index| {
        responses::ev_assistant_message(
            &format!("reasoning-no-usage-large-{index}"),
            &large_reasoning,
        )
    }));
    large_response.extend([
        responses::ev_function_call("no-usage-tool", "missing_tool", "{}"),
        completed_without_usage("resp-no-usage-large"),
    ]);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-no-usage-baseline"),
                responses::ev_assistant_message("msg-no-usage-baseline", "baseline"),
                responses::ev_completed_with_tokens("resp-no-usage-baseline", 5_000),
            ]),
            responses::sse(large_response),
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
    let partial_reasoning = "r".repeat(30_000);
    let mut partial_response = vec![responses::ev_response_created("resp-retry-partial")];
    partial_response.extend((0..6).map(|index| {
        responses::ev_assistant_message(
            &format!("reasoning-retry-partial-{index}"),
            &partial_reasoning,
        )
    }));
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-retry-baseline"),
                responses::ev_assistant_message("msg-retry-baseline", "baseline"),
                responses::ev_completed_with_tokens("resp-retry-baseline", 5_000),
            ]),
            responses::sse(partial_response),
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
    let cancelled_reasoning = "c".repeat(30_000);
    let sleep_args = serde_json::json!({
        "command": "sleep 60",
        "timeout_ms": 60_000
    })
    .to_string();
    let mut cancelled_response = vec![responses::ev_response_created("resp-cancel-large")];
    cancelled_response.extend((0..6).map(|index| {
        responses::ev_assistant_message(
            &format!("reasoning-cancel-large-{index}"),
            &cancelled_reasoning,
        )
    }));
    cancelled_response.extend([
        responses::ev_function_call("cancel-sleep", "shell_command", &sleep_args),
        completed_without_usage("resp-cancel-large"),
    ]);
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-cancel-baseline"),
                responses::ev_assistant_message("msg-cancel-baseline", "baseline"),
                responses::ev_completed_with_tokens("resp-cancel-baseline", 5_000),
            ]),
            responses::sse(cancelled_response),
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
    for tool_name in ["open", "close", "next"] {
        assert!(
            has_namespaced_tool(tools, "spine", tool_name),
            "missing spine.{tool_name} native namespace tool"
        );
    }
    let exec_description = tools
        .iter()
        .find(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .and_then(|tool| tool.get("description"))
        .and_then(Value::as_str)
        .context("Responses Lite request should contain the exec tool")?;
    assert!(!exec_description.contains("spine__open"));
    assert!(!exec_description.contains("spine__close"));
    assert!(!exec_description.contains("spine__next"));

    Ok(())
}

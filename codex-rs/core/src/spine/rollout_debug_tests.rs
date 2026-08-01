use codex_protocol::protocol::RolloutLine;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::DebugRolloutRecord;
use super::RolloutDebugRedactor;
use super::RolloutDebugRedactorError;

const SECRET: &str = "private-seed-7c84f24b";

fn line(item_type: &str, payload: Value) -> Value {
    json!({
        "timestamp": format!("{SECRET}-timestamp"),
        "type": item_type,
        "payload": payload,
    })
}

fn redact(redactor: &mut RolloutDebugRedactor, value: Value) -> Value {
    serde_json::to_value(redactor.redact_value(value)).expect("debug record serializes")
}

fn redact_public(
    redactor: &mut RolloutDebugRedactor,
    value: Value,
) -> Result<Value, RolloutDebugRedactorError> {
    let line = serde_json::to_vec(&value).expect("rollout line serializes");
    redactor.redact_json_line_to_value(&line)
}

fn assert_secret_absent(value: &Value) {
    let encoded = serde_json::to_string(value).expect("value serializes");
    assert!(
        !encoded.contains(SECRET),
        "redacted record retained the seeded secret: {encoded}"
    );
}

#[test]
fn malformed_unknown_and_oversized_records_are_positional_placeholders() {
    let mut redactor = RolloutDebugRedactor::default();
    let malformed = redactor.redact_json_line(br#"{"timestamp":"x""#);
    assert_eq!(
        serde_json::to_value(malformed).expect("record serializes"),
        json!({"record_type": "malformed_redacted", "scope": "line"})
    );

    let unknown = redact(
        &mut redactor,
        line(
            "response_item",
            json!({"type": "future_secret_item", "content": SECRET}),
        ),
    );
    assert_eq!(
        unknown,
        json!({"record_type": "unknown_redacted", "scope": "response_item"})
    );
    assert_eq!(
        serde_json::to_value(DebugRolloutRecord::oversized()).expect("record serializes"),
        json!({"record_type": "oversized_redacted"})
    );
}

#[test]
fn unknown_top_level_and_event_variants_are_distinct_from_malformed_records() {
    let mut redactor = RolloutDebugRedactor::default();
    let top_level = redact(
        &mut redactor,
        line(
            "future_secret_item",
            json!({"content": SECRET, "path": format!("/{SECRET}")}),
        ),
    );
    assert_eq!(
        top_level,
        json!({"record_type": "unknown_redacted", "scope": "top_level"})
    );

    let event = redact(
        &mut redactor,
        line(
            "event_msg",
            json!({"type": "future_secret_event", "content": SECRET}),
        ),
    );
    assert_eq!(
        event,
        json!({"record_type": "unknown_redacted", "scope": "event"})
    );
}

#[test]
fn message_content_and_raw_identifiers_never_survive() {
    let mut redactor = RolloutDebugRedactor::default();
    let first = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "message",
                "id": format!("{SECRET}-item"),
                "role": "user",
                "content": [
                    {"type": "input_text", "text": SECRET},
                    {"type": "input_image", "image_url": format!("file:///{SECRET}.png")}
                ],
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": format!("{SECRET}-turn")
                }
            }),
        ),
    );
    let second = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "message",
                "id": format!("{SECRET}-item"),
                "role": format!("{SECRET}-role"),
                "content": [{"type": "output_text", "text": SECRET}],
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": format!("{SECRET}-turn")
                }
            }),
        ),
    );

    assert_secret_absent(&first);
    assert_secret_absent(&second);
    assert_eq!(first["item"]["id"], second["item"]["id"]);
    assert_eq!(first["item"]["turn_id"], second["item"]["turn_id"]);
    assert_eq!(first["item"]["role"], "user");
    assert_eq!(second["item"]["role"], "other");
    assert_eq!(
        first["item"]["content"],
        json!(["input_text", "input_image"])
    );
}

#[test]
fn direct_control_shapes_preserve_invalidity_and_exact_success() {
    let mut redactor = RolloutDebugRedactor::default();
    let request = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "function_call",
                "namespace": "spine",
                "name": "next",
                "arguments": serde_json::to_string(&json!({
                    "summary": " ",
                    "memory": SECRET,
                    "unexpected": SECRET
                })).expect("arguments serialize"),
                "call_id": format!("{SECRET}-call")
            }),
        ),
    );
    assert_secret_absent(&request);
    assert_eq!(request["item"]["tool"], "spine_next");
    assert_eq!(request["item"]["arguments"]["summary"], "whitespace");
    assert_eq!(request["item"]["arguments"]["memory"], "non_empty");
    assert_eq!(request["item"]["arguments"]["unknown_fields"], true);
    assert_eq!(request["item"]["arguments"]["valid"], false);

    let accepted = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "function_call_output",
                "call_id": format!("{SECRET}-call"),
                "output": "Spine next accepted."
            }),
        ),
    );
    let _ = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "function_call",
                "namespace": "spine",
                "name": "next",
                "arguments": serde_json::to_string(&json!({
                    "summary": " ",
                    "memory": SECRET,
                    "unexpected": SECRET
                })).expect("arguments serialize"),
                "call_id": format!("{SECRET}-near-miss-call")
            }),
        ),
    );
    let near_miss = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "function_call_output",
                "call_id": format!("{SECRET}-near-miss-call"),
                "output": " Spine next accepted. "
            }),
        ),
    );
    assert_eq!(accepted["item"]["output"]["exact_success_carrier"], true);
    assert_eq!(near_miss["item"]["output"]["exact_success_carrier"], false);
}

#[test]
fn direct_control_argument_classification_matrix_is_preserved() {
    let cases = [
        ("open", json!({}), false),
        ("open", json!({"summary": ""}), false),
        ("open", json!({"summary": " \n"}), false),
        ("open", json!({"summary": 7}), false),
        ("open", json!({"summary": SECRET}), true),
        ("close", json!({"memory": ""}), false),
        ("close", json!({"memory": SECRET}), true),
        ("next", json!({"summary": SECRET, "memory": false}), false),
        ("next", json!({"summary": SECRET, "memory": SECRET}), true),
        (
            "next",
            json!({"summary": SECRET, "memory": SECRET, "extra": SECRET}),
            false,
        ),
        ("spawn", json!({"tasks": []}), false),
        (
            "spawn",
            json!({
                "tasks": [
                    {"summary": SECRET, "prompt": SECRET},
                    {"summary": format!("{SECRET}-2"), "prompt": SECRET}
                ]
            }),
            true,
        ),
        (
            "spawn",
            json!({
                "tasks": [
                    {"summary": SECRET, "prompt": SECRET, "extra": SECRET},
                    {"summary": SECRET, "prompt": SECRET}
                ]
            }),
            false,
        ),
    ];

    for (index, (name, arguments, expected_valid)) in cases.into_iter().enumerate() {
        let mut redactor = RolloutDebugRedactor::default();
        let output = redact(
            &mut redactor,
            line(
                "response_item",
                json!({
                    "type": "function_call",
                    "namespace": "spine",
                    "name": name,
                    "arguments": serde_json::to_string(&arguments)
                        .expect("arguments serialize"),
                    "call_id": format!("{SECRET}-call-{index}")
                }),
            ),
        );
        assert_secret_absent(&output);
        assert_eq!(
            output["item"]["arguments"]["valid"], expected_valid,
            "unexpected classification for {name}: {arguments}"
        );
    }
}

#[test]
fn spawn_unknown_schema_is_not_repaired() {
    let mut redactor = RolloutDebugRedactor::default();
    let call_id = format!("{SECRET}-spawn");
    let request = line(
        "response_item",
        json!({
            "type": "function_call",
            "namespace": "spine",
            "name": "spawn",
            "arguments": serde_json::to_string(&json!({
                "tasks": [
                    {"summary": SECRET, "prompt": SECRET},
                    {"summary": format!("{SECRET}-2"), "prompt": format!("{SECRET}-2")}
                ]
            })).expect("arguments serialize"),
            "call_id": call_id,
        }),
    );
    let _ = redact(&mut redactor, request);

    let output = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "function_call_output",
                "call_id": format!("{SECRET}-spawn"),
                "output": serde_json::to_string(&json!({
                    "schema": "spine.spawn.result.v2",
                    "results": [
                        {
                            "ordinal": 0,
                            "outcome": "completed",
                            "memory_body": SECRET
                        },
                        {
                            "ordinal": 1,
                            "outcome": "completed",
                            "memory_body": SECRET
                        }
                    ]
                })).expect("receipt serializes")
            }),
        ),
    );
    assert_secret_absent(&output);
    assert_eq!(output["item"]["output"]["receipt"]["schema"], "other");
    assert_eq!(
        output["item"]["output"]["receipt"]["valid_for_request"],
        false
    );
    assert_eq!(
        output["item"]["output"]["receipt"]["results"]["items"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}

#[test]
fn malformed_code_mode_boolean_fields_keep_their_shape() {
    let mut redactor = RolloutDebugRedactor::default();
    let call_id = format!("{SECRET}-exec");
    let _ = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": call_id,
                "input": SECRET
            }),
        ),
    );
    let malformed_carrier = serde_json::to_string(&json!({
        "schema": "spine.code_mode.output.v1",
        "visible_body": SECRET,
        "outer_success": "false",
        "cell_id": format!("{SECRET}-cell"),
        "nested_spine_calls": [{
            "runtime_call_id": format!("{SECRET}-runtime"),
            "invocation_ordinal": 0,
            "name": "open",
            "arguments": serde_json::to_string(&json!({"summary": SECRET}))
                .expect("arguments serialize"),
            "output": {"success": "false", "body": "Spine open accepted."}
        }]
    }))
    .expect("carrier serializes");
    let output = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "custom_tool_call_output",
                "call_id": format!("{SECRET}-exec"),
                "name": "spine.code_mode.output.v1",
                "output": malformed_carrier
            }),
        ),
    );

    assert_secret_absent(&output);
    let inspection = &output["item"]["output"]["carrier"]["inspection"];
    assert_eq!(inspection["schema"], "exact_v1");
    assert_eq!(inspection["outer_success"], "wrong_type");
    assert_eq!(
        inspection["nested_calls"]["items"][0]["output_success"],
        "wrong_type"
    );
}

#[test]
fn valid_code_mode_keeps_order_control_shape_and_boolean_values() {
    let mut redactor = RolloutDebugRedactor::default();
    let call_id = format!("{SECRET}-exec-valid");
    let _ = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": call_id,
                "input": SECRET
            }),
        ),
    );
    let carrier = serde_json::to_string(&json!({
        "schema": "spine.code_mode.output.v1",
        "visible_body": SECRET,
        "outer_success": false,
        "cell_id": format!("{SECRET}-cell"),
        "nested_spine_calls": [{
            "runtime_call_id": format!("{SECRET}-runtime-0"),
            "invocation_ordinal": 0,
            "name": "trim",
            "arguments": serde_json::to_string(&json!({
                "TRIM_ID": format!("{SECRET}-trim"),
                "op": "snip"
            })).expect("arguments serialize"),
            "output": {"success": false, "body": SECRET}
        }, {
            "runtime_call_id": format!("{SECRET}-runtime-1"),
            "invocation_ordinal": 1,
            "name": "open",
            "arguments": serde_json::to_string(&json!({
                "summary": SECRET
            })).expect("arguments serialize"),
            "output": {"success": true, "body": "Spine open accepted."}
        }]
    }))
    .expect("carrier serializes");
    let output = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "custom_tool_call_output",
                "call_id": format!("{SECRET}-exec-valid"),
                "name": "spine.code_mode.output.v1",
                "output": carrier
            }),
        ),
    );

    assert_secret_absent(&output);
    let carrier = &output["item"]["output"]["carrier"];
    assert_eq!(carrier["state"], "valid");
    assert_eq!(carrier["outer_success"], "false");
    assert_eq!(carrier["nested_calls"][0]["tool"], "spine_trim");
    assert_eq!(carrier["nested_calls"][0]["success"], false);
    assert_eq!(carrier["nested_calls"][1]["tool"], "spine_open");
    assert_eq!(
        carrier["nested_calls"][1]["output"]["exact_success_carrier"],
        true
    );
}

#[test]
fn code_mode_valid_controls_and_spawn_keep_classification() {
    let valid_spawn_receipt = serde_json::to_string(&json!({
        "schema": "spine.spawn.result.v1",
        "results": [{
            "ordinal": 0,
            "outcome": "completed",
            "memory_body": SECRET,
            "diagnostic": null,
            "execution_ref": null
        }, {
            "ordinal": 1,
            "outcome": "completed",
            "memory_body": SECRET,
            "diagnostic": null,
            "execution_ref": null
        }]
    }))
    .expect("spawn receipt serializes");
    let cases = [
        (
            "open",
            json!({"summary": SECRET}),
            "Spine open accepted.".to_string(),
        ),
        (
            "close",
            json!({"memory": SECRET}),
            "Spine close accepted.".to_string(),
        ),
        (
            "next",
            json!({"summary": SECRET, "memory": SECRET}),
            "Spine next accepted.".to_string(),
        ),
        (
            "spawn",
            json!({
                "tasks": [
                    {"summary": SECRET, "prompt": SECRET},
                    {"summary": format!("{SECRET}-2"), "prompt": SECRET}
                ]
            }),
            valid_spawn_receipt,
        ),
    ];

    for (index, (name, arguments, body)) in cases.into_iter().enumerate() {
        let mut redactor = RolloutDebugRedactor::default();
        let call_id = format!("{SECRET}-exec-{index}");
        let _ = redact(
            &mut redactor,
            line(
                "response_item",
                json!({
                    "type": "custom_tool_call",
                    "name": "exec",
                    "call_id": call_id,
                    "input": SECRET
                }),
            ),
        );
        let carrier = serde_json::to_string(&json!({
            "schema": "spine.code_mode.output.v1",
            "visible_body": SECRET,
            "outer_success": true,
            "cell_id": format!("{SECRET}-cell-{index}"),
            "nested_spine_calls": [{
                "runtime_call_id": format!("{SECRET}-runtime-{index}"),
                "invocation_ordinal": 0,
                "name": name,
                "arguments": serde_json::to_string(&arguments)
                    .expect("arguments serialize"),
                "output": {"success": true, "body": body}
            }]
        }))
        .expect("carrier serializes");
        let output = redact(
            &mut redactor,
            line(
                "response_item",
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": format!("{SECRET}-exec-{index}"),
                    "name": "spine.code_mode.output.v1",
                    "output": carrier
                }),
            ),
        );

        assert_secret_absent(&output);
        let nested = &output["item"]["output"]["carrier"]["nested_calls"][0];
        assert_eq!(nested["arguments"]["valid"], true, "{name}");
        if name == "spawn" {
            assert_eq!(nested["output"]["receipt"]["valid_for_request"], true);
        } else {
            assert_eq!(nested["output"]["exact_success_carrier"], true, "{name}");
        }
    }
}

#[test]
fn token_usage_survives_but_rate_limit_identity_does_not() {
    let mut redactor = RolloutDebugRedactor::default();
    let output = redact(
        &mut redactor,
        line(
            "event_msg",
            json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 11,
                        "cached_input_tokens": 12,
                        "output_tokens": 13,
                        "reasoning_output_tokens": 14,
                        "total_tokens": 50
                    },
                    "last_token_usage": {
                        "input_tokens": 1,
                        "cached_input_tokens": 2,
                        "output_tokens": 3,
                        "reasoning_output_tokens": 4,
                        "total_tokens": 10
                    },
                    "model_context_window": 272000
                },
                "rate_limits": {
                    "limit_id": SECRET,
                    "limit_name": SECRET,
                    "primary": null,
                    "secondary": null,
                    "credits": null,
                    "individual_limit": null,
                    "plan_type": null,
                    "rate_limit_reached_type": null
                }
            }),
        ),
    );

    assert_secret_absent(&output);
    assert_eq!(
        output["event"]["token_usage"]["total"]["cached_input_tokens"],
        12
    );
    assert_eq!(
        output["event"]["token_usage"]["last"]["reasoning_output_tokens"],
        4
    );
    assert_eq!(
        output["event"]["token_usage"]["model_context_window"],
        272000
    );
    assert!(output["event"].get("rate_limits").is_none());
}

#[test]
fn compact_replacement_is_recursively_redacted_and_not_replayable() {
    let mut redactor = RolloutDebugRedactor::default();
    let output = redact(
        &mut redactor,
        line(
            "compacted",
            json!({
                "message": SECRET,
                "replacement_history": [{
                    "type": "message",
                    "id": format!("{SECRET}-item"),
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": SECRET}]
                }],
                "window_number": 9,
                "first_window_id": format!("{SECRET}-window-a"),
                "previous_window_id": format!("{SECRET}-window-b"),
                "window_id": format!("{SECRET}-window-c")
            }),
        ),
    );

    assert_secret_absent(&output);
    assert_eq!(output["window_number"], 9);
    assert_eq!(
        output["replacement_history"][0]["content"],
        json!(["output_text"])
    );
    assert!(serde_json::from_value::<RolloutLine>(output).is_err());
}

#[test]
fn identifier_state_limits_fail_closed_without_double_counting() {
    let mut entry_limited = RolloutDebugRedactor::with_limits(1024, 1, 8);
    assert_eq!(entry_limited.register_thread_id("thread-a"), Ok(0));
    assert_eq!(entry_limited.register_thread_id("thread-a"), Ok(0));
    assert_eq!(
        entry_limited.register_thread_id("thread-b"),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );
    assert_eq!(
        entry_limited.register_thread_id("thread-a"),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );

    let mut byte_limited = RolloutDebugRedactor::with_limits(8, 8, 8);
    assert_eq!(byte_limited.register_thread_id("12345678"), Ok(0));
    assert_eq!(
        byte_limited.register_thread_id("9"),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );
}

#[test]
fn pending_call_limit_fails_closed_and_completed_output_releases_slot() {
    let function_call = |call_id: &str| {
        line(
            "response_item",
            json!({
                "type": "function_call",
                "namespace": "spine",
                "name": "open",
                "arguments": serde_json::to_string(&json!({"summary": SECRET}))
                    .expect("arguments serialize"),
                "call_id": call_id
            }),
        )
    };
    let function_output = |call_id: &str| {
        line(
            "response_item",
            json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Spine open accepted."
            }),
        )
    };

    let mut pending_limited = RolloutDebugRedactor::with_limits(4096, 16, 1);
    let first =
        redact_public(&mut pending_limited, function_call("private-call-a")).expect("first call");
    assert_secret_absent(&first);
    assert_eq!(
        redact_public(&mut pending_limited, function_call("private-call-b")),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );

    let mut reusable = RolloutDebugRedactor::with_limits(4096, 16, 1);
    redact_public(&mut reusable, function_call("private-call-a")).expect("first call");
    let output =
        redact_public(&mut reusable, function_output("private-call-a")).expect("first output");
    assert_secret_absent(&output);
    let second =
        redact_public(&mut reusable, function_call("private-call-b")).expect("released slot");
    assert_secret_absent(&second);
}

#[test]
fn structural_node_limits_fail_closed_before_high_cardinality_expansion() {
    let mut production_limits = RolloutDebugRedactor::default();
    assert_eq!(
        redact_public(
            &mut production_limits,
            line(
                "response_item",
                json!({
                    "type": "function_call",
                    "namespace": "spine",
                    "name": "spawn",
                    "arguments": serde_json::to_string(&json!({
                        "tasks": vec![json!(0); 70_000]
                    }))
                    .expect("spawn arguments serialize"),
                    "call_id": "spawn-production-limit"
                }),
            )
        ),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );

    let cases = [
        line(
            "response_item",
            json!({
                "type": "function_call",
                "namespace": "spine",
                "name": "spawn",
                "arguments": serde_json::to_string(&json!({
                    "tasks": vec![json!(0); 256]
                }))
                .expect("spawn arguments serialize"),
                "call_id": "spawn-many"
            }),
        ),
        line(
            "compacted",
            json!({
                "message": SECRET,
                "replacement_history": vec![
                    json!({
                        "type": "message",
                        "role": "assistant",
                        "content": []
                    });
                    64
                ]
            }),
        ),
        line(
            "response_item",
            json!({
                "type": "custom_tool_call_output",
                "call_id": "code-mode-many",
                "name": "spine.code_mode.output.v1",
                "output": serde_json::to_string(&json!({
                    "schema": "spine.code_mode.output.v1",
                    "visible_body": "",
                    "outer_success": false,
                    "cell_id": "cell",
                    "nested_spine_calls": vec![json!(0); 256]
                }))
                .expect("code mode carrier serializes")
            }),
        ),
    ];

    for value in cases {
        let mut redactor = RolloutDebugRedactor::with_json_node_limits(128, 4096);
        assert_eq!(
            redact_public(&mut redactor, value),
            Err(RolloutDebugRedactorError::ResourceLimitExceeded)
        );
        assert_eq!(
            redact_public(&mut redactor, line("world_state", json!({"full": true}))),
            Err(RolloutDebugRedactorError::ResourceLimitExceeded),
            "the first structural failure must latch for the package"
        );
    }
}

#[test]
fn later_nested_spawn_stops_after_structural_preflight_failure() {
    let oversized_nested_arguments = serde_json::to_string(&json!({
        "tasks": vec![
            json!({
                "summary": "branch",
                "prompt": "work"
            });
            64
        ]
    }))
    .expect("nested spawn arguments serialize");
    let carrier = serde_json::to_string(&json!({
        "schema": "spine.code_mode.output.v1",
        "visible_body": "",
        "outer_success": true,
        "cell_id": "cell",
        "nested_spine_calls": [
            {
                "runtime_call_id": "nested-spawn",
                "invocation_ordinal": 0,
                "name": "trim",
                "arguments": oversized_nested_arguments,
                "output": {
                    "success": false,
                    "body": "trim rejected"
                }
            },
            {
                "runtime_call_id": "nested-after-failure",
                "invocation_ordinal": 1,
                "name": "spawn",
                "arguments": "{\"tasks\":[{\"summary\":\"a\",\"prompt\":\"a\"},{\"summary\":\"b\",\"prompt\":\"b\"}]}",
                "output": {
                    "success": true,
                    "body": "{\"schema\":\"spine.spawn.result.v1\",\"results\":[]}"
                }
            }
        ]
    }))
    .expect("Code Mode carrier serializes");
    let mut redactor = RolloutDebugRedactor::with_json_node_limits(256, 4096);

    assert_eq!(
        redact_public(
            &mut redactor,
            line(
                "response_item",
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": "code-mode-nested-spawn",
                    "name": "spine.code_mode.output.v1",
                    "output": carrier
                }),
            )
        ),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );
    assert_eq!(
        redact_public(&mut redactor, line("world_state", json!({"full": true}))),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded),
        "the nested argument preflight failure must latch for the package"
    );
}

#[test]
fn nested_spawn_receipt_and_package_node_budgets_fail_closed() {
    let mut receipt_limited = RolloutDebugRedactor::with_json_node_limits(128, 4096);
    redact_public(
        &mut receipt_limited,
        line(
            "response_item",
            json!({
                "type": "function_call",
                "namespace": "spine",
                "name": "spawn",
                "arguments": serde_json::to_string(&json!({
                    "tasks": [
                        {"summary": "a", "prompt": "a"},
                        {"summary": "b", "prompt": "b"}
                    ]
                }))
                .expect("spawn arguments serialize"),
                "call_id": "spawn-receipt-many"
            }),
        ),
    )
    .expect("small request fits");
    assert_eq!(
        redact_public(
            &mut receipt_limited,
            line(
                "response_item",
                json!({
                    "type": "function_call_output",
                    "call_id": "spawn-receipt-many",
                    "output": serde_json::to_string(&json!({
                        "schema": "spine.spawn.result.v1",
                        "results": vec![json!(0); 256]
                    }))
                    .expect("spawn receipt serializes")
                }),
            )
        ),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );

    let mut package_limited = RolloutDebugRedactor::with_json_node_limits(128, 12);
    redact_public(
        &mut package_limited,
        line("world_state", json!({"full": true})),
    )
    .expect("first small record fits");
    assert_eq!(
        redact_public(
            &mut package_limited,
            line("world_state", json!({"full": true}))
        ),
        Err(RolloutDebugRedactorError::ResourceLimitExceeded)
    );
}

#[test]
fn duplicate_outstanding_call_id_is_ambiguous_but_completed_id_can_be_reused() {
    let function_call = |name: &str, namespace: Option<&str>, call_id: &str| {
        line(
            "response_item",
            json!({
                "type": "function_call",
                "namespace": namespace,
                "name": name,
                "arguments": serde_json::to_string(&json!({"summary": SECRET}))
                    .expect("arguments serialize"),
                "call_id": call_id
            }),
        )
    };
    let function_output = |call_id: &str| {
        line(
            "response_item",
            json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Spine open accepted."
            }),
        )
    };

    let mut ambiguous = RolloutDebugRedactor::default();
    redact_public(
        &mut ambiguous,
        function_call("shell_command", None, "duplicate-call"),
    )
    .expect("first outstanding call");
    assert_eq!(
        redact_public(
            &mut ambiguous,
            function_call("open", Some("spine"), "duplicate-call")
        ),
        Err(RolloutDebugRedactorError::AmbiguousCallId)
    );
    assert_eq!(
        redact_public(&mut ambiguous, function_output("duplicate-call")),
        Err(RolloutDebugRedactorError::AmbiguousCallId)
    );

    let mut reusable = RolloutDebugRedactor::default();
    redact_public(
        &mut reusable,
        function_call("open", Some("spine"), "reused-call"),
    )
    .expect("first call");
    redact_public(&mut reusable, function_output("reused-call")).expect("completed output");
    redact_public(
        &mut reusable,
        function_call("open", Some("spine"), "reused-call"),
    )
    .expect("completed call id can be reused");
}

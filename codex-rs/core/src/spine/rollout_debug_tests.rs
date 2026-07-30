use codex_protocol::protocol::RolloutLine;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::DebugRolloutRecord;
use super::RolloutDebugRedactor;

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
    let near_miss = redact(
        &mut redactor,
        line(
            "response_item",
            json!({
                "type": "function_call_output",
                "call_id": format!("{SECRET}-call"),
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
                    {"summary": SECRET, "prompt": SECRET}
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
                    {"summary": SECRET, "prompt": SECRET}
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

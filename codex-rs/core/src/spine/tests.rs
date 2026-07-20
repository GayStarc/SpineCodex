use super::*;
use crate::tools::code_mode::spine_bridge::CODE_MODE_SPINE_CARRIER_MARKER;
use crate::tools::code_mode::spine_bridge::CodeModeOutputCarrierV1;
use crate::tools::code_mode::spine_bridge::NestedSpineCallV1;
use crate::tools::code_mode::spine_bridge::NestedSpineOutputV1;
use crate::tools::code_mode::spine_bridge::NestedSpineToolName;
use crate::tools::code_mode::spine_bridge::encode_carrier;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::WorldStateItem;
use pretty_assertions::assert_eq;

fn message(role: &str, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn call(call_id: &str, name: &str, arguments: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_call(call_id: &str, name: &str, input: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: call_id.to_string(),
        name: name.to_string(),
        namespace: None,
        input: input.to_string(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn namespaced_call(call_id: &str, namespace: &str, name: &str, arguments: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: Some(namespace.to_string()),
        arguments: arguments.to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn output(call_id: &str, success: Option<bool>, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text.to_string()),
            success,
        },
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_output(call_id: &str, success: Option<bool>, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.to_string(),
        name: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text.to_string()),
            success,
        },
        internal_chat_message_metadata_passthrough: None,
    })
}

fn nested_spine_call(
    ordinal: u64,
    name: NestedSpineToolName,
    arguments: &str,
    body: &str,
) -> NestedSpineCallV1 {
    NestedSpineCallV1 {
        runtime_call_id: format!("runtime-{ordinal}"),
        invocation_ordinal: ordinal,
        name,
        arguments: arguments.to_string(),
        output: NestedSpineOutputV1 {
            success: true,
            body: body.to_string(),
        },
    }
}

fn code_mode_carrier_output(
    call_id: &str,
    visible_body: FunctionCallOutputBody,
    nested_spine_calls: Vec<NestedSpineCallV1>,
) -> RolloutItem {
    code_mode_carrier_output_with_success(call_id, visible_body, nested_spine_calls, Some(true))
}

fn code_mode_carrier_output_with_success(
    call_id: &str,
    visible_body: FunctionCallOutputBody,
    nested_spine_calls: Vec<NestedSpineCallV1>,
    success: Option<bool>,
) -> RolloutItem {
    let carrier = CodeModeOutputCarrierV1::new(
        visible_body,
        success,
        "cell-1".to_string(),
        nested_spine_calls,
    )
    .unwrap();
    RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.to_string(),
        name: Some(CODE_MODE_SPINE_CARRIER_MARKER.to_string()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(encode_carrier(&carrier).unwrap()),
            success,
        },
        internal_chat_message_metadata_passthrough: None,
    })
}

fn malformed_code_mode_carrier_output(call_id: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.to_string(),
        name: Some(CODE_MODE_SPINE_CARRIER_MARKER.to_string()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("{not-json".to_string()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    })
}

fn spine_success_output(call_id: &str, tool: tool_response::SpineToolResponse) -> RolloutItem {
    let output = tool.success();
    let success = output.success;
    self::output(call_id, success, &output.into_text())
}

#[test]
fn code_mode_spine_admission_requires_a_sole_outer_exec_call() {
    assert!(
        validate_code_mode_spine_outer_exec(
            &[custom_call("exec-1", "exec", "text('ok')")],
            "exec-1",
        )
        .is_ok()
    );
    for rollout in [
        vec![
            call("ordinary", "shell", r#"{"cmd":"pwd"}"#),
            custom_call("exec-1", "exec", "text('ok')"),
        ],
        vec![
            custom_call("exec-1", "exec", "text('ok')"),
            namespaced_call("open-1", "spine", "open", r#"{"summary":"child"}"#),
        ],
    ] {
        assert!(validate_code_mode_spine_outer_exec(&rollout, "exec-1").is_err());
    }
}

#[test]
fn code_mode_carrier_applies_nested_open_and_restores_exact_visible_body() {
    let visible_body = FunctionCallOutputBody::ContentItems(vec![
        codex_protocol::models::FunctionCallOutputContentItem::InputText {
            text: "visible".to_string(),
        },
        codex_protocol::models::FunctionCallOutputContentItem::InputImage {
            image_url: "data:image/png;base64,AA==".to_string(),
            detail: None,
        },
    ]);
    let rollout = vec![
        custom_call("exec-1", "exec", "text('visible')"),
        code_mode_carrier_output(
            "exec-1",
            visible_body.clone(),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"nested child"}"#,
                "Spine open accepted.",
            )],
        ),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1.1");
    let ResponseItem::CustomToolCallOutput { name, output, .. } = &projection.context[2] else {
        panic!("expected projected exec output");
    };
    assert_eq!(name, &None);
    assert_eq!(output.body, visible_body);

    let raw = response_items(&rollout);
    let ResponseItem::CustomToolCallOutput { name, output, .. } = &raw[1] else {
        panic!("expected raw carrier");
    };
    assert_eq!(name.as_deref(), Some(CODE_MODE_SPINE_CARRIER_MARKER));
    assert_ne!(output.body, visible_body);
}

#[test]
fn code_mode_carrier_applies_nested_trim_to_the_previous_native_toolcall() {
    let original = trim_candidate_text("0123456789\n");
    let rollout = vec![
        call("ordinary", "shell", r#"{"cmd":"large"}"#),
        output("ordinary", Some(true), &original),
        custom_call("exec-1", "exec", "tools.spine.trim({})"),
        code_mode_carrier_output(
            "exec-1",
            FunctionCallOutputBody::Text("trim staged".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Trim,
                r#"{"TRIM_ID":"trim_1","op":"snip"}"#,
                "Spine trim accepted.",
            )],
        ),
    ];

    let projection = derive_from_rollout_with_features(&rollout, true, true, true);
    let ordinary_output = projection
        .context
        .iter()
        .find(|item| {
            matches!(
                item,
                ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "ordinary"
            )
        })
        .expect("ordinary output");
    assert_eq!(output_text(ordinary_output), TOOL_RESULT_CLEARED_MESSAGE);
    let exec_output = projection
        .context
        .iter()
        .find(|item| {
            matches!(
                item,
                ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == "exec-1"
            )
        })
        .expect("exec output");
    assert_eq!(output_text(exec_output), "trim staged");
}

#[test]
fn code_mode_carrier_imports_nested_spawn_without_exposing_the_receipt() {
    let receipt = spawn_receipt();
    let rollout = vec![
        custom_call("exec-1", "exec", "await tools.spine.spawn({tasks})"),
        code_mode_carrier_output(
            "exec-1",
            FunctionCallOutputBody::Text(r#"{"status":"success"}"#.to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Spawn,
                &spawn_arguments(),
                &receipt,
            )],
        ),
    ];

    let projection = derive_from_rollout_with_features(&rollout, true, false, true);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(
        projection
            .spine
            .nodes
            .iter()
            .filter(|node| node.kind == codex_spine_core::NodeKind::Task)
            .count(),
        2
    );
    assert_eq!(
        output_text(&projection.context[1]),
        r#"{"status":"success"}"#
    );
    assert!(projection.context.iter().all(|item| {
        serde_json::to_string(item).is_ok_and(|text| !text.contains("spine.spawn.result.v1"))
    }));
}

#[test]
fn valid_carrier_verdict_contains_fully_analyzed_nested_calls() {
    let request = custom_call("exec-1", "exec", "tools.spine.open({})");
    let response = code_mode_carrier_output_with_success(
        "exec-1",
        FunctionCallOutputBody::Text("visible".to_string()),
        vec![nested_spine_call(
            3,
            NestedSpineToolName::Open,
            r#"{"summary":"analyzed task"}"#,
            "Spine open accepted.",
        )],
        None,
    );
    let RolloutItem::ResponseItem(request) = &request else {
        panic!("expected request item");
    };
    let RolloutItem::ResponseItem(response) = &response else {
        panic!("expected response item");
    };

    let CarrierGroupVerdict::Valid(analysis) = code_mode_carrier_verdict(&[request], &[response])
    else {
        panic!("expected a valid analyzed carrier");
    };
    assert_eq!(analysis.outer_call_id, "exec-1");
    assert_eq!(
        analysis.visible_output,
        FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("visible".to_string()),
            success: None,
        }
    );
    assert_eq!(analysis.nested_calls.len(), 1);
    let nested = &analysis.nested_calls[0];
    assert_eq!(nested.invocation_ordinal, 3);
    assert_eq!(nested.tool_name, "spine.open");
    assert_eq!(nested.arguments, r#"{"summary":"analyzed task"}"#);
    assert_eq!(nested.output, "Spine open accepted.");
    assert!(nested.success);
}

#[test]
fn invalid_nested_call_does_not_partially_transition() {
    let rollout = vec![
        custom_call("exec-1", "exec", "tools.spine.open({})"),
        code_mode_carrier_output(
            "exec-1",
            FunctionCallOutputBody::Text("visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"","extra":true}"#,
                "Spine open accepted.",
            )],
        ),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(
        output_text(&projection.context[1]),
        "Code Mode Spine evidence is invalid and was not applied."
    );
}

#[test]
fn invalid_nested_success_output_fails_the_marked_carrier_closed() {
    let rollout = vec![
        custom_call("exec-1", "exec", "tools.spine.open({})"),
        code_mode_carrier_output(
            "exec-1",
            FunctionCallOutputBody::Text("visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"valid task"}"#,
                "forged success",
            )],
        ),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(
        output_text(&projection.context[1]),
        "Code Mode Spine evidence is invalid and was not applied."
    );
}

#[test]
fn orphaned_marked_carrier_projects_a_failure_instead_of_raw_evidence() {
    let rollout = vec![code_mode_carrier_output(
        "missing-exec",
        FunctionCallOutputBody::Text("visible".to_string()),
        vec![nested_spine_call(
            0,
            NestedSpineToolName::Open,
            r#"{"summary":"must not open"}"#,
            "Spine open accepted.",
        )],
    )];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    let ResponseItem::CustomToolCallOutput { name, output, .. } = &projection.context[0] else {
        panic!("expected deterministic failed output");
    };
    assert_eq!(name, &None);
    assert_eq!(output.success, Some(false));
    assert_eq!(
        output.body,
        FunctionCallOutputBody::Text(
            "Code Mode Spine evidence is invalid and was not applied.".to_string()
        )
    );
    assert!(
        !serde_json::to_string(&projection.context)
            .expect("serialize projected context")
            .contains(CODE_MODE_SPINE_CARRIER_MARKER)
    );
}

#[test]
fn duplicate_code_mode_carrier_outputs_fail_the_entire_group_closed() {
    let valid = || {
        code_mode_carrier_output(
            "exec-1",
            FunctionCallOutputBody::Text("visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"must not open"}"#,
                "Spine open accepted.",
            )],
        )
    };
    let unmarked = || custom_output("exec-1", Some(true), "ordinary output");
    let cases = [
        (
            "malformed then valid",
            vec![malformed_code_mode_carrier_output("exec-1"), valid()],
            2,
        ),
        (
            "valid then malformed",
            vec![valid(), malformed_code_mode_carrier_output("exec-1")],
            2,
        ),
        (
            "malformed then malformed",
            vec![
                malformed_code_mode_carrier_output("exec-1"),
                malformed_code_mode_carrier_output("exec-1"),
            ],
            2,
        ),
        ("valid then valid", vec![valid(), valid()], 2),
        ("marked then unmarked", vec![valid(), unmarked()], 1),
        ("unmarked then marked", vec![unmarked(), valid()], 1),
    ];

    for (case, outputs, expected_failed_marked_outputs) in cases {
        let mut rollout = vec![custom_call("exec-1", "exec", "tools.spine.open({})")];
        rollout.extend(outputs);

        let projection = derive_from_rollout(&rollout);
        assert_eq!(
            projection.spine.cursor.to_string(),
            "1",
            "{case} changed the Spine cursor"
        );
        assert_eq!(
            projection
                .context
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        ResponseItem::CustomToolCallOutput { output, .. }
                            if output.body.to_text().as_deref()
                                == Some("Code Mode Spine evidence is invalid and was not applied.")
                    )
                })
                .count(),
            expected_failed_marked_outputs,
            "{case} did not fail every marked output"
        );
        assert!(
            !serde_json::to_string(&projection.context)
                .expect("serialize projected context")
                .contains(CODE_MODE_SPINE_CARRIER_MARKER),
            "{case} leaked the carrier marker"
        );
    }
}

#[test]
fn duplicate_code_mode_exec_requests_make_carrier_pairing_ambiguous() {
    let rollout = vec![
        custom_call("exec-1", "exec", "tools.spine.open({})"),
        custom_call("exec-1", "exec", "tools.spine.open({})"),
        code_mode_carrier_output(
            "exec-1",
            FunctionCallOutputBody::Text("visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"must not open"}"#,
                "Spine open accepted.",
            )],
        ),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(
        projection.spine.nodes.len(),
        1,
        "ambiguous outer exec pairing produced a Spine node"
    );
    assert_eq!(
        output_text(&projection.context[2]),
        "Code Mode Spine evidence is invalid and was not applied."
    );
    assert!(
        !serde_json::to_string(&projection.context)
            .expect("serialize projected context")
            .contains(CODE_MODE_SPINE_CARRIER_MARKER)
    );
}

#[test]
fn ordinary_tool_call_still_accepts_multiple_outputs() {
    let rollout = vec![
        call("tool-1", "ordinary_tool", "{}"),
        output("tool-1", Some(true), "first"),
        output("tool-1", Some(true), "second"),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(output_text(&projection.context[1]), "first");
    assert_eq!(output_text(&projection.context[2]), "second");
}

#[test]
fn code_mode_carrier_close_returns_ownership_to_the_parent() {
    let rollout = vec![
        custom_call("exec-open", "exec", "tools.spine.open({})"),
        code_mode_carrier_output(
            "exec-open",
            FunctionCallOutputBody::Text("open visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"nested child"}"#,
                "Spine open accepted.",
            )],
        ),
        message("user", "child detail"),
        custom_call("exec-close", "exec", "tools.spine.close({})"),
        code_mode_carrier_output(
            "exec-close",
            FunctionCallOutputBody::Text("close visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Close,
                r#"{"memory":"nested child complete"}"#,
                "Spine close accepted.",
            )],
        ),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert!(projection.spine.nodes.iter().any(|node| {
        node.id.to_string() == "1.1" && node.status == codex_spine_core::NodeStatus::Closed
    }));
    assert!(projection.context.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { content, .. }
                if content.iter().any(|part| matches!(
                    part,
                    ContentItem::InputText { text }
                        if text.contains("nested child complete")
                ))
        )
    }));
    let close_output = projection
        .context
        .iter()
        .find(|item| {
            matches!(
                item,
                ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == "exec-close"
            )
        })
        .expect("close outer exec belongs to the parent");
    assert_eq!(output_text(close_output), "close visible");
}

#[test]
fn code_mode_carrier_next_opens_a_sibling_and_replays_exactly() {
    let rollout = vec![
        message("user", "request"),
        custom_call("exec-open", "exec", "tools.spine.open({})"),
        code_mode_carrier_output(
            "exec-open",
            FunctionCallOutputBody::Text("open visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"first"}"#,
                "Spine open accepted.",
            )],
        ),
        message("user", "first detail"),
        custom_call("exec-next", "exec", "tools.spine.next({})"),
        code_mode_carrier_output(
            "exec-next",
            FunctionCallOutputBody::Text("next visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Next,
                r#"{"summary":"second","memory":"first complete"}"#,
                "Spine next accepted.",
            )],
        ),
    ];

    let live = derive_from_rollout(&rollout);
    assert_eq!(live.spine.cursor.to_string(), "1.2");
    assert!(live.spine.nodes.iter().any(|node| {
        node.id.to_string() == "1.1" && node.status == codex_spine_core::NodeStatus::Closed
    }));
    assert!(live.spine.nodes.iter().any(|node| {
        node.id.to_string() == "1.2" && node.status == codex_spine_core::NodeStatus::Live
    }));
    assert!(live.context.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { content, .. }
                if content.iter().any(|part| matches!(
                    part,
                    ContentItem::InputText { text }
                        if text.contains("first complete")
                ))
        )
    }));

    let persisted = serde_json::to_string(&rollout).expect("serialize carrier rollout");
    let restored: Vec<RolloutItem> =
        serde_json::from_str(&persisted).expect("deserialize carrier rollout");
    assert_eq!(derive_from_rollout(&restored), live);
}

#[test]
fn accepted_nested_control_survives_outer_exec_failure() {
    let rollout = vec![
        custom_call("exec-failed", "exec", "throw new Error('after open')"),
        code_mode_carrier_output_with_success(
            "exec-failed",
            FunctionCallOutputBody::Text("Script failed after open".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"durable child"}"#,
                "Spine open accepted.",
            )],
            Some(false),
        ),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1.1");
    let ResponseItem::CustomToolCallOutput { output, name, .. } = &projection.context[2] else {
        panic!("expected projected failed exec output");
    };
    assert_eq!(name, &None);
    assert_eq!(output.success, Some(false));
    assert_eq!(
        output_text(&projection.context[2]),
        "Script failed after open"
    );
}

#[test]
fn carrier_rollback_and_fork_rederive_from_the_selected_native_prefix() {
    let prefix = vec![
        message("user", "request"),
        custom_call("exec-open", "exec", "tools.spine.open({})"),
        code_mode_carrier_output(
            "exec-open",
            FunctionCallOutputBody::Text("open visible".to_string()),
            vec![nested_spine_call(
                0,
                NestedSpineToolName::Open,
                r#"{"summary":"retained child"}"#,
                "Spine open accepted.",
            )],
        ),
    ];
    let expected = derive_from_rollout(&prefix);
    assert_eq!(expected.spine.cursor.to_string(), "1.1");
    assert_eq!(
        derive_from_rollout(&prefix[..1]).spine.cursor.to_string(),
        "1"
    );

    let mut rolled_back = prefix.clone();
    rolled_back.extend([
        message("user", "discarded turn"),
        message("assistant", "discarded response"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ]);
    assert_eq!(derive_from_rollout(&rolled_back), expected);
}

fn spawn_arguments() -> String {
    serde_json::json!({
        "tasks": [
            {"summary": "first", "prompt": "inspect first"},
            {"summary": "second", "prompt": "inspect second"}
        ]
    })
    .to_string()
}

fn spawn_receipt() -> String {
    serde_json::json!({
        "schema": spine_core::SPINE_SPAWN_RESULT_SCHEMA,
        "results": [
            {
                "ordinal": 0,
                "outcome": "completed",
                "memory_body": "first memory",
                "execution_ref": "thread-first"
            },
            {
                "ordinal": 1,
                "outcome": "errored",
                "memory_body": "second error memory",
                "diagnostic": "child failed",
                "execution_ref": "thread-second"
            }
        ]
    })
    .to_string()
}

fn response_items(rollout: &[RolloutItem]) -> Vec<ResponseItem> {
    rollout
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect()
}

fn trim_candidate_text(fragment: &str) -> String {
    assert!(!fragment.is_empty());
    let minimum_bytes = spine_core::TOOL_RESPONSE_TRIM_THRESHOLD_BYTES + 1;
    fragment.repeat(minimum_bytes.div_ceil(fragment.len()))
}

fn text(item: &ResponseItem) -> &str {
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected input text");
    };
    text
}

fn output_text(item: &ResponseItem) -> String {
    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.body.to_text().unwrap(),
        _ => panic!("expected tool output"),
    }
}

fn token_count(input_tokens: i64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
        info: Some(TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens,
                total_tokens: input_tokens,
                ..TokenUsage::default()
            },
            last_token_usage: TokenUsage {
                input_tokens,
                total_tokens: input_tokens,
                ..TokenUsage::default()
            },
            model_context_window: Some(200_000),
        }),
        rate_limits: None,
    }))
}

#[test]
fn spine_transition_status_matches_spine_dev_fields_and_context_accounting() {
    let rollout = vec![
        message("user", "request"),
        call(
            "open",
            "spine.open",
            r#"{"summary":"child \"scope\" <leaf> & focus"}"#,
        ),
        output("open", Some(true), "Spine open accepted."),
        token_count(10_000),
        message("user", "detail"),
        token_count(42_000),
    ];
    let transition_status = status::transition_item(&rollout, Some(55_000), Some(100_000), true);

    assert_eq!(
        text(&transition_status),
        r#"<spine_tran_status cursor="1.1" summary="child &quot;scope&quot; &lt;leaf&gt; &amp; focus" parent="1" parent_summary="root" cursor_context="45.0K" context_left="100K" />"#
    );
}

#[test]
fn spine_transition_status_does_not_reuse_stale_rollout_usage() {
    let rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", Some(true), "Spine open accepted."),
        token_count(10_000),
        message("user", "detail"),
        token_count(42_000),
    ];
    let transition_status = status::transition_item(&rollout, None, None, true);

    assert_eq!(
        text(&transition_status),
        r#"<spine_tran_status cursor="1.1" summary="task" parent="1" parent_summary="root" cursor_context="unavailable" context_left="unavailable" />"#
    );
}

#[test]
fn spine_transition_status_allows_missing_baseline_for_new_node() {
    let rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", Some(true), "Spine open accepted."),
    ];
    let transition_status = status::transition_item(&rollout, Some(55_000), Some(100_000), true);

    assert_eq!(
        text(&transition_status),
        r#"<spine_tran_status cursor="1.1" summary="task" parent="1" parent_summary="root" cursor_context="unavailable" context_left="100K" />"#
    );
}

#[test]
fn nested_open_keeps_the_materialized_parent_marker_prefix_stable() {
    let mut rollout = vec![
        message("user", "request"),
        call("open-parent", "spine.open", r#"{"summary":"parent"}"#),
        output("open-parent", Some(true), "Spine open accepted."),
    ];
    let parent = derive_from_rollout(&rollout);
    assert_eq!(
        text(&parent.context[1]),
        r#"<spine_node id="1.1" summary="parent" status="opened" />"#
    );

    rollout.extend([
        call("open-child", "spine.open", r#"{"summary":"child"}"#),
        output("open-child", Some(true), "Spine open accepted."),
    ]);
    let nested = derive_from_rollout(&rollout);
    assert!(
        nested.context.starts_with(&parent.context),
        "opening a child must only append to the materialized parent prefix"
    );
}

#[test]
fn spine_transition_statuses_follow_normal_rollout_projection() {
    let mut rollout = vec![
        message("user", "request"),
        call("open-parent", "spine.open", r#"{"summary":"parent"}"#),
        spine_success_output("open-parent", tool_response::SpineToolResponse::Open),
    ];
    let parent_status = status::transition_item(&rollout, None, None, true);
    rollout.push(RolloutItem::ResponseItem(parent_status.clone()));
    rollout.extend([
        call("open-child", "spine.open", r#"{"summary":"child"}"#),
        spine_success_output("open-child", tool_response::SpineToolResponse::Open),
    ]);
    let child_status = status::transition_item(&rollout, None, None, true);
    rollout.push(RolloutItem::ResponseItem(child_status.clone()));

    let projection = derive_from_rollout_with_features(&rollout, true, false, true);

    assert_eq!(projection.spine.cursor.to_string(), "1.1.1");
    assert!(projection.context.contains(&parent_status));
    assert!(projection.context.contains(&child_status));
    assert_eq!(projection.context.last(), Some(&child_status));
}

#[test]
fn compact_replacement_preserves_spine_transition_status() {
    let mut rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        spine_success_output("open", tool_response::SpineToolResponse::Open),
    ];
    let status_item = status::transition_item(&rollout, None, None, true);
    rollout.push(RolloutItem::ResponseItem(status_item.clone()));
    rollout.push(RolloutItem::Compacted(CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(vec![
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "native compact baseline".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            status_item.clone(),
        ]),
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    }));

    let projection = derive_from_rollout(&rollout);

    assert_eq!(projection.spine.cursor.to_string(), "2");
    assert_eq!(projection.context.len(), 2);
    assert!(projection.context.contains(&status_item));
    let ResponseItem::Message { content, .. } = &projection.context[0] else {
        panic!("expected compact replacement message");
    };
    assert_eq!(
        content,
        &[ContentItem::OutputText {
            text: "native compact baseline".to_string()
        }]
    );
}

#[test]
fn node_context_pressure_is_a_pure_rollout_prefix_projection() {
    let mut rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", Some(true), "Spine open accepted."),
        token_count(10_000),
        message("user", "detail"),
        token_count(42_000),
    ];

    let full_projection = derive_from_rollout(&rollout).spine;
    let full = pressure::project(&rollout, &full_projection);
    let full_active = full
        .iter()
        .find(|(node_id, _)| node_id.to_string() == "1.1")
        .map(|(_, pressure)| pressure)
        .expect("active node pressure");
    assert_eq!(
        full_active,
        &pressure::NodeContextPressure {
            open_input_tokens: Some(10_000),
            current_input_tokens: Some(42_000),
            context_tokens: Some(32_000),
            problem: None,
        }
    );

    let resumed_projection = derive_from_rollout(&rollout).spine;
    assert_eq!(pressure::project(&rollout, &resumed_projection), full);

    let fork = &rollout[..4];
    let fork_projection = derive_from_rollout(fork).spine;
    let fork_pressure = pressure::project(fork, &fork_projection);
    assert_eq!(
        fork_pressure
            .iter()
            .find(|(node_id, _)| node_id.to_string() == "1.1")
            .and_then(|(_, pressure)| pressure.context_tokens),
        Some(0)
    );

    rollout.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent { num_turns: 1 },
    )));
    let rollback_projection = derive_from_rollout(&rollout).spine;
    let rollback_pressure = pressure::project(&rollout, &rollback_projection);
    assert_eq!(
        rollback_pressure
            .iter()
            .find(|(node_id, _)| node_id.to_string() == "1.1")
            .and_then(|(_, pressure)| pressure.context_tokens),
        Some(0)
    );
}

fn long_tool_rollout() -> Vec<RolloutItem> {
    vec![
        call("shell", "shell", r#"{"cmd":"cat"}"#),
        output("shell", Some(true), &trim_candidate_text("0123456789\n")),
    ]
}

#[test]
fn adapter_projects_open_and_close_from_native_function_carriers() {
    let rollout = vec![
        message("user", "request"),
        namespaced_call("open", "spine", "open", r#"{"summary":"task"}"#),
        output("open", Some(true), "Spine open accepted."),
        message("user", "detail"),
        call("close", "spine.close", r#"{"memory":"done"}"#),
        output("close", Some(true), "Spine close accepted."),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.context.len(), 5);
    assert_eq!(text(&projection.context[0]), "[U1]\nrequest");
    assert_eq!(text(&projection.context[1]), "[U2]\ndetail");
    assert_eq!(
        text(&projection.context[2]),
        "<spine_memory node_id=\"1.1\">\ndone\n</spine_memory>"
    );
    assert!(matches!(
        projection.context[3],
        ResponseItem::FunctionCall { .. }
    ));
    assert!(matches!(
        projection.context[4],
        ResponseItem::FunctionCallOutput { .. }
    ));
}

#[test]
fn adapter_flattens_nested_memory_slots_in_source_order() {
    let rollout = vec![
        call("open-parent", "spine.open", r#"{"summary":"parent"}"#),
        output("open-parent", Some(true), "ok"),
        message("user", "before"),
        call("open-child", "spine.open", r#"{"summary":"child"}"#),
        output("open-child", Some(true), "ok"),
        message("user", "inside"),
        call("close-child", "spine.close", r#"{"memory":"child done"}"#),
        output("close-child", Some(true), "ok"),
        message("user", "after"),
        call("close-parent", "spine.close", r#"{"memory":"parent done"}"#),
        output("close-parent", Some(true), "ok"),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.context.len(), 7);
    assert_eq!(text(&projection.context[0]), "[U1]\nbefore");
    assert_eq!(text(&projection.context[1]), "[U2]\ninside");
    assert_eq!(
        text(&projection.context[2]),
        "<spine_memory node_id=\"1.1.1\">\nchild done\n</spine_memory>"
    );
    assert_eq!(text(&projection.context[3]), "[U3]\nafter");
    assert_eq!(
        text(&projection.context[4]),
        "<spine_memory node_id=\"1.1\">\nparent done\n</spine_memory>"
    );
    assert!(matches!(
        projection.context[5],
        ResponseItem::FunctionCall { .. }
    ));
    assert!(matches!(
        projection.context[6],
        ResponseItem::FunctionCallOutput { .. }
    ));
}

#[test]
fn adapter_replays_persisted_spine_success_carriers_without_success_metadata() {
    let rollout = vec![
        message("user", "request"),
        call("open-1", "spine.open", r#"{"summary":"first"}"#),
        spine_success_output("open-1", tool_response::SpineToolResponse::Open),
        message("user", "detail"),
        call("open-2", "spine.open", r#"{"summary":"second"}"#),
        spine_success_output("open-2", tool_response::SpineToolResponse::Open),
    ];

    let persisted = serde_json::to_string(&rollout).expect("serialize rollout");
    let restored: Vec<RolloutItem> = serde_json::from_str(&persisted).expect("deserialize rollout");
    for index in [2, 5] {
        let RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { output, .. }) =
            &restored[index]
        else {
            panic!("expected restored function output at index {index}");
        };
        assert_eq!(output.success, None);
    }

    let projection = derive_from_rollout(&restored);
    assert_eq!(projection.spine.cursor.to_string(), "1.1.1");
}

#[test]
fn adapter_does_not_accept_near_miss_spine_success_text() {
    let rollout = vec![
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", None, "Spine open accepted"),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
}

#[test]
fn spawn_bridge_projects_one_ordered_atomic_batch_and_hides_success_carrier() {
    let mut rollout = vec![message("user", "request")];
    rollout.extend([
        namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
        output("spawn", Some(true), &spawn_receipt()),
        message("user", "after"),
    ]);

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.spine.nodes.len(), 3);
    assert_eq!(projection.spine.nodes[1].summary.as_deref(), Some("first"));
    assert_eq!(projection.spine.nodes[2].summary.as_deref(), Some("second"));
    assert!(
        projection
            .spine
            .nodes
            .iter()
            .skip(1)
            .all(|node| node.status == NodeStatus::Closed)
    );
    assert_eq!(text(&projection.context[0]), "[U1]\nrequest");
    assert!(matches!(
        projection.context[1],
        ResponseItem::FunctionCall { .. }
    ));
    assert_eq!(
        output_text(&projection.context[2]),
        r#"{"status":"success"}"#
    );
    assert_eq!(
        text(&projection.context[4]),
        "<spine_memory node_id=\"1.1\">\nfirst memory\n</spine_memory>"
    );
    assert!(text(&projection.context[5]).contains("\"summary\": \"second\""));
    assert!(text(&projection.context[5]).contains("\"diagnostic\": \"child failed\""));
    assert_eq!(
        text(&projection.context[6]),
        "<spine_memory node_id=\"1.2\">\nsecond error memory\n</spine_memory>"
    );
    assert_eq!(text(&projection.context[7]), "[U2]\nafter");
    assert_eq!(projection.context.len(), 8);

    let effective = effective_rollout(&rollout);
    let events = lex_rollout(&effective, true);
    let registration = SpineRegistration::builder()
        .enable(Feature::Jit)
        .build()
        .unwrap();
    let mut live = SpineCompiler::new(SpineConfig::v1(), registration).unwrap();
    for event in events {
        live.eat(event).unwrap();
    }
    assert_eq!(live.projection(), &projection.spine);
    assert_eq!(
        materialize_context(
            &live.projection().visible_context,
            &rollout,
            None,
            None,
            true,
        )
        .expect("test rollout sources resolve"),
        projection.context
    );
    let before_receipt = derive_from_rollout(&rollout[..2]);
    assert_eq!(before_receipt.spine.nodes.len(), 1);
    assert_eq!(text(&before_receipt.context[0]), "[U1]\nrequest");
    assert!(matches!(
        before_receipt.context[1],
        ResponseItem::FunctionCall { .. }
    ));
    assert_eq!(response_items(&rollout).len(), 4);
}

#[test]
fn spawn_bridge_replay_accepts_persisted_carrier_without_success_metadata() {
    let rollout = vec![
        namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
        output("spawn", Some(true), &spawn_receipt()),
    ];
    let live = derive_from_rollout(&rollout);
    let persisted = serde_json::to_string(&rollout).expect("serialize spawn rollout");
    let mut restored: Vec<RolloutItem> = serde_json::from_str(&persisted).expect("restore rollout");
    let RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { output, .. }) =
        &mut restored[1]
    else {
        panic!("expected persisted spawn output");
    };
    assert_eq!(output.success, None);
    let replay = derive_from_rollout(&restored);
    assert_eq!(live, replay);
    assert_eq!(replay.spine.nodes.len(), 3);
    assert_eq!(replay.context.len(), 6);
}

#[test]
fn spawn_lifecycle_rederives_from_selected_native_rollout_prefix() {
    let completed_prefix = vec![
        message("user", "before spawn"),
        namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
        output("spawn", Some(true), &spawn_receipt()),
    ];
    let completed = derive_from_rollout(&completed_prefix);
    assert_eq!(completed.spine.nodes.len(), 3);

    let persisted = serde_json::to_string(&completed_prefix).expect("serialize completed spawn");
    let restored: Vec<RolloutItem> =
        serde_json::from_str(&persisted).expect("restore completed spawn");
    assert_eq!(derive_from_rollout(&restored), completed);

    let before_receipt = derive_from_rollout(&completed_prefix[..2]);
    assert_eq!(before_receipt.spine.nodes.len(), 1);
    assert_eq!(text(&before_receipt.context[0]), "[U1]\nbefore spawn");
    assert!(matches!(
        before_receipt.context[1],
        ResponseItem::FunctionCall { .. }
    ));

    let retained_fork = derive_from_rollout(&completed_prefix);
    assert_eq!(retained_fork, completed);
    let pre_call_fork = derive_from_rollout(&completed_prefix[..1]);
    assert_eq!(pre_call_fork.spine.nodes.len(), 1);

    let mut rollback_after = completed_prefix.clone();
    rollback_after.push(message("user", "later turn"));
    rollback_after.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent { num_turns: 1 },
    )));
    assert_eq!(derive_from_rollout(&rollback_after), completed);

    let mut rollback_before = completed_prefix;
    rollback_before.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent { num_turns: 1 },
    )));
    let rolled_back_before = derive_from_rollout(&rollback_before);
    assert_eq!(rolled_back_before.spine.nodes.len(), 1);
    assert!(rolled_back_before.context.is_empty());
}

#[test]
fn spawn_bridge_keeps_malformed_failed_and_incomplete_groups_ordinary() {
    let cases = [
        vec![
            namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
            output("spawn", Some(true), "not a receipt"),
        ],
        vec![
            namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
            output(
                "spawn",
                Some(true),
                &serde_json::json!({"schema":"wrong","results":[]}).to_string(),
            ),
        ],
        vec![
            namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
            output("spawn", Some(false), "capacity rejected"),
        ],
        vec![namespaced_call(
            "spawn",
            "spine",
            "spawn",
            &spawn_arguments(),
        )],
    ];

    for (case, rollout) in cases.into_iter().enumerate() {
        let projection = derive_from_rollout(&rollout);
        assert_eq!(projection.spine.nodes.len(), 1);
        assert_eq!(projection.spine.cursor.to_string(), "1");
        if case < 3 {
            assert_eq!(
                output_text(&projection.context[1]),
                r#"{"status":"failure"}"#
            );
        } else {
            assert_eq!(projection.context, response_items(&rollout));
        }
    }
}

#[test]
fn spawn_bridge_feature_off_preserves_native_context_and_tree() {
    let rollout = vec![
        namespaced_call("spawn", "spine", "spawn", &spawn_arguments()),
        output("spawn", Some(true), &spawn_receipt()),
    ];
    let projection = derive_from_rollout_with_features(&rollout, true, false, false);
    assert_eq!(projection.spine.nodes.len(), 1);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.context, response_items(&rollout));
}

#[test]
fn closed_memory_projection_entries_follow_rollout_projection() {
    let rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", Some(true), "ok"),
        message("user", "detail"),
        call("close", "spine.close", r#"{"memory":"done"}"#),
        output("close", Some(true), "ok"),
    ];

    let projection = derive_from_rollout(&rollout).spine;
    let entries = closed_memory_projection_entries(&projection);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].node_id, "1.1");
    assert_eq!(entries[0].summary, "task");
    assert_eq!(
        entries[0].body,
        "# Spine Memory 1.1\n\n## Node Memory\ndone"
    );
}

#[test]
fn user_message_projection_entries_follow_effective_rollout() {
    let rollout = vec![
        message(
            "user",
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
        ),
        message("user", "first"),
        message("assistant", "answer"),
        message("user", "rolled back"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        message("user", "replacement"),
    ];

    assert_eq!(
        user_message_projection_entries(&rollout),
        vec![
            memory_projection::SpinetreeUserMessageProjectionEntry {
                anchor: 1,
                body: "first".to_string(),
            },
            memory_projection::SpinetreeUserMessageProjectionEntry {
                anchor: 2,
                body: "replacement".to_string(),
            },
        ]
    );
}

#[test]
fn adapter_projects_next_group_into_the_new_sibling() {
    let rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"first"}"#),
        output("open", Some(true), "Spine open accepted."),
        message("user", "detail"),
        call(
            "next",
            "spine.next",
            r#"{"summary":"second","memory":"first done"}"#,
        ),
        output("next", Some(true), "Spine next accepted."),
        message("user", "continue"),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1.2");
    assert_eq!(text(&projection.context[0]), "[U1]\nrequest");
    assert_eq!(text(&projection.context[1]), "[U2]\ndetail");
    assert_eq!(
        text(&projection.context[2]),
        "<spine_memory node_id=\"1.1\">\nfirst done\n</spine_memory>"
    );
    assert!(text(&projection.context[3]).contains("id=\"1.2\""));
    assert!(matches!(
        projection.context[4],
        ResponseItem::FunctionCall { .. }
    ));
    assert!(matches!(
        projection.context[5],
        ResponseItem::FunctionCallOutput { .. }
    ));
    assert_eq!(text(&projection.context[6]), "[U3]\ncontinue");
}

#[test]
fn adapter_keeps_leading_assistant_and_multi_call_group_together() {
    let rollout = vec![
        message("assistant", "inspect first"),
        call("shell", "shell", r#"{"cmd":"pwd"}"#),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("shell", Some(true), "workdir"),
        output("open", Some(true), "Spine open accepted."),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1.1");
    assert!(text(&projection.context[0]).starts_with("<spine_node"));
    assert_eq!(text(&projection.context[1]), "inspect first");
    assert_eq!(projection.context.len(), 6);
}

fn closed_tool_group_boundaries(rollout: &[RolloutItem]) -> Vec<(RawBoundary, RawBoundary)> {
    let effective = effective_rollout(rollout);
    let mut boundaries = Vec::new();
    let mut index = 0;
    while index < effective.len() {
        let Some((group, consumed)) = completed_tool_group(&effective, index, true) else {
            index += 1;
            continue;
        };
        if group.calls.iter().all(|call| call.outcome.is_some()) {
            boundaries.push((group.start, group.end));
            index += consumed;
        } else {
            // A trailing request without every matching output remains ordinary pending
            // material. It must not be emitted to the reducer and retracted later.
            index += 1;
        }
    }
    boundaries
}

#[test]
fn append_prefixes_reconstruct_closed_group_frontier_without_retraction() {
    let rollout = vec![
        message("assistant", "inspect first"),
        call("shell", "shell", r#"{"cmd":"pwd"}"#),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("shell", Some(true), "workdir"),
        output("open", Some(true), "Spine open accepted."),
        message("user", "continue"),
    ];

    let mut emitted = Vec::new();
    for prefix_len in 1..=rollout.len() {
        let available = closed_tool_group_boundaries(&rollout[..prefix_len]);
        assert!(available.starts_with(&emitted));
        emitted = available;
    }

    assert_eq!(
        emitted,
        vec![(RawBoundary(0), RawBoundary(4))],
        "the complete response group must be emitted once after its last output"
    );
}

#[test]
fn separate_outputs_and_persisted_roundtrip_preserve_group_frontier() {
    let rollout = vec![
        call("first", "shell", r#"{"cmd":"one"}"#),
        call("second", "shell", r#"{"cmd":"two"}"#),
        output("first", Some(true), "one"),
        output("second", Some(true), "two"),
    ];
    let persisted = serde_json::to_string(&rollout).expect("serialize rollout");
    let restored: Vec<RolloutItem> = serde_json::from_str(&persisted).expect("restore rollout");

    assert!(closed_tool_group_boundaries(&rollout[..3]).is_empty());
    assert_eq!(
        closed_tool_group_boundaries(&rollout),
        vec![(RawBoundary(0), RawBoundary(3))]
    );
    assert_eq!(
        closed_tool_group_boundaries(&restored),
        closed_tool_group_boundaries(&rollout)
    );
    for item in &restored[2..] {
        let RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { output, .. }) = item
        else {
            panic!("expected restored function output");
        };
        assert_eq!(output.success, None);
    }
}

#[test]
fn an_end_of_prefix_with_missing_output_stays_ordinary_and_never_guesses_control() {
    let rollout = vec![
        call("open", "spine.open", r#"{"summary":"task"}"#),
        message("user", "a later native item"),
    ];

    assert!(closed_tool_group_boundaries(&rollout).is_empty());
    assert_eq!(derive_from_rollout(&rollout).spine.cursor.to_string(), "1");
}

#[test]
fn failed_and_incomplete_control_outputs_do_not_transition() {
    let failed = vec![
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", Some(false), "failed"),
    ];
    let incomplete = vec![call("open", "spine.open", r#"{"summary":"task"}"#)];

    assert_eq!(derive_from_rollout(&failed).spine.cursor.to_string(), "1");
    assert_eq!(
        derive_from_rollout(&incomplete).spine.cursor.to_string(),
        "1"
    );
}

#[test]
fn successful_close_carrier_at_root_does_not_transition() {
    let rollout = vec![
        call("close", "spine.close", r#"{"memory":"invalid"}"#),
        output("close", Some(true), "Spine close accepted."),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.context.len(), rollout.len());
}

#[test]
fn trim_tag_bytes_persist_after_eligibility_expires() {
    let mut rollout = long_tool_rollout();
    let tagged = derive_from_rollout_with_features(&rollout, true, true, true);
    let tagged_output = output_text(&tagged.context[1]).to_string();
    assert!(tagged_output.starts_with("[TRIM_ID: trim_1]"));

    rollout.extend([
        call("trim", "spine.trim", r#"{"TRIM_ID":"trim_1","op":"snip"}"#),
        output("trim", Some(true), "Spine trim accepted."),
    ]);
    let snipped = derive_from_rollout_with_features(&rollout, true, true, true);
    assert_eq!(
        output_text(&snipped.context[1]),
        TOOL_RESULT_CLEARED_MESSAGE
    );

    let mut expired = long_tool_rollout();
    expired.extend([
        call("next-tool", "shell", r#"{"cmd":"next"}"#),
        output("next-tool", Some(true), "short"),
    ]);
    let expired = derive_from_rollout_with_features(&expired, true, true, true);
    assert_eq!(output_text(&expired.context[1]), tagged_output);
    assert_eq!(&expired.context[..tagged.context.len()], tagged.context);
}

#[test]
fn trim_validation_rejects_a_visible_but_expired_id() {
    let mut rollout = long_tool_rollout();
    rollout.extend([
        call("next-tool", "shell", r#"{"cmd":"next"}"#),
        output("next-tool", Some(true), "short"),
        call("trim", "spine.trim", r#"{"TRIM_ID":"trim_1","op":"snip"}"#),
    ]);
    let request =
        codex_spine_core::TrimRequest::parse(r#"{"TRIM_ID":"trim_1","op":"snip"}"#).unwrap();

    let error = validate_trim_request(&rollout, "trim", &request).unwrap_err();

    assert!(error.contains("previous completed toolcall does not contain TRIM_ID trim_1"));
    let projection = derive_from_rollout_with_features(&rollout, true, true, true);
    assert!(output_text(&projection.context[1]).starts_with("[TRIM_ID: trim_1]"));
}

#[test]
fn trim_custom_output_uses_the_same_tag_validate_and_edit_path() {
    let original = trim_candidate_text("0123456789\n");
    let base = vec![
        custom_call("custom", "exec", "return await tools.shell_command({});"),
        custom_output("custom", Some(true), &original),
    ];

    let tagged = derive_from_rollout_with_features(&base, false, true, true);
    assert!(matches!(
        &tagged.context[0],
        ResponseItem::CustomToolCall { .. }
    ));
    assert!(matches!(
        &tagged.context[1],
        ResponseItem::CustomToolCallOutput { .. }
    ));
    assert!(output_text(&tagged.context[1]).starts_with("[TRIM_ID: trim_1]"));

    for (arguments, expected) in [
        (
            r#"{"TRIM_ID":"trim_1","op":"snip"}"#,
            TOOL_RESULT_CLEARED_MESSAGE,
        ),
        (r#"{"TRIM_ID":"trim_1","op":"slice","head":4}"#, "0123"),
    ] {
        let mut rollout = base.clone();
        rollout.push(call("trim", "spine.trim", arguments));
        let request = codex_spine_core::TrimRequest::parse(arguments).unwrap();
        assert!(validate_trim_request(&rollout, "trim", &request).is_ok());
        rollout.push(output("trim", Some(true), "Spine trim accepted."));

        let projected = derive_from_rollout_with_features(&rollout, false, true, true);
        assert!(matches!(
            &projected.context[1],
            ResponseItem::CustomToolCallOutput { .. }
        ));
        assert_eq!(output_text(&projected.context[1]), expected);
        let raw_items = response_items(&rollout);
        assert_eq!(output_text(&raw_items[1]), original);
    }
}

#[test]
fn trim_indexes_function_and_custom_responses_in_one_completed_group() {
    let mut rollout = vec![
        call("function", "shell", r#"{"cmd":"first"}"#),
        custom_call("custom", "exec", "return 'second';"),
        output(
            "function",
            Some(true),
            &trim_candidate_text("function output\n"),
        ),
        custom_output(
            "custom",
            Some(true),
            &trim_candidate_text("custom output\n"),
        ),
    ];

    let tagged = derive_from_rollout_with_features(&rollout, true, true, true);
    assert!(output_text(&tagged.context[2]).starts_with("[TRIM_ID: trim_2]"));
    let custom_tagged = output_text(&tagged.context[3]);
    assert!(
        custom_tagged.starts_with("[TRIM_ID: trim_3]"),
        "unexpected custom projection prefix: {:?}",
        custom_tagged.lines().next()
    );

    let arguments = r#"{"TRIM_ID":"trim_3","op":"snip"}"#;
    rollout.extend([
        call("trim", "spine.trim", arguments),
        output("trim", Some(true), "Spine trim accepted."),
    ]);
    let projected = derive_from_rollout_with_features(&rollout, true, true, true);
    assert!(output_text(&projected.context[2]).starts_with("[TRIM_ID: trim_2]"));
    assert_eq!(
        output_text(&projected.context[3]),
        TOOL_RESULT_CLEARED_MESSAGE
    );
}

#[test]
fn trim_slice_shapes_are_deterministic_and_independent_of_jit() {
    let base = long_tool_rollout();
    let cases = [
        (r#"{"TRIM_ID":"trim_1","op":"slice","head":4}"#, "0123"),
        (r#"{"TRIM_ID":"trim_1","op":"slice","tail":4}"#, "789\n"),
    ];
    for (arguments, expected_fragment) in cases {
        let mut rollout = base.clone();
        rollout.extend([
            call("trim", "spine.trim", arguments),
            output("trim", Some(true), "Spine trim accepted."),
        ]);
        for jit in [false, true] {
            let projection = derive_from_rollout_with_features(&rollout, jit, true, true);
            let output = &projection.context[1];
            assert_eq!(output_text(output), expected_fragment);
        }
    }

    let mut anchored = base;
    anchored.extend([
        call(
            "trim",
            "spine.trim",
            r#"{"TRIM_ID":"trim_1","op":"slice","anchor":"345","preceding":1,"following":1}"#,
        ),
        output("trim", Some(true), "Spine trim accepted."),
    ]);
    let projection = derive_from_rollout_with_features(&anchored, false, true, true);
    assert_eq!(
        output_text(&projection.context[1]),
        "0123456789\n0123456789\n"
    );
}

#[test]
fn trim_feature_matrix_preserves_native_shape_when_jit_is_off() {
    let rollout = long_tool_rollout();
    for (jit, trim, expected_tag) in [
        (false, false, false),
        (true, false, false),
        (false, true, true),
        (true, true, true),
    ] {
        let projection = derive_from_rollout_with_features(&rollout, jit, trim, true);
        let output = &projection.context[1];
        assert_eq!(output_text(output).contains("TRIM_ID"), expected_tag);
    }
}

#[test]
fn trim_feature_off_is_native_context_identity() {
    let rollout = long_tool_rollout();
    let expected = rollout
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        derive_from_rollout_with_features(&rollout, false, false, true).context,
        expected
    );
}

#[test]
fn failed_and_incomplete_trim_requests_do_not_rewrite_output() {
    for suffix in [
        vec![
            call("trim", "spine.trim", r#"{"TRIM_ID":"trim_1","op":"snip"}"#),
            output("trim", Some(false), "trim rejected"),
        ],
        vec![call(
            "trim",
            "spine.trim",
            r#"{"TRIM_ID":"trim_1","op":"snip"}"#,
        )],
    ] {
        let mut rollout = long_tool_rollout();
        rollout.extend(suffix);
        let projection = derive_from_rollout_with_features(&rollout, false, true, true);
        let body = output_text(&projection.context[1]);
        assert!(body.starts_with("[TRIM_ID: trim_1]"));
        assert_ne!(body, TOOL_RESULT_CLEARED_MESSAGE);
    }
}

#[test]
fn trim_and_ordinary_tool_in_one_group_apply_old_edit_and_tag_new_output() {
    let mut rollout = long_tool_rollout();
    rollout.extend([
        call("trim", "spine.trim", r#"{"TRIM_ID":"trim_1","op":"snip"}"#),
        call("next-shell", "shell", r#"{"cmd":"next"}"#),
        output("trim", Some(true), "Spine trim accepted."),
        output(
            "next-shell",
            Some(true),
            &trim_candidate_text("new evidence\n"),
        ),
    ]);
    let projection = derive_from_rollout_with_features(&rollout, true, true, true);
    assert_eq!(
        output_text(&projection.context[1]),
        TOOL_RESULT_CLEARED_MESSAGE
    );
    assert!(output_text(&projection.context[5]).starts_with("[TRIM_ID: trim_5]"));
}

#[test]
fn trim_output_itself_never_becomes_a_candidate() {
    let rollout = vec![
        call("trim", "spine.trim", r#"{"TRIM_ID":"missing","op":"snip"}"#),
        output("trim", Some(true), &trim_candidate_text("not a candidate")),
    ];
    let projection = derive_from_rollout_with_features(&rollout, false, true, true);
    assert!(!output_text(&projection.context[1]).contains("TRIM_ID"));
}

#[test]
fn compact_replaces_old_trim_baseline_and_replays_new_candidates() {
    let replacement = vec![message("assistant", "native compact baseline")]
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut rollout = long_tool_rollout();
    rollout.push(RolloutItem::Compacted(CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(replacement.clone()),
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    }));
    rollout.extend([
        call("new-shell", "shell", r#"{"cmd":"new"}"#),
        output(
            "new-shell",
            Some(true),
            &trim_candidate_text("new evidence\n"),
        ),
    ]);
    let tagged = derive_from_rollout_with_features(&rollout, false, true, true);
    assert_eq!(tagged.context[0], replacement[0]);
    assert!(output_text(&tagged.context[2]).starts_with("[TRIM_ID: trim_4]"));

    rollout.extend([
        call("trim", "spine.trim", r#"{"TRIM_ID":"trim_4","op":"snip"}"#),
        output("trim", Some(true), "Spine trim accepted."),
    ]);
    let snipped = derive_from_rollout_with_features(&rollout, false, true, true);
    assert_eq!(
        output_text(&snipped.context[2]),
        TOOL_RESULT_CLEARED_MESSAGE
    );
}

#[test]
fn trim_rollback_and_fork_rederive_from_selected_native_prefix() {
    let first = long_tool_rollout();
    let mut rollout = vec![message("user", "first")];
    rollout.extend(first);
    rollout.push(message("user", "second"));
    rollout.extend([
        call("second-shell", "shell", r#"{"cmd":"second"}"#),
        output(
            "second-shell",
            Some(true),
            &trim_candidate_text("second result\n"),
        ),
        call("trim", "spine.trim", r#"{"TRIM_ID":"trim_5","op":"snip"}"#),
        output("trim", Some(true), "Spine trim accepted."),
    ]);

    let fork = derive_from_rollout_with_features(&rollout[..3], false, true, true);
    assert!(output_text(&fork.context[2]).starts_with("[TRIM_ID: trim_2]"));

    rollout.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent { num_turns: 1 },
    )));
    let rolled_back = derive_from_rollout_with_features(&rollout, false, true, true);
    assert_eq!(rolled_back.context, fork.context);
}

#[test]
fn multiple_successful_controls_in_one_group_are_conflicting() {
    let rollout = vec![
        call("open", "spine.open", r#"{"summary":"task"}"#),
        call(
            "next",
            "spine.next",
            r#"{"summary":"sibling","memory":"done"}"#,
        ),
        output("open", Some(true), "Spine open accepted."),
        output("next", Some(true), "Spine next accepted."),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.context.len(), rollout.len());
}

#[test]
fn compact_replacement_history_is_materialized_exactly_once() {
    let replacement = vec![ResponseItem::Message {
        id: Some("replacement".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "native summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let rollout = vec![
        message("user", "old"),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(replacement.clone()),
            window_number: Some(1),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "2");
    assert_eq!(projection.context, replacement);
}

#[test]
fn rollback_rederives_from_surviving_native_prefix() {
    let rollout = vec![
        message("user", "first"),
        call("open", "spine.open", r#"{"summary":"first task"}"#),
        spine_success_output("open", tool_response::SpineToolResponse::Open),
        message("user", "second"),
        call("close", "spine.close", r#"{"memory":"done"}"#),
        spine_success_output("close", tool_response::SpineToolResponse::Close),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let persisted = serde_json::to_string(&rollout).expect("serialize rollback rollout");
    let restored: Vec<RolloutItem> =
        serde_json::from_str(&persisted).expect("deserialize rollback rollout");
    let projection = derive_from_rollout(&restored);
    assert_eq!(projection.spine.cursor.to_string(), "1.1");
    assert_eq!(projection.context.len(), 4);
    assert_eq!(text(&projection.context[0]), "[U1]\nfirst");
}

#[test]
fn rollback_selected_prefix_trims_pre_turn_context_updates() {
    let rollout = vec![
        message(
            "developer",
            "<permissions instructions>base</permissions instructions>",
        ),
        message("user", "first"),
        message("assistant", "first response"),
        message(
            "developer",
            "<collaboration_mode>rolled back</collaboration_mode>",
        ),
        token_count(17),
        message("user", "second"),
        message("assistant", "second response"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.context.len(), 3);
    assert_eq!(
        text(&projection.context[0]),
        "<permissions instructions>base</permissions instructions>"
    );
    assert_eq!(text(&projection.context[1]), "[U1]\nfirst");
    assert_eq!(text(&projection.context[2]), "first response");
}

#[test]
fn fork_prefix_and_resume_full_rollout_are_pure_derivations() {
    let rollout = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        spine_success_output("open", tool_response::SpineToolResponse::Open),
        message("user", "detail"),
    ];
    let persisted = serde_json::to_string(&rollout).expect("serialize resumable rollout");
    let restored: Vec<RolloutItem> =
        serde_json::from_str(&persisted).expect("deserialize resumable rollout");
    let full = derive_from_rollout(&restored);
    let resumed = derive_from_rollout(&restored);
    let fork = derive_from_rollout(&restored[..3]);
    assert_eq!(full, resumed);
    assert_eq!(fork.spine.cursor.to_string(), "1.1");
    assert_eq!(fork.context.len(), 4);
}

#[test]
fn non_context_rollout_records_do_not_change_response_ordinals() {
    let response_only = vec![
        message("user", "request"),
        call("open", "spine.open", r#"{"summary":"task"}"#),
        output("open", Some(true), "ok"),
    ];
    let with_metadata = vec![
        response_only[0].clone(),
        RolloutItem::WorldState(WorldStateItem {
            full: true,
            state: serde_json::json!({"cwd":"/tmp"}),
        }),
        response_only[1].clone(),
        response_only[2].clone(),
    ];

    assert_eq!(
        derive_from_rollout(&response_only),
        derive_from_rollout(&with_metadata)
    );
}

#[test]
fn multimodal_user_items_are_preserved_while_text_is_tagged() {
    let item = ResponseItem::Message {
        id: Some("multimodal".to_string()),
        role: "user".to_string(),
        content: vec![
            ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            },
            ContentItem::InputText {
                text: "inspect image".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let projection = derive_from_rollout(&[RolloutItem::ResponseItem(item)]);
    let ResponseItem::Message { content, .. } = &projection.context[0] else {
        panic!("expected message");
    };
    assert!(matches!(content[0], ContentItem::InputImage { .. }));
    assert!(matches!(
        &content[1],
        ContentItem::InputText { text } if text == "[U1]\ninspect image"
    ));
}

#[test]
fn contextual_user_message_keeps_host_role_without_consuming_an_anchor() {
    let rollout = vec![
        message(
            "user",
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
        ),
        message("user", "actual request"),
    ];
    let projection = derive_from_rollout(&rollout);

    assert_eq!(
        text(&projection.context[0]),
        "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>"
    );
    assert_eq!(text(&projection.context[1]), "[U1]\nactual request");
}

#[test]
fn closed_memory_user_slot_preserves_the_complete_native_message() {
    let item = ResponseItem::Message {
        id: Some("multimodal-memory".to_string()),
        role: "user".to_string(),
        content: vec![
            ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            },
            ContentItem::InputText {
                text: "inspect image".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut expected = item.clone();
    tag_user_message(&mut expected, 1);
    let rollout = vec![
        call("open", "spine.open", r#"{"summary":"image task"}"#),
        output("open", Some(true), "ok"),
        RolloutItem::ResponseItem(item),
        call("close", "spine.close", r#"{"memory":"image inspected"}"#),
        output("close", Some(true), "ok"),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.context[0], expected);
    assert_eq!(
        text(&projection.context[1]),
        "<spine_memory node_id=\"1.1\">\nimage inspected\n</spine_memory>"
    );
}

#[test]
fn rollback_after_compact_keeps_native_replacement_baseline() {
    let replacement = vec![ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "native summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let rollout = vec![
        message("user", "first"),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(replacement.clone()),
            window_number: Some(1),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        message("user", "rolled back"),
        call("open", "spine.open", r#"{"summary":"discarded"}"#),
        output("open", Some(true), "ok"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "2");
    assert_eq!(projection.context, replacement);
}

#[test]
fn adapter_returns_materialized_context_without_persistence() {
    let rollout = vec![message("user", "request")];
    let projection = derive_from_rollout(&rollout);
    assert_eq!(projection.spine.cursor.to_string(), "1");
    assert_eq!(projection.context.len(), 1);
    assert_eq!(text(&projection.context[0]), "[U1]\nrequest");
}

use super::*;
use crate::session::tests::make_session_configuration_for_tests;
use crate::state::AutoCompactWindowSnapshot;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CreditsSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SpendControlLimitSnapshot;
use pretty_assertions::assert_eq;

fn response_message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn spine_transition_items(
    name: &str,
    arguments: String,
    call_id: &str,
    output_text: &str,
) -> Vec<ResponseItem> {
    vec![
        ResponseItem::FunctionCall {
            id: None,
            name: name.to_string(),
            namespace: None,
            arguments,
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_text(output_text.to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

fn trim_candidate_text(fragment: &str) -> String {
    assert!(!fragment.is_empty());
    let minimum_bytes = spine_core::TOOL_RESPONSE_TRIM_THRESHOLD_BYTES + 1;
    fragment.repeat(minimum_bytes.div_ceil(fragment.len()))
}

fn rollout_history(items: &[RolloutItem]) -> Vec<ResponseItem> {
    items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(item) => Some(item.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn spine_feature_off_clones_native_history_unchanged() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.disable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let message = response_message("user", "request");
    state.record_items(std::iter::once(&message), TruncationPolicy::Tokens(10_000));

    assert_eq!(state.clone_history().raw_items(), &[message]);
}

fn assert_incremental_matches_rebuild(
    configuration: &SessionConfiguration,
    live: &SessionState,
    rollout: &[RolloutItem],
) {
    let mut rebuilt = SessionState::new(configuration.clone());
    rebuilt.replace_history_from_rollout(rollout_history(rollout), None, rollout);
    assert_eq!(
        live.clone_history().raw_items(),
        rebuilt.clone_history().raw_items()
    );
}

#[tokio::test]
async fn trim_only_materialization_matches_full_rebuild() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.disable_spine_jit_for_test();
    configuration.enable_spine_trim_for_test();
    let mut live = SessionState::new(configuration.clone());
    let shell = ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"cat"}"#.to_string(),
        call_id: "shell".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let shell_output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "shell".to_string(),
        output: FunctionCallOutputPayload::from_text(trim_candidate_text("source")),
        internal_chat_message_metadata_passthrough: None,
    };
    let trim = spine_transition_items(
        "spine.trim",
        r#"{"TRIM_ID":"trim_1","op":"snip"}"#.to_string(),
        "trim",
        "Spine trim accepted.",
    );
    let mut rollout = vec![
        RolloutItem::ResponseItem(shell.clone()),
        RolloutItem::ResponseItem(shell_output.clone()),
    ];
    live.record_items([&shell, &shell_output], TruncationPolicy::Tokens(10_000));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    live.record_items(trim.iter(), TruncationPolicy::Tokens(10_000));
    rollout.extend(trim.into_iter().map(RolloutItem::ResponseItem));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);
}

#[tokio::test]
async fn spine_trim_only_projects_native_history_without_tree_messages() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.disable_spine_jit_for_test();
    session_configuration.enable_spine_trim_for_test();
    let mut state = SessionState::new(session_configuration);
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"cat"}"#.to_string(),
        call_id: "shell".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "shell".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(trim_candidate_text("x")),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    let projected = state.clone_history();
    assert_eq!(projected.raw_items().len(), 2);
    let ResponseItem::FunctionCallOutput { output, .. } = &projected.raw_items()[1] else {
        panic!("expected native tool output");
    };
    assert!(
        output
            .body
            .to_text()
            .unwrap()
            .starts_with("[TRIM_ID: trim_1]")
    );
}

#[tokio::test]
async fn spine_trim_validation_uses_only_the_previous_completed_toolcall() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.disable_spine_jit_for_test();
    session_configuration.enable_spine_trim_for_test();
    let mut state = SessionState::new(session_configuration);
    let call = |call_id: &str| ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"cat"}"#.to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = |call_id: &str, fragment: &str| ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(trim_candidate_text(fragment)),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    let first_call = call("shell-1");
    let second_call = call("shell-2");
    let first_output = output("shell-1", "first");
    let second_output = output("shell-2", "second");
    let items = [
        call("old-shell"),
        output("old-shell", "old"),
        first_call,
        second_call,
        first_output,
        second_output,
        ResponseItem::FunctionCall {
            id: None,
            name: "trim".to_string(),
            namespace: Some("spine".to_string()),
            arguments: r#"{"TRIM_ID":"trim_5","op":"snip"}"#.to_string(),
            call_id: "trim".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    state.record_items(items.iter(), TruncationPolicy::Tokens(10_000));

    let valid = spine_core::TrimRequest::parse(r#"{"TRIM_ID":"trim_5","op":"snip"}"#).unwrap();
    assert!(state.validate_spine_trim("trim", &valid).is_ok());
    let missed = spine_core::TrimRequest::parse(r#"{"TRIM_ID":"trim_1","op":"snip"}"#).unwrap();
    assert!(
        state
            .validate_spine_trim("trim", &missed)
            .unwrap_err()
            .contains("previous completed toolcall does not contain TRIM_ID trim_1")
    );

    let items = [
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "trim".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("trim failed".to_string()),
                success: Some(false),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "trim".to_string(),
            namespace: Some("spine".to_string()),
            arguments: r#"{"TRIM_ID":"trim_5","op":"snip"}"#.to_string(),
            call_id: "trim-retry".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    state.record_items(items.iter(), TruncationPolicy::Tokens(10_000));
    assert!(
        state
            .validate_spine_trim("trim-retry", &valid)
            .unwrap_err()
            .contains("previous completed toolcall does not contain TRIM_ID trim_5")
    );
    let retry_output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "trim-retry".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("trim failed again".to_string()),
            success: Some(false),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items(
        std::slice::from_ref(&retry_output),
        TruncationPolicy::Tokens(10_000),
    );

    let replacement = response_message("user", "compacted context");
    state.replace_history(vec![replacement], None);
    state.compact_spine_live();
    let trim_after_compact = ResponseItem::FunctionCall {
        id: None,
        name: "trim".to_string(),
        namespace: Some("spine".to_string()),
        arguments: r#"{"TRIM_ID":"trim_5","op":"snip"}"#.to_string(),
        call_id: "trim-after-compact".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items(
        std::slice::from_ref(&trim_after_compact),
        TruncationPolicy::Tokens(10_000),
    );
    assert!(
        state
            .validate_spine_trim("trim-after-compact", &valid)
            .unwrap_err()
            .contains("previous completed toolcall does not contain TRIM_ID trim_5")
    );
}

#[tokio::test]
// Verifies connector merging deduplicates repeated IDs.
async fn merge_connector_selection_deduplicates_entries() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);
    let merged = state.merge_connector_selection([
        "calendar".to_string(),
        "calendar".to_string(),
        "drive".to_string(),
    ]);

    assert_eq!(
        merged,
        HashSet::from(["calendar".to_string(), "drive".to_string()])
    );
}

#[tokio::test]
// Verifies clearing connector selection removes all saved IDs.
async fn clear_connector_selection_removes_entries() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);
    state.merge_connector_selection(["calendar".to_string()]);

    state.clear_connector_selection();

    assert_eq!(state.get_connector_selection(), HashSet::new());
}

#[tokio::test]
async fn set_rate_limits_defaults_limit_id_to_codex_when_missing() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 12.0,
            window_minutes: Some(60),
            resets_at: Some(100),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state
            .latest_rate_limits
            .as_ref()
            .and_then(|v| v.limit_id.clone()),
        Some("codex".to_string())
    );
}

#[tokio::test]
async fn replace_history_clears_auto_compact_window_prefill() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_auto_compact_window_estimated_prefill(/*tokens*/ 100);
    state.replace_history(Vec::new(), /*reference_context_item*/ None);

    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            prefill_input_tokens: None,
        }
    );
}

#[tokio::test]
async fn set_rate_limits_defaults_to_codex_when_limit_id_missing_after_other_bucket() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex_other".to_string()),
        limit_name: Some("codex_other".to_string()),
        primary: Some(RateLimitWindow {
            used_percent: 20.0,
            window_minutes: Some(60),
            resets_at: Some(200),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });
    state.set_rate_limits(RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 30.0,
            window_minutes: Some(60),
            resets_at: Some(300),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state
            .latest_rate_limits
            .as_ref()
            .and_then(|v| v.limit_id.clone()),
        Some("codex".to_string())
    );
}

#[tokio::test]
async fn set_rate_limits_carries_account_metadata_from_codex_to_codex_other() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: Some("codex".to_string()),
        primary: Some(RateLimitWindow {
            used_percent: 10.0,
            window_minutes: Some(60),
            resets_at: Some(100),
        }),
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: Some("50".to_string()),
        }),
        individual_limit: Some(SpendControlLimitSnapshot {
            limit: "25000".to_string(),
            used: "8000".to_string(),
            remaining_percent: 68,
            resets_at: 300,
        }),
        plan_type: Some(codex_protocol::account::PlanType::Plus),
        rate_limit_reached_type: None,
    });

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex_other".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 30.0,
            window_minutes: Some(120),
            resets_at: Some(200),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state.latest_rate_limits,
        Some(RateLimitSnapshot {
            limit_id: Some("codex_other".to_string()),
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 30.0,
                window_minutes: Some(120),
                resets_at: Some(200),
            }),
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("50".to_string()),
            }),
            individual_limit: Some(SpendControlLimitSnapshot {
                limit: "25000".to_string(),
                used: "8000".to_string(),
                remaining_percent: 68,
                resets_at: 300,
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
            rate_limit_reached_type: None,
        })
    );
}

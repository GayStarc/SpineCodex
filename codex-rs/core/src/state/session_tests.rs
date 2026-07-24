use super::*;
use crate::session::tests::make_session_configuration_for_tests;
use crate::state::AutoCompactWindowSnapshot;
use codex_protocol::AgentPath;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::CreditsSnapshot;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SpendControlLimitSnapshot;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
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

fn response_text(item: &ResponseItem) -> &str {
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected text");
    };
    text
}

fn spawn_call_and_output() -> (ResponseItem, ResponseItem) {
    let arguments = serde_json::json!({
        "tasks": [
            {"summary": "first", "prompt": "inspect first"},
            {"summary": "second", "prompt": "inspect second"},
            {"summary": "third", "prompt": "inspect third"}
        ]
    })
    .to_string();
    let receipt = serde_json::json!({
        "schema": spine_core::SPINE_SPAWN_RESULT_SCHEMA,
        "results": [
            {"ordinal": 0, "outcome": "completed", "memory_body": "first memory"},
            {
                "ordinal": 1,
                "outcome": "errored",
                "memory_body": "second memory",
                "diagnostic": "capacity"
            },
            {
                "ordinal": 2,
                "outcome": "aborted",
                "memory_body": "third memory",
                "diagnostic": "interrupted"
            }
        ]
    })
    .to_string();
    (
        ResponseItem::FunctionCall {
            id: None,
            name: "spawn".to_string(),
            namespace: Some("spine".to_string()),
            arguments,
            call_id: "spawn".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "spawn".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(receipt),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    )
}

fn spine_open_items(summary: &str, call_id: &str) -> Vec<ResponseItem> {
    spine_transition_items(
        "spine.open",
        serde_json::json!({"summary": summary}).to_string(),
        call_id,
        "Spine open accepted.",
    )
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

fn token_count(input_tokens: i64) -> RolloutItem {
    RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::TokenCount(
        token_count_event(input_tokens),
    ))
}

fn encrypted_reasoning(encoded_bytes: usize) -> ResponseItem {
    ResponseItem::Reasoning {
        id: Some("old-reasoning".to_string()),
        summary: Vec::new(),
        content: None,
        encrypted_content: Some("r".repeat(encoded_bytes)),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn spine_call(name: &str, arguments: &str, call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: format!("spine.{name}"),
        namespace: None,
        arguments: arguments.to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_output(call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text.to_string()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

fn record_native_and_rollout(state: &mut SessionState, items: &[ResponseItem]) {
    state.record_items(items.iter(), TruncationPolicy::Tokens(1_000_000));
    state.append_spine_inputs(
        &items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect::<Vec<_>>(),
    );
}

fn active_usage(state: &SessionState, server_reasoning_included: bool) -> i64 {
    state.get_total_token_usage(server_reasoning_included)
}

fn usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens.saturating_sub(1),
        output_tokens: 1,
        total_tokens,
        ..TokenUsage::default()
    }
}

fn record_first_projected_request_usage(
    state: &mut SessionState,
    estimated_input_tokens: i64,
    provider_usage: &TokenUsage,
    model: &str,
) {
    let claim = state
        .begin_auto_compact_window_sampling_request(estimated_input_tokens)
        .expect("first projected sampling request should claim the window prefill");
    state.update_token_info_from_sampling_usage(provider_usage, Some(272_000), model);
    state.record_auto_compact_window_server_prefill_from_usage(Some(claim), provider_usage, model);
}

fn token_count_event(input_tokens: i64) -> TokenCountEvent {
    TokenCountEvent {
        info: Some(TokenUsageInfo {
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage {
                input_tokens,
                total_tokens: input_tokens,
                ..TokenUsage::default()
            },
            model_context_window: Some(200_000),
        }),
        rate_limits: None,
    }
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

fn current_rollout_history(items: &[RolloutItem]) -> Vec<ResponseItem> {
    let (replacement, suffix) = match items
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)))
    {
        Some(index) => {
            let RolloutItem::Compacted(compacted) = &items[index] else {
                unreachable!("matched compacted rollout item")
            };
            (
                compacted.replacement_history.clone().unwrap_or_default(),
                &items[index + 1..],
            )
        }
        None => (Vec::new(), items),
    };
    let mut history = ContextManager::new();
    history.replace(replacement);
    for item in suffix {
        match item {
            RolloutItem::ResponseItem(item) => {
                history.record_items([item], TruncationPolicy::Tokens(10_000));
            }
            RolloutItem::InterAgentCommunication(communication) => {
                let item = communication.to_model_input_item();
                history.record_items([&item], TruncationPolicy::Tokens(10_000));
            }
            _ => {}
        }
    }
    history.into_raw_items()
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

#[tokio::test]
async fn spine_fresh_pressure_preserves_basecodex_behavior() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let state = SessionState::new(session_configuration);

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::ProviderValid
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ false),
        state
            .clone_history()
            .get_total_token_usage(/*server_reasoning_included*/ false)
    );
}

#[tokio::test]
async fn spine_local_append_keeps_provider_usage_valid() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let initial = vec![
        response_message("user", "request"),
        response_message("assistant", "response"),
    ];
    record_native_and_rollout(&mut state, &initial);
    let provider_usage = usage(10_000);
    record_first_projected_request_usage(&mut state, 5_000, &provider_usage, "gpt-test");
    let before = state.projected_history_snapshot();

    let local_item = response_message("user", "local follow-up");
    record_native_and_rollout(&mut state, std::slice::from_ref(&local_item));
    state.reconcile_projected_history(before.as_deref());

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::ProviderValid
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        state
            .clone_history()
            .get_total_token_usage(/*server_reasoning_included*/ true)
    );
    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            estimated_prefill_input_tokens: Some(5_000),
            server_prefill_input_tokens: Some(provider_usage.input_tokens),
        }
    );
}

#[tokio::test]
async fn spine_large_tool_output_uses_provider_usage_plus_pending_tail() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "shell_command".to_string(),
        namespace: None,
        arguments: r#"{"command":"large-output"}"#.to_string(),
        call_id: "large-output".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    record_native_and_rollout(&mut state, &[response_message("user", "request"), call]);
    let provider_usage = usage(10_000);
    state.update_token_info_from_sampling_usage(&provider_usage, Some(272_000), "gpt-test");
    let before = state.projected_history_snapshot();

    let output = function_output("large-output", &"x".repeat(50_000));
    state.record_items([&output], TruncationPolicy::Tokens(2_000));
    state.append_spine_inputs(&[RolloutItem::ResponseItem(output)]);
    state.reconcile_projected_history(before.as_deref());

    let expected = state
        .clone_history()
        .get_total_token_usage(/*server_reasoning_included*/ true);
    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::ProviderValid
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        expected
    );
    assert!(
        expected > provider_usage.total_tokens,
        "the retained tool output must be counted as BaseCodex pending tail"
    );
}

#[tokio::test]
async fn spine_provider_valid_reasoning_header_uses_projected_basecodex_formula() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    record_native_and_rollout(
        &mut state,
        &[
            response_message("user", "first request"),
            encrypted_reasoning(80_000),
            response_message("assistant", "first response"),
            response_message("user", "second request"),
            response_message("assistant", "second response"),
        ],
    );
    let provider_usage = usage(10_000);
    state.update_token_info_from_sampling_usage(&provider_usage, Some(272_000), "gpt-test");

    let excluded = active_usage(&state, /*server_reasoning_included*/ true);
    let included = active_usage(&state, /*server_reasoning_included*/ false);
    assert_eq!(excluded, provider_usage.total_tokens);
    assert!(
        included > excluded,
        "old encrypted reasoning must be counted exactly when the server omitted it"
    );
    assert_eq!(
        included,
        state
            .clone_history()
            .get_total_token_usage(/*server_reasoning_included*/ false)
    );
}

#[tokio::test]
async fn spine_model_append_uses_current_estimate_until_provider_usage() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let initial = vec![
        response_message("user", "request"),
        response_message("assistant", "first response"),
    ];
    record_native_and_rollout(&mut state, &initial);
    state.update_token_info_from_sampling_usage(&usage(250_000), Some(272_000), "gpt-test");
    let before = state.projected_history_snapshot();

    let model_item = response_message("assistant", "response without usage");
    record_native_and_rollout(&mut state, std::slice::from_ref(&model_item));
    state.reconcile_projected_history(before.as_deref());

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::EstimateCurrent
    );
    let expected = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("current projection estimate");
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        expected
    );

    state.update_token_info_from_sampling_usage(&usage(12_000), Some(272_000), "gpt-test");
    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::ProviderValid
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        12_000
    );
}

#[tokio::test]
async fn spine_compact_usage_does_not_validate_normal_sampling_projection() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    record_native_and_rollout(
        &mut state,
        &[response_message("user", "normal sampling input")],
    );
    record_first_projected_request_usage(&mut state, 0, &usage(10_000), "gpt-test");

    let expected = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("current projection estimate");

    state.update_token_info_from_non_sampling_usage(&usage(250_000), Some(272_000));

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::EstimateCurrent,
        "a compact or other non-sampling request does not measure the normal h(PS) input"
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        expected
    );
}

#[tokio::test]
async fn spine_nonappend_rewrite_discards_stale_usage_but_preserves_window_prefill() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let before_close_output = vec![
        response_message("user", "root request"),
        spine_call("open", r#"{"summary":"child"}"#, "open"),
        function_output("open", "Spine open accepted."),
        response_message("user", "large child detail"),
        encrypted_reasoning(160_000),
        response_message("assistant", "child work"),
        response_message("user", "close the child"),
        spine_call("close", r#"{"memory":"short memory"}"#, "close"),
    ];
    record_native_and_rollout(&mut state, &before_close_output);
    let stale_usage = usage(250_000);
    record_first_projected_request_usage(&mut state, 0, &stale_usage, "gpt-test");
    let before = state.projected_history_snapshot();

    let close_output = function_output("close", "Spine close accepted.");
    record_native_and_rollout(&mut state, std::slice::from_ref(&close_output));
    assert!(
        !state
            .clone_history()
            .raw_items()
            .starts_with(before.as_deref().expect("projected snapshot"))
    );
    state.reconcile_projected_history(before.as_deref());

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::EstimateCurrent
    );
    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            estimated_prefill_input_tokens: Some(0),
            server_prefill_input_tokens: Some(stale_usage.input_tokens),
        }
    );
    let expected = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("current projection estimate");
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        expected
    );
    assert!(
        expected < stale_usage.total_tokens,
        "the stale provider scalar must not contribute after the rewrite"
    );
    assert_eq!(
        state
            .context_pressure(/*server_reasoning_included*/ true, "gpt-test")
            .body_after_prefix_tokens,
        expected,
        "stale scoped pressure must use the preserved estimated baseline"
    );
}

#[tokio::test]
async fn spine_later_usage_cannot_claim_a_no_usage_first_request_prefill() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    record_native_and_rollout(
        &mut state,
        &[response_message("user", "first request without usage")],
    );
    let _first_claim = state
        .begin_auto_compact_window_sampling_request(/*estimated_input_tokens*/ 0)
        .expect("first request should claim the window");

    let model_output = response_message("assistant", &"x".repeat(40_000));
    record_native_and_rollout(&mut state, std::slice::from_ref(&model_output));
    state.mark_projected_usage_stale();
    assert!(
        state
            .begin_auto_compact_window_sampling_request(/*estimated_input_tokens*/ 10_000)
            .is_none(),
        "the next request must not inherit first-request prefill ownership"
    );

    let later_usage = usage(250_000);
    state.update_token_info_from_sampling_usage(&later_usage, Some(272_000), "gpt-test");
    state.record_auto_compact_window_server_prefill_from_usage(
        /*claim*/ None,
        &later_usage,
        "gpt-test",
    );

    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            estimated_prefill_input_tokens: Some(0),
            server_prefill_input_tokens: None,
        }
    );
    let estimated = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("current projection estimate");
    let pressure = state.context_pressure(/*server_reasoning_included*/ true, "gpt-test");
    assert_eq!(pressure.active_context_tokens, later_usage.total_tokens);
    assert_eq!(
        pressure.body_after_prefix_tokens, estimated,
        "without U0 the scoped budget must remain in the estimator coordinate"
    );
    assert_eq!(pressure.body_after_prefix_prefill_tokens, Some(0));
}

#[tokio::test]
async fn spine_model_change_uses_estimated_body_after_prefix_coordinate() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    record_native_and_rollout(
        &mut state,
        &[response_message("user", "request on first model")],
    );
    let first_usage = usage(100_000);
    record_first_projected_request_usage(&mut state, 0, &first_usage, "first-model");

    let estimated = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("current projection estimate");
    let pressure = state.context_pressure(/*server_reasoning_included*/ true, "second-model");
    assert_eq!(pressure.active_context_tokens, first_usage.total_tokens);
    assert_eq!(pressure.body_after_prefix_tokens, estimated);
    assert_eq!(pressure.body_after_prefix_prefill_tokens, Some(0));
}

#[tokio::test]
async fn spine_model_switch_back_does_not_mix_latest_usage_with_old_model_prefill() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    record_native_and_rollout(
        &mut state,
        &[response_message("user", "request on model A")],
    );
    let model_a_usage = usage(100_000);
    record_first_projected_request_usage(&mut state, 0, &model_a_usage, "model-A");

    let model_b_usage = usage(90_000);
    state.update_token_info_from_sampling_usage(&model_b_usage, Some(272_000), "model-B");

    let estimated = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("current projection estimate");
    let pressure = state.context_pressure(/*server_reasoning_included*/ true, "model-A");
    assert_eq!(pressure.active_context_tokens, model_b_usage.total_tokens);
    assert_eq!(
        pressure.body_after_prefix_tokens, estimated,
        "active usage from model B cannot be subtracted from model A's U0 after switching back"
    );
    assert_eq!(pressure.body_after_prefix_prefill_tokens, Some(0));
}

#[tokio::test]
async fn spine_resume_keeps_restored_usage_stale() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let closed_rollout = vec![
        response_message("user", "root request"),
        spine_call("open", r#"{"summary":"child"}"#, "open"),
        function_output("open", "Spine open accepted."),
        response_message("user", "large child detail"),
        spine_call("close", r#"{"memory":"short memory"}"#, "close"),
        function_output("close", "Spine close accepted."),
    ];
    state.record_items(closed_rollout.iter(), TruncationPolicy::Tokens(1_000_000));
    state.replace_spine_rollout(
        &closed_rollout
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect::<Vec<_>>(),
    );
    state.set_token_info(Some(TokenUsageInfo {
        total_token_usage: TokenUsage::default(),
        last_token_usage: usage(250_000),
        model_context_window: Some(272_000),
    }));

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::EstimateCurrent
    );
    assert!(active_usage(&state, /*server_reasoning_included*/ false) < 244_800);
}

#[tokio::test]
async fn spine_set_token_usage_full_restores_forced_provider_basis() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    state.mark_projected_usage_stale();

    state.set_token_usage_full(272_000);

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::ProviderValid
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        272_000
    );
}

#[tokio::test]
async fn spine_history_replacement_ignores_recomputed_ui_scalar() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let stale_usage = usage(250_000);
    record_first_projected_request_usage(&mut state, 100, &stale_usage, "gpt-test");

    state.replace_history(
        vec![response_message("user", "short replacement")],
        /*reference_context_item*/ None,
    );
    state.set_token_info(Some(TokenUsageInfo {
        total_token_usage: TokenUsage::default(),
        last_token_usage: usage(180_000),
        model_context_window: Some(272_000),
    }));

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::EstimateCurrent
    );
    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            estimated_prefill_input_tokens: None,
            server_prefill_input_tokens: None,
        }
    );
    let expected = state
        .clone_history()
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("replacement estimate");
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        expected,
        "recomputed TokenCount is UI state, not a provider baseline"
    );
    assert_ne!(expected, 180_000);
}

#[tokio::test]
async fn spine_invalid_image_rewrite_invalidates_usage_but_preserves_window_prefill() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "view_image".to_string(),
        namespace: None,
        arguments: r#"{"path":"poisoned.png"}"#.to_string(),
        call_id: "poisoned-image".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "poisoned-image".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,poisoned".to_string(),
                    detail: None,
                },
            ]),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    record_native_and_rollout(
        &mut state,
        &[response_message("user", "show image"), call, output],
    );
    let stale_usage = usage(250_000);
    record_first_projected_request_usage(&mut state, 0, &stale_usage, "gpt-test");

    assert!(state.replace_last_turn_images("Invalid image"));

    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::EstimateCurrent
    );
    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            estimated_prefill_input_tokens: Some(0),
            server_prefill_input_tokens: Some(stale_usage.input_tokens),
        }
    );
    let projected = state.clone_history();
    let projected_output = projected
        .raw_items()
        .iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCallOutput { output, .. } => Some(output),
            _ => None,
        })
        .expect("projected function output");
    assert_eq!(
        projected_output.body,
        FunctionCallOutputBody::ContentItems(vec![FunctionCallOutputContentItem::InputText {
            text: "Invalid image".to_string(),
        },])
    );
    let expected = projected
        .estimate_token_count_with_base_instructions(&BaseInstructions {
            text: state.session_configuration.base_instructions().to_string(),
        })
        .expect("sanitized projection estimate");
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ true),
        expected
    );
    assert_eq!(
        state
            .context_pressure(/*server_reasoning_included*/ true, "gpt-test")
            .body_after_prefix_tokens,
        expected
    );
    assert!(expected < stale_usage.total_tokens);
}

#[tokio::test]
async fn spine_feature_off_pressure_matches_native_history() {
    for server_reasoning_included in [false, true] {
        let mut session_configuration = make_session_configuration_for_tests().await;
        session_configuration.disable_spine_jit_for_test();
        session_configuration.disable_spine_trim_for_test();
        let mut state = SessionState::new(session_configuration);
        let items = vec![
            response_message("user", "first request"),
            encrypted_reasoning(80_000),
            response_message("assistant", "first response"),
            response_message("user", "second request"),
        ];
        state.record_items(items.iter(), TruncationPolicy::Tokens(1_000_000));
        state.update_token_info_from_sampling_usage(&usage(10_000), Some(272_000), "gpt-test");
        state.mark_projected_usage_stale();

        assert_eq!(
            active_usage(&state, server_reasoning_included),
            state
                .history
                .get_total_token_usage(server_reasoning_included)
        );
    }
}

#[tokio::test]
async fn feature_off_body_after_prefix_keeps_basecodex_server_prefill_behavior() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.disable_spine_jit_for_test();
    session_configuration.disable_spine_trim_for_test();
    let mut state = SessionState::new(session_configuration);
    state.set_auto_compact_window_estimated_prefill(/*tokens*/ 100);
    let provider_usage = usage(10_000);
    state.update_token_info_from_sampling_usage(&provider_usage, Some(272_000), "base-model");
    state.record_auto_compact_window_server_prefill_from_usage(
        /*claim*/ None,
        &provider_usage,
        "base-model",
    );

    let pressure =
        state.context_pressure(/*server_reasoning_included*/ true, "different-model");
    assert_eq!(pressure.active_context_tokens, provider_usage.total_tokens);
    assert_eq!(
        pressure.body_after_prefix_tokens,
        provider_usage
            .total_tokens
            .saturating_sub(provider_usage.input_tokens)
    );
    assert_eq!(
        pressure.body_after_prefix_prefill_tokens,
        Some(provider_usage.input_tokens)
    );
}

#[tokio::test]
async fn historical_code_mode_carrier_is_projected_with_spine_features_off() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.disable_spine_jit_for_test();
    session_configuration.disable_spine_trim_for_test();
    let mut state = SessionState::new(session_configuration);
    assert!(state.spine_rollout.is_none());

    let call_id = "historical-exec";
    let exec = ResponseItem::CustomToolCall {
        id: None,
        status: None,
        call_id: call_id.to_string(),
        name: "exec".to_string(),
        namespace: None,
        input: "text('visible exec output')".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let carrier = CodeModeOutputCarrierV1::new(
        FunctionCallOutputBody::Text("visible exec output".to_string()),
        Some(true),
        "historical-cell".to_string(),
        vec![NestedSpineCallV1 {
            runtime_call_id: "historical-open".to_string(),
            invocation_ordinal: 0,
            name: NestedSpineToolName::Open,
            arguments: r#"{"summary":"historical task"}"#.to_string(),
            output: NestedSpineOutputV1 {
                success: true,
                body: "Spine open accepted.".to_string(),
            },
        }],
    )
    .expect("valid historical carrier");
    let carrier_output = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.to_string(),
        name: Some(CODE_MODE_SPINE_CARRIER_MARKER.to_string()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(encode_carrier(&carrier).expect("encode carrier")),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    let rollout = vec![
        RolloutItem::ResponseItem(exec.clone()),
        RolloutItem::ResponseItem(carrier_output.clone()),
    ];
    state.record_items([&exec, &carrier_output], TruncationPolicy::Tokens(10_000));
    state.replace_spine_rollout(&rollout);

    let projected = state.clone_history();
    assert_eq!(projected.raw_items().len(), 2);
    assert_eq!(projected.raw_items()[0], exec);
    let ResponseItem::CustomToolCallOutput {
        name,
        output,
        call_id: projected_call_id,
        ..
    } = &projected.raw_items()[1]
    else {
        panic!("expected projected exec output");
    };
    assert_eq!(projected_call_id, call_id);
    assert_eq!(name, &None);
    assert_eq!(
        output.body,
        FunctionCallOutputBody::Text("visible exec output".to_string())
    );
    assert_eq!(output.success, Some(true));
    assert!(state.spine_tree_update().is_none());

    state.update_token_info_from_sampling_usage(
        &TokenUsage {
            input_tokens: 9_000,
            output_tokens: 1_000,
            total_tokens: 10_000,
            ..TokenUsage::default()
        },
        Some(272_000),
        "gpt-test",
    );
    assert_eq!(
        state.projected_usage_basis,
        ProjectedUsageBasis::ProviderValid
    );
    assert_eq!(
        active_usage(&state, /*server_reasoning_included*/ false),
        state.history.get_total_token_usage(false),
        "historical carrier projection must not enable Spine accounting when features are off"
    );
}

#[tokio::test]
async fn spine_jit_is_enabled_in_default_session_state() {
    let session_configuration = make_session_configuration_for_tests().await;
    assert!(session_configuration.spine_jit_enabled());
    assert!(session_configuration.spine_status_enabled());
    assert!(
        SessionState::new(session_configuration)
            .spine_tree_update()
            .is_some()
    );
}

#[tokio::test]
async fn spine_status_requires_both_spine_jit_and_spine_status() {
    let enabled = make_session_configuration_for_tests().await;
    assert!(enabled.spine_status_enabled());
    assert!(
        SessionState::new(enabled)
            .spine_status_prompt_overlay(None)
            .is_some()
    );

    let mut status_disabled = make_session_configuration_for_tests().await;
    status_disabled.disable_spine_status_for_test();
    assert!(!status_disabled.spine_status_enabled());
    assert!(
        SessionState::new(status_disabled)
            .spine_status_prompt_overlay(None)
            .is_none()
    );

    let mut jit_disabled = make_session_configuration_for_tests().await;
    jit_disabled.disable_spine_jit_for_test();
    assert!(!jit_disabled.spine_status_enabled());
    assert!(
        SessionState::new(jit_disabled)
            .spine_status_prompt_overlay(None)
            .is_none()
    );
}

fn assert_incremental_matches_rebuild(
    configuration: &SessionConfiguration,
    live: &SessionState,
    rollout: &[RolloutItem],
) {
    let mut rebuilt = SessionState::new(configuration.clone());
    rebuilt.replace_history_from_rollout(current_rollout_history(rollout), None, rollout);
    assert_eq!(
        live.clone_history().raw_items(),
        rebuilt.clone_history().raw_items()
    );
    assert_eq!(live.spine_tree_update(), rebuilt.spine_tree_update());
}

#[tokio::test]
async fn incremental_materialization_matches_full_rebuild_across_transition_matrix() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.enable_spine_jit_for_test();
    configuration.enable_spine_trim_for_test();
    let mut live = SessionState::new(configuration.clone());
    let mut rollout = Vec::new();

    let user = response_message("user", "request");
    live.record_items([&user], TruncationPolicy::Tokens(10_000));
    rollout.push(RolloutItem::ResponseItem(user));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let open = spine_open_items("child", "open");
    live.record_items(open.iter(), TruncationPolicy::Tokens(10_000));
    rollout.extend(open.iter().cloned().map(RolloutItem::ResponseItem));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let detail = response_message("user", "child detail");
    live.record_items([&detail], TruncationPolicy::Tokens(10_000));
    rollout.push(RolloutItem::ResponseItem(detail));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

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
    live.record_items([&shell, &shell_output], TruncationPolicy::Tokens(10_000));
    rollout.extend([
        RolloutItem::ResponseItem(shell),
        RolloutItem::ResponseItem(shell_output),
    ]);
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let trim = spine_transition_items(
        "spine.trim",
        r#"{"TRIM_ID":"trim_1","op":"snip"}"#.to_string(),
        "trim",
        "Spine trim accepted.",
    );
    live.record_items(trim.iter(), TruncationPolicy::Tokens(10_000));
    rollout.extend(trim.iter().cloned().map(RolloutItem::ResponseItem));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let next = spine_transition_items(
        "spine.next",
        r#"{"summary":"sibling","memory":"child done"}"#.to_string(),
        "next",
        "Spine next accepted.",
    );
    live.record_items(next.iter(), TruncationPolicy::Tokens(10_000));
    rollout.extend(next.iter().cloned().map(RolloutItem::ResponseItem));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let sibling_detail = response_message("user", "sibling detail");
    live.record_items([&sibling_detail], TruncationPolicy::Tokens(10_000));
    rollout.push(RolloutItem::ResponseItem(sibling_detail));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let close = spine_transition_items(
        "spine.close",
        r#"{"memory":"sibling done"}"#.to_string(),
        "close",
        "Spine close accepted.",
    );
    live.record_items(close.iter(), TruncationPolicy::Tokens(10_000));
    rollout.extend(close.iter().cloned().map(RolloutItem::ResponseItem));
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);

    let replacement = response_message("user", "compacted context");
    let compacted = RolloutItem::Compacted(CompactedItem {
        message: "compact memory".to_string(),
        replacement_history: Some(vec![replacement.clone()]),
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    });
    live.replace_history(vec![replacement], None);
    live.compact_spine_live();
    rollout.push(compacted);
    assert_incremental_matches_rebuild(&configuration, &live, &rollout);
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
async fn spine_event_handler_commits_projected_context_to_history() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "spine.open".to_string(),
        namespace: None,
        arguments: r#"{"summary":"task"}"#.to_string(),
        call_id: "open".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "open".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("Spine open accepted.".to_string()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items([&call, &output], TruncationPolicy::Tokens(10_000));

    assert_eq!(state.clone_history().raw_items(), state.history.raw_items());
    assert_eq!(state.history.raw_items().len(), 3);
    assert!(response_text(&state.history.raw_items()[0]).starts_with("<spine_node"));
}

#[tokio::test]
async fn spine_live_append_uses_source_ordinals_across_event_items() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let user = response_message("user", "request");
    state.record_items(std::iter::once(&user), TruncationPolicy::Tokens(10_000));
    state.observe_token_count(token_count_event(1_000));

    let call = ResponseItem::FunctionCall {
        id: None,
        name: "open".to_string(),
        namespace: Some("spine".to_string()),
        arguments: r#"{"summary":"task"}"#.to_string(),
        call_id: "open".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items(std::iter::once(&call), TruncationPolicy::Tokens(10_000));

    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "open".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("Spine open accepted.".to_string()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items(std::iter::once(&output), TruncationPolicy::Tokens(10_000));

    let projected = state.clone_history();
    assert!(response_text(&projected.raw_items()[0]).contains("request"));
    assert_eq!(
        state
            .spine_tree_update()
            .expect("Spine tree snapshot should be enabled")
            .active_node_id,
        "1.1"
    );
}

#[tokio::test]
async fn spine_projection_reuses_host_truncated_tool_output() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    session_configuration.disable_spine_trim_for_test();
    let mut state = SessionState::new(session_configuration);
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"large-output"}"#.to_string(),
        call_id: "large-output".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "large-output".to_string(),
        output: FunctionCallOutputPayload::from_text("x".repeat(50_000)),
        internal_chat_message_metadata_passthrough: None,
    };
    state.record_items([&call, &output], TruncationPolicy::Tokens(50));
    let native_output = state.history.raw_items()[1].clone();

    let projected = state.clone_history();
    assert_eq!(projected.raw_items()[1], native_output);
    let ResponseItem::FunctionCallOutput { output, .. } = &projected.raw_items()[1] else {
        panic!("expected function output");
    };
    assert!(output.body.to_text().unwrap().len() < 1_000);
}

#[tokio::test]
async fn spine_materialization_updates_trimmed_boundaries_and_rebuilds_after_compact() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
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
        output: FunctionCallOutputPayload::from_text(trim_candidate_text("source")),
        internal_chat_message_metadata_passthrough: None,
    };
    let trim_call = ResponseItem::FunctionCall {
        id: None,
        name: "trim".to_string(),
        namespace: Some("spine".to_string()),
        arguments: r#"{"TRIM_ID":"trim_1","op":"snip"}"#.to_string(),
        call_id: "trim".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let trim_output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "trim".to_string(),
        output: FunctionCallOutputPayload::from_text("Spine trim accepted.".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let rollout = vec![
        RolloutItem::ResponseItem(call.clone()),
        RolloutItem::ResponseItem(output.clone()),
        RolloutItem::ResponseItem(trim_call.clone()),
        RolloutItem::ResponseItem(trim_output.clone()),
    ];
    state.record_items([&call, &output], TruncationPolicy::Tokens(10_000));
    let tagged = state.clone_history();
    let ResponseItem::FunctionCallOutput { output, .. } = &tagged.raw_items()[1] else {
        panic!("expected tagged shell output");
    };
    assert!(
        output
            .body
            .to_text()
            .expect("tagged output should be text")
            .starts_with("[TRIM_ID: trim_1]")
    );
    state.record_items([&trim_call, &trim_output], TruncationPolicy::Tokens(10_000));
    let snipped = state.clone_history();
    let ResponseItem::FunctionCallOutput { output, .. } = &snipped.raw_items()[1] else {
        panic!("expected snipped shell output");
    };
    assert_eq!(
        output.body.to_text().as_deref(),
        Some(crate::spine::TOOL_RESULT_CLEARED_MESSAGE)
    );

    let replacement = response_message("user", "compacted context");
    state.replace_history(vec![replacement.clone()], None);
    state.compact_spine_live();
    assert_eq!(state.clone_history().raw_items(), &[replacement]);

    state.replace_history_from_rollout(snipped.raw_items().to_vec(), None, &rollout);
    assert_eq!(state.clone_history().raw_items(), snipped.raw_items());
}

#[tokio::test]
async fn spawn_context_install_is_atomic_and_independently_feature_gated() {
    let mut enabled = make_session_configuration_for_tests().await;
    enabled.enable_spine_jit_for_test();
    enabled.enable_spine_spawn_for_test();
    let mut state = SessionState::new(enabled);
    let (call, output) = spawn_call_and_output();

    state.record_items([&call], TruncationPolicy::Tokens(10_000));
    assert_eq!(state.clone_history().raw_items(), &[call.clone()]);
    assert_eq!(
        state.spine_tree_update().expect("tree enabled").nodes.len(),
        1
    );

    state.record_items([&output], TruncationPolicy::Tokens(10_000));
    let projected = state.clone_history();
    assert_eq!(projected.raw_items().len(), 8);
    assert!(response_text(&projected.raw_items()[2]).contains("spine_spawn_evidence"));
    assert!(response_text(&projected.raw_items()[3]).contains("first memory"));
    assert!(response_text(&projected.raw_items()[4]).contains("spine_spawn_evidence"));
    assert!(response_text(&projected.raw_items()[5]).contains("second memory"));
    assert!(response_text(&projected.raw_items()[6]).contains("spine_spawn_evidence"));
    assert!(response_text(&projected.raw_items()[7]).contains("third memory"));
    let tree = state.spine_tree_update().expect("tree enabled");
    assert_eq!(tree.nodes.len(), 4);
    assert_eq!(tree.settled_spawn_call_ids, ["spawn"]);
    assert_eq!(
        tree.nodes[1].spawn_outcome,
        Some(codex_protocol::spine_tree::SpineSpawnOutcome::Completed)
    );
    assert_eq!(
        tree.nodes[2].spawn_outcome,
        Some(codex_protocol::spine_tree::SpineSpawnOutcome::Errored)
    );
    assert_eq!(
        tree.nodes[3].spawn_outcome,
        Some(codex_protocol::spine_tree::SpineSpawnOutcome::Aborted)
    );

    let mut disabled = make_session_configuration_for_tests().await;
    disabled.enable_spine_jit_for_test();
    let mut disabled_state = SessionState::new(disabled);
    disabled_state.record_items([&call, &output], TruncationPolicy::Tokens(10_000));
    assert_eq!(
        disabled_state.clone_history().raw_items(),
        &[call.clone(), output.clone()]
    );
    assert_eq!(
        disabled_state
            .spine_tree_update()
            .expect("Spine JIT remains enabled")
            .nodes
            .len(),
        1
    );
    assert!(
        response_text(
            &disabled_state
                .spine_status_prompt_overlay(None)
                .expect("status overlay enabled")
        )
        .contains("cursor=\"1\"")
    );
}

#[tokio::test]
async fn spine_tree_spawn_outcome_belongs_only_to_the_spawned_node() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    session_configuration.enable_spine_spawn_for_test();
    let mut state = SessionState::new(session_configuration);
    let (spawn_call, spawn_output) = spawn_call_and_output();

    state.append_spine_inputs(&[
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            id: None,
            name: "spine.open".to_string(),
            namespace: None,
            arguments: r#"{"summary":"parent"}"#.to_string(),
            call_id: "open-parent".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "open-parent".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Spine open accepted.".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::ResponseItem(spawn_call),
        RolloutItem::ResponseItem(spawn_output),
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            id: None,
            name: "spine.close".to_string(),
            namespace: None,
            arguments: r#"{"memory":"parent completed normally"}"#.to_string(),
            call_id: "close-parent".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "close-parent".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Spine close accepted.".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }),
    ]);

    let snapshot = state.spine_tree_update().expect("tree enabled");
    let parent = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == "1.1")
        .expect("parent node");
    let errored_child = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == "1.1.2")
        .expect("errored spawned child");

    assert_eq!(parent.spawn_outcome, None);
    assert_eq!(
        errored_child.spawn_outcome,
        Some(codex_protocol::spine_tree::SpineSpawnOutcome::Errored)
    );
}

#[tokio::test]
async fn context_transitions_publish_compact_and_replay_before_return() {
    let mut disabled = make_session_configuration_for_tests().await;
    disabled.disable_spine_jit_for_test();
    assert!(SessionState::new(disabled).spine_tree_update().is_none());

    let mut enabled = make_session_configuration_for_tests().await;
    enabled.enable_spine_jit_for_test();
    let mut state = SessionState::new(enabled);
    let initial = state
        .spine_tree_update()
        .expect("Spine tree snapshot should be enabled");
    assert_eq!(initial.active_node_id, "1");
    assert_eq!(initial.nodes.len(), 1);

    let opened_items = spine_open_items("task", "open");
    let opened_rollout = opened_items
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    state.record_items(opened_items.iter(), TruncationPolicy::Tokens(10_000));
    let opened = state
        .spine_tree_update()
        .expect("opened snapshot should be available");
    assert_eq!(opened.active_node_id, "1.1");
    assert_eq!(opened.nodes.len(), 2);
    assert_eq!(
        opened.nodes[0].status,
        codex_protocol::spine_tree::SpineTreeNodeStatus::Opened
    );
    assert_eq!(
        opened.nodes[1].status,
        codex_protocol::spine_tree::SpineTreeNodeStatus::Live
    );
    assert_eq!(opened.nodes[1].summary.as_deref(), Some("task"));

    let replacement = response_message("user", "compacted context");
    let compacted_item = CompactedItem {
        message: "native compact memory".to_string(),
        replacement_history: Some(vec![replacement.clone()]),
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    };
    state.replace_history(vec![replacement.clone()], None);
    state.compact_spine_live();
    let compacted = state
        .spine_tree_update()
        .expect("compacted snapshot should be available");
    assert_eq!(compacted.active_node_id, "2");
    assert_eq!(
        compacted.nodes[0].status,
        codex_protocol::spine_tree::SpineTreeNodeStatus::Compacted
    );
    assert_eq!(
        compacted.nodes.last().map(|node| node.status),
        Some(codex_protocol::spine_tree::SpineTreeNodeStatus::Live)
    );
    assert_eq!(state.clone_history().raw_items(), &[replacement]);

    let stale_items = spine_open_items("stale", "stale");
    let fresh_items = spine_open_items("fresh", "fresh");
    let mut compacted_rollout = opened_rollout.clone();
    compacted_rollout.push(RolloutItem::Compacted(compacted_item));
    compacted_rollout.extend(stale_items.into_iter().map(RolloutItem::ResponseItem));
    state.replace_history_from_rollout(fresh_items.clone(), None, &compacted_rollout);
    let restored = state.spine_tree_update().expect("restored snapshot");
    assert_eq!(restored.active_node_id, "2.1");
    assert_eq!(
        restored
            .nodes
            .iter()
            .find(|node| node.node_id == "1.1")
            .map(|node| node.status),
        Some(codex_protocol::spine_tree::SpineTreeNodeStatus::Compacted)
    );
    assert_eq!(
        restored
            .nodes
            .last()
            .and_then(|node| node.summary.as_deref()),
        Some("fresh")
    );
    assert!(
        !restored
            .nodes
            .iter()
            .any(|node| node.summary.as_deref() == Some("stale"))
    );
    assert_eq!(
        state.history.raw_items(),
        &[
            response_message(
                "developer",
                r#"<spine_node id="2.1" summary="fresh" status="live" />"#,
            ),
            fresh_items[0].clone(),
            fresh_items[1].clone(),
        ]
    );
    assert_eq!(
        state
            .take_spine_observer_effect()
            .and_then(|effect| effect.tree_update),
        Some(restored)
    );
    assert_eq!(state.take_spine_observer_effect(), None);

    state.replace_history_from_rollout(opened_items.clone(), None, &opened_rollout);
    assert_eq!(
        state
            .spine_tree_update()
            .expect("replayed snapshot should be available"),
        opened
    );
    state.replace_history_from_rollout(Vec::new(), None, &opened_rollout);
    assert_eq!(
        state
            .spine_tree_update()
            .expect("rolled-back snapshot should be available"),
        initial
    );
}

#[tokio::test]
async fn compact_replay_does_not_project_replacement_history_twice() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(configuration);
    let first = response_message("user", "first");
    let projected_first = response_message("user", "[U1]\nfirst");
    let after_compact = response_message("user", "after compact");
    let rollout = vec![
        RolloutItem::ResponseItem(first),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(vec![projected_first.clone()]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::ResponseItem(after_compact.clone()),
    ];

    state.replace_history_from_rollout(
        vec![projected_first.clone(), after_compact],
        None,
        &rollout,
    );

    assert_eq!(
        state.clone_history().raw_items(),
        &[
            projected_first,
            response_message("user", "[U2]\nafter compact"),
        ]
    );
}

#[tokio::test]
async fn legacy_compact_recovery_keeps_reconstructed_prefix_opaque() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(configuration);
    let before = response_message("user", "before compact");
    let replacement = response_message("user", "legacy baseline");
    let after = response_message("user", "after compact");
    let rollout = vec![
        RolloutItem::ResponseItem(before),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::ResponseItem(after.clone()),
    ];

    state.replace_history_from_rollout(vec![replacement.clone(), after], None, &rollout);

    assert_eq!(
        state.clone_history().raw_items(),
        &[replacement, response_message("user", "[U2]\nafter compact"),]
    );
}

#[tokio::test]
async fn archived_inter_agent_message_does_not_consume_a_user_anchor() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(configuration);
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("worker path"),
        Vec::new(),
        "archived child message".to_string(),
        /*trigger_turn*/ false,
    );
    let replacement = response_message("user", "replacement");
    let after = response_message("user", "after compact");
    let rollout = vec![
        RolloutItem::InterAgentCommunication(communication),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(vec![replacement.clone()]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::ResponseItem(after.clone()),
    ];

    state.replace_history_from_rollout(vec![replacement.clone(), after], None, &rollout);

    assert_eq!(
        state.clone_history().raw_items(),
        &[replacement, response_message("user", "[U1]\nafter compact"),]
    );
}

#[tokio::test]
async fn post_compact_usage_is_recovered_for_the_live_root_epoch() {
    let mut configuration = make_session_configuration_for_tests().await;
    configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(configuration);
    let replacement = response_message("assistant", "replacement");
    let after = response_message("user", "after compact");
    let rollout = vec![
        RolloutItem::ResponseItem(response_message("user", "before compact")),
        token_count(10_000),
        RolloutItem::Compacted(CompactedItem {
            message: "summary".to_string(),
            replacement_history: Some(vec![replacement.clone()]),
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
        RolloutItem::ResponseItem(after.clone()),
        token_count(42_000),
    ];

    state.replace_history_from_rollout(vec![replacement, after], None, &rollout);

    assert_eq!(
        state
            .spine_tree_update()
            .expect("recovered tree")
            .nodes
            .last()
            .and_then(|node| node.context_pressure.as_ref())
            .and_then(|pressure| pressure.current_input_tokens),
        Some(42_000)
    );
}

#[tokio::test]
async fn spine_tree_pressure_rederives_for_resume_and_rollback_prefixes() {
    let mut enabled = make_session_configuration_for_tests().await;
    enabled.enable_spine_jit_for_test();
    let mut state = SessionState::new(enabled);
    let open_prefix = vec![
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            id: None,
            name: "spine.open".to_string(),
            namespace: None,
            arguments: r#"{"summary":"task"}"#.to_string(),
            call_id: "open".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "open".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Spine open accepted.".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }),
        token_count(10_000),
    ];
    let mut full = open_prefix.clone();
    full.push(RolloutItem::ResponseItem(response_message(
        "user", "detail",
    )));
    full.push(token_count(42_000));

    state.replace_history_from_rollout(rollout_history(&full), None, &full);
    let resumed = state
        .spine_tree_update()
        .expect("resumed pressure snapshot");
    let active = resumed
        .nodes
        .iter()
        .find(|node| node.node_id == "1.1")
        .expect("active node");
    assert_eq!(
        active.context_pressure,
        Some(
            codex_protocol::spine_tree::SpineNodeContextPressureSnapshot {
                open_input_tokens: Some(10_000),
                current_input_tokens: Some(42_000),
                context_tokens: Some(32_000),
                problem: None,
            }
        )
    );

    let mut rolled_back = full;
    rolled_back.push(RolloutItem::EventMsg(
        codex_protocol::protocol::EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        }),
    ));
    state.replace_history_from_rollout(rollout_history(&open_prefix), None, &rolled_back);
    let rollback = state
        .spine_tree_update()
        .expect("rollback pressure snapshot");
    assert_eq!(
        rollback
            .nodes
            .iter()
            .find(|node| node.node_id == "1.1")
            .and_then(|node| node.context_pressure.as_ref())
            .and_then(|pressure| pressure.context_tokens),
        Some(0)
    );

    state.replace_history_from_rollout(rollout_history(&open_prefix), None, &open_prefix);
    assert_eq!(
        state
            .spine_tree_update()
            .expect("fork pressure snapshot")
            .nodes
            .iter()
            .find(|node| node.node_id == "1.1")
            .and_then(|node| node.context_pressure.as_ref())
            .and_then(|pressure| pressure.context_tokens),
        Some(0)
    );
}

#[tokio::test]
async fn live_append_after_rollback_preserves_canonical_source_ordinals() {
    let mut enabled = make_session_configuration_for_tests().await;
    enabled.enable_spine_jit_for_test();
    let mut live = SessionState::new(enabled.clone());
    let mut canonical = vec![
        RolloutItem::ResponseItem(response_message("user", "first")),
        RolloutItem::ResponseItem(response_message("user", "removed")),
        RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ThreadRolledBack(
            ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];
    let open_items = vec![
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            id: None,
            name: "open".to_string(),
            namespace: Some("spine".to_string()),
            arguments: r#"{"summary":"after rollback"}"#.to_string(),
            call_id: "open".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "open".to_string(),
            output: FunctionCallOutputPayload::from_text("Spine open accepted.".to_string()),
            internal_chat_message_metadata_passthrough: None,
        }),
    ];

    let first = response_message("user", "first");
    live.replace_history_from_rollout(vec![first.clone()], None, &canonical);
    let open_history = rollout_history(&open_items);
    live.record_items(open_history.iter(), TruncationPolicy::Tokens(10_000));
    let live_snapshot = live.spine_tree_update().expect("live snapshot");

    canonical.extend(open_items);
    let mut replayed = SessionState::new(enabled);
    let mut replayed_history = vec![first];
    replayed_history.extend(open_history);
    replayed.replace_history_from_rollout(replayed_history, None, &canonical);
    assert_eq!(
        live_snapshot,
        replayed.spine_tree_update().expect("replayed snapshot")
    );
}

#[tokio::test]
async fn spine_tree_snapshot_uses_the_closed_nodes_final_summary_slot() {
    let mut session_configuration = make_session_configuration_for_tests().await;
    session_configuration.enable_spine_jit_for_test();
    let mut state = SessionState::new(session_configuration);
    let items = [
        ResponseItem::FunctionCall {
            id: None,
            name: "spine.open".to_string(),
            namespace: None,
            arguments: r#"{"summary":"task"}"#.to_string(),
            call_id: "open".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "open".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Spine open accepted.".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        response_message("user", "detail"),
        ResponseItem::FunctionCall {
            id: None,
            name: "spine.close".to_string(),
            namespace: None,
            arguments: r#"{"memory":"done"}"#.to_string(),
            call_id: "close".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "close".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Spine close accepted.".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    state.record_items(items.iter(), TruncationPolicy::Tokens(10_000));

    let snapshot = state
        .spine_tree_update()
        .expect("closed snapshot should be available");
    let task = snapshot
        .nodes
        .iter()
        .find(|node| node.node_id == "1.1")
        .expect("closed task should be present");
    assert_eq!(task.memory_summary.as_deref(), Some("done"));
}

#[tokio::test]
async fn spine_control_validation_uses_the_pre_group_rollout_projection() {
    let mut disabled = make_session_configuration_for_tests().await;
    disabled.disable_spine_jit_for_test();
    disabled.disable_spine_trim_for_test();
    let disabled_state = SessionState::new(disabled);
    assert!(
        disabled_state
            .validate_spine_control(spine_core::SpineTool::Open)
            .is_err()
    );

    let mut enabled = make_session_configuration_for_tests().await;
    enabled.enable_spine_jit_for_test();
    enabled.disable_spine_trim_for_test();
    let mut state = SessionState::new(enabled);
    assert!(
        state
            .validate_spine_control(spine_core::SpineTool::Open)
            .is_ok()
    );
    assert!(
        state
            .validate_spine_control(spine_core::SpineTool::Close)
            .is_err()
    );
    assert!(
        state
            .validate_spine_control(spine_core::SpineTool::Next)
            .is_err()
    );

    let items = [
        ResponseItem::FunctionCall {
            id: None,
            name: "spine.open".to_string(),
            namespace: None,
            arguments: r#"{"summary":"task"}"#.to_string(),
            call_id: "open".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "open".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("Spine open accepted.".to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    state.record_items(items.iter(), TruncationPolicy::Tokens(10_000));
    assert!(
        state
            .validate_spine_control(spine_core::SpineTool::Close)
            .is_ok()
    );
    assert!(
        state
            .validate_spine_control(spine_core::SpineTool::Next)
            .is_ok()
    );
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
            estimated_prefill_input_tokens: None,
            server_prefill_input_tokens: None,
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

use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::items::AgentMessageContent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some("msg-1".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

async fn assert_no_spine_transition_status(session: &Session) {
    let history = session.clone_history().await;
    assert!(history.raw_items().iter().all(|item| !matches!(
        item,
        ResponseItem::Message { content, .. }
            if content.iter().any(|item| matches!(
                item,
                ContentItem::InputText { text }
                    if text.starts_with("<spine_tran_status ")
            ))
    )));
}

async fn assert_spine_transition_status_available(session: &Session, turn_context: &TurnContext) {
    let state = session.state.lock().await;
    assert!(
        state
            .spine_transition_status_item(
                Some(1),
                turn_context.model_info.auto_compact_token_limit(),
            )
            .is_some(),
        "test fixture must be able to generate a Spine transition status"
    );
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn failed_in_flight_output_panics_before_persisting_spine_transition_status() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    assert_spine_transition_status_available(&session, &turn_context).await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut in_flight = FuturesOrdered::new();
    let failed: BoxFuture<'static, CodexResult<ResponseInputItem>> =
        Box::pin(async { Err(CodexErr::Fatal("tool failed".to_string())) });
    in_flight.push_back(failed);

    let task = tokio::spawn({
        let session = Arc::clone(&session);
        let turn_context = Arc::clone(&turn_context);
        async move {
            drain_in_flight(
                &mut in_flight,
                session,
                turn_context,
                /*has_spine_control_call*/ true,
                Some(1),
            )
            .await
        }
    });
    let error = task
        .await
        .expect_err("debug builds panic on failed futures");
    assert!(error.is_panic());

    assert_no_spine_transition_status(&session).await;
}

#[cfg(not(debug_assertions))]
#[tokio::test]
async fn failed_in_flight_output_is_logged_without_persisting_spine_transition_status() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    assert_spine_transition_status_available(&session, &turn_context).await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let mut in_flight = FuturesOrdered::new();
    let failed: BoxFuture<'static, CodexResult<ResponseInputItem>> =
        Box::pin(async { Err(CodexErr::Fatal("tool failed".to_string())) });
    in_flight.push_back(failed);

    drain_in_flight(
        &mut in_flight,
        Arc::clone(&session),
        turn_context,
        /*has_spine_control_call*/ true,
        Some(1),
    )
    .await
    .expect("release builds log failed futures and finish draining");

    assert_no_spine_transition_status(&session).await;
}

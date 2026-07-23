use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SpineSpawnProgressEvent;
use tracing::warn;

use super::observer::CodexSpineObserverEffect;

pub(crate) async fn dispatch(
    session: &Session,
    turn_id: &str,
    effect: Option<CodexSpineObserverEffect>,
) {
    let Some(effect) = effect else {
        return;
    };
    if let Some(snapshot) = effect.tree_update {
        session
            .deliver_event_raw(Event {
                id: turn_id.to_string(),
                msg: EventMsg::SpineTreeUpdate(snapshot),
            })
            .await;
    }
    let (Some(projection), Some(memory)) = (
        session.spinetree_memory_projection(),
        effect.memory_projection,
    ) else {
        return;
    };
    match tokio::task::spawn_blocking(move || {
        projection.persist(&memory.entries, &memory.user_messages)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!("failed to publish Spine memory projection: {err:#}"),
        Err(err) => warn!("Spine memory projection task failed: {err}"),
    }
}

pub(crate) async fn emit_spawn_progress(
    session: &Session,
    turn_context: &TurnContext,
    progress: SpineSpawnProgressEvent,
) {
    session
        .deliver_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::SpineSpawnProgress(progress),
        })
        .await;
}

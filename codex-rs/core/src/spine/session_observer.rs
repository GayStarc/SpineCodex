use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SpineSpawnProgressEvent;

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

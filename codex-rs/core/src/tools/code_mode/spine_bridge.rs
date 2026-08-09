use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::models::FunctionCallOutputBody;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use tokio::sync::Notify;

pub(crate) const CODE_MODE_SPINE_CARRIER_MARKER: &str = "spine.code_mode.output.v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NestedSpineToolName {
    Open,
    Close,
    Next,
    Trim,
    Spawn,
}

impl NestedSpineToolName {
    fn exclusive_kind(self) -> Option<ExclusiveKind> {
        match self {
            Self::Open | Self::Close | Self::Next => Some(ExclusiveKind::ParserControl),
            Self::Spawn => Some(ExclusiveKind::Spawn),
            Self::Trim => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NestedSpineOutputV1 {
    pub(crate) success: bool,
    pub(crate) body: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NestedSpineCallV1 {
    pub(crate) runtime_call_id: String,
    pub(crate) invocation_ordinal: u64,
    pub(crate) name: NestedSpineToolName,
    pub(crate) arguments: String,
    pub(crate) output: NestedSpineOutputV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeModeOutputCarrierV1 {
    schema: String,
    pub(crate) visible_body: FunctionCallOutputBody,
    #[serde(deserialize_with = "deserialize_required_outer_success")]
    pub(crate) outer_success: Option<bool>,
    pub(crate) cell_id: String,
    pub(crate) nested_spine_calls: Vec<NestedSpineCallV1>,
}

fn deserialize_required_outer_success<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<bool>::deserialize(deserializer)
}

impl CodeModeOutputCarrierV1 {
    pub(crate) fn new(
        visible_body: FunctionCallOutputBody,
        outer_success: Option<bool>,
        cell_id: String,
        nested_spine_calls: Vec<NestedSpineCallV1>,
    ) -> Result<Self, String> {
        if cell_id.is_empty() {
            return Err("Code Mode Spine cell id must not be empty".to_string());
        }
        validate_nested_calls(&nested_spine_calls)?;
        Ok(Self {
            schema: CODE_MODE_SPINE_CARRIER_MARKER.to_string(),
            visible_body,
            outer_success,
            cell_id,
            nested_spine_calls,
        })
    }
}

pub(crate) fn encode_carrier(carrier: &CodeModeOutputCarrierV1) -> Result<String, String> {
    validate_carrier(carrier)?;
    serde_json::to_string(carrier)
        .map_err(|error| format!("failed to encode Code Mode Spine carrier: {error}"))
}

pub(crate) fn decode_marked_body(
    output_name: Option<&str>,
    body: &FunctionCallOutputBody,
) -> Result<Option<CodeModeOutputCarrierV1>, String> {
    if output_name != Some(CODE_MODE_SPINE_CARRIER_MARKER) {
        return Ok(None);
    }
    let FunctionCallOutputBody::Text(body) = body else {
        return Err("marked Code Mode Spine carrier must have a text body".to_string());
    };
    let carrier: CodeModeOutputCarrierV1 = serde_json::from_str(body)
        .map_err(|error| format!("malformed Code Mode Spine carrier: {error}"))?;
    validate_carrier(&carrier)?;
    Ok(Some(carrier))
}

pub(crate) fn marked_body_has_parser_transition(
    output_name: Option<&str>,
    body: &FunctionCallOutputBody,
) -> Result<bool, String> {
    Ok(
        decode_marked_body(output_name, body)?.is_some_and(|carrier| {
            carrier.nested_spine_calls.iter().any(|call| {
                matches!(
                    call.name,
                    NestedSpineToolName::Open
                        | NestedSpineToolName::Close
                        | NestedSpineToolName::Next
                )
            })
        }),
    )
}

fn validate_carrier(carrier: &CodeModeOutputCarrierV1) -> Result<(), String> {
    if carrier.schema != CODE_MODE_SPINE_CARRIER_MARKER {
        return Err(format!(
            "unsupported Code Mode Spine carrier schema `{}`",
            carrier.schema
        ));
    }
    if carrier.cell_id.is_empty() {
        return Err("Code Mode Spine cell id must not be empty".to_string());
    }
    validate_nested_calls(&carrier.nested_spine_calls)
}

fn validate_nested_calls(calls: &[NestedSpineCallV1]) -> Result<(), String> {
    let mut previous_ordinal = None;
    let mut exclusive = None;
    for call in calls {
        if call.runtime_call_id.is_empty() {
            return Err("Code Mode Spine runtime call id must not be empty".to_string());
        }
        if let Some(previous) = previous_ordinal
            && call.invocation_ordinal <= previous
        {
            return Err(
                "Code Mode Spine invocation ordinals must be strictly increasing".to_string(),
            );
        }
        previous_ordinal = Some(call.invocation_ordinal);

        let Some(kind) = call.name.exclusive_kind() else {
            continue;
        };
        if exclusive.replace(kind).is_some() {
            return Err(
                "Code Mode Spine carrier permits one parser control or one spawn".to_string(),
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExclusiveKind {
    ParserControl,
    Spawn,
}

#[derive(Debug)]
struct NestedSpineRequestV1 {
    runtime_call_id: String,
    invocation_ordinal: u64,
    name: NestedSpineToolName,
    arguments: String,
}

#[derive(Debug)]
enum NestedSpineSlot {
    InFlight(NestedSpineRequestV1),
    Completed(NestedSpineCallV1),
    Aborted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum FirstOutputPhase {
    #[default]
    Accepting,
    Sealed,
    Taken,
}

#[derive(Debug, Default)]
struct CellSpineInner {
    outer_exec_call_id: Option<String>,
    admission_enabled: bool,
    first_output: FirstOutputPhase,
    runtime_closed: bool,
    next_ordinal: u64,
    exclusive: Option<ExclusiveKind>,
    slots: Vec<NestedSpineSlot>,
}

#[derive(Debug, Default)]
pub(crate) struct CellSpineState {
    inner: Mutex<CellSpineInner>,
    changed: Notify,
}

pub(crate) struct CellFirstOutputJoin {
    state: Arc<CellSpineState>,
}

impl CellFirstOutputJoin {
    pub(crate) async fn finish(self) -> Result<Vec<NestedSpineCallV1>, String> {
        self.state.finish_first_output().await
    }
}

impl CellSpineState {
    pub(crate) fn register_outer_exec(
        &self,
        call_id: &str,
        admission_enabled: bool,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        match &inner.outer_exec_call_id {
            Some(existing)
                if existing != call_id || inner.admission_enabled != admission_enabled =>
            {
                Err(format!(
                    "Code Mode cell is already registered to outer exec `{existing}`"
                ))
            }
            Some(_) => Ok(()),
            None => {
                inner.outer_exec_call_id = Some(call_id.to_string());
                inner.admission_enabled = admission_enabled;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn outer_exec_call_id(&self) -> Option<String> {
        self.lock().outer_exec_call_id.clone()
    }

    pub(crate) fn admit(
        self: &Arc<Self>,
        runtime_call_id: String,
        name: NestedSpineToolName,
        arguments: String,
    ) -> Result<NestedSpineAdmission, String> {
        let mut inner = self.lock();
        if inner.first_output != FirstOutputPhase::Accepting {
            return Err("Code Mode cell is sealed against nested Spine calls".to_string());
        }
        if !inner.admission_enabled {
            return Err("Code Mode nested Spine calls require a sole outer exec call".to_string());
        }
        if runtime_call_id.is_empty() {
            return Err("Code Mode Spine runtime call id must not be empty".to_string());
        }
        let outer_exec_call_id = inner
            .outer_exec_call_id
            .clone()
            .ok_or_else(|| "Code Mode cell is missing its outer exec identity".to_string())?;

        let invocation_ordinal = inner.next_ordinal;
        let next_ordinal = inner
            .next_ordinal
            .checked_add(1)
            .ok_or_else(|| "Code Mode Spine invocation ordinal overflow".to_string())?;

        if let Some(kind) = name.exclusive_kind() {
            if inner.exclusive.is_some() {
                return Err("Code Mode cell permits one parser control or one spawn".to_string());
            }
            inner.exclusive = Some(kind);
        }

        inner.next_ordinal = next_ordinal;
        inner
            .slots
            .push(NestedSpineSlot::InFlight(NestedSpineRequestV1 {
                runtime_call_id,
                invocation_ordinal,
                name,
                arguments,
            }));
        Ok(NestedSpineAdmission {
            state: Arc::clone(self),
            invocation_ordinal,
            outer_exec_call_id,
            finished: false,
        })
    }

    pub(crate) fn begin_first_output(self: &Arc<Self>) -> Result<CellFirstOutputJoin, String> {
        let mut inner = self.lock();
        match inner.first_output {
            FirstOutputPhase::Accepting => {
                inner.first_output = FirstOutputPhase::Sealed;
                Ok(CellFirstOutputJoin {
                    state: Arc::clone(self),
                })
            }
            FirstOutputPhase::Sealed | FirstOutputPhase::Taken => {
                Err("Code Mode cell first output was already sealed".to_string())
            }
        }
    }

    async fn finish_first_output(&self) -> Result<Vec<NestedSpineCallV1>, String> {
        loop {
            let notified = self.changed.notified();
            {
                let mut inner = self.lock();
                match inner.first_output {
                    FirstOutputPhase::Accepting => {
                        return Err("Code Mode cell first output was not begun".to_string());
                    }
                    FirstOutputPhase::Taken => {
                        return Err("Code Mode cell first output was already sealed".to_string());
                    }
                    FirstOutputPhase::Sealed => {}
                }
                if !inner
                    .slots
                    .iter()
                    .any(|slot| matches!(slot, NestedSpineSlot::InFlight(_)))
                {
                    let calls = inner
                        .slots
                        .iter()
                        .filter_map(|slot| match slot {
                            NestedSpineSlot::Completed(call) => Some(call.clone()),
                            NestedSpineSlot::Aborted => None,
                            NestedSpineSlot::InFlight(_) => unreachable!(),
                        })
                        .collect::<Vec<_>>();
                    validate_nested_calls(&calls)?;
                    inner.first_output = FirstOutputPhase::Taken;
                    inner.slots.clear();
                    return Ok(calls);
                }
            }
            notified.await;
        }
    }

    pub(crate) fn mark_runtime_closed(&self) {
        self.lock().runtime_closed = true;
    }

    pub(crate) fn lifecycle_complete(&self) -> bool {
        let inner = self.lock();
        inner.runtime_closed
            && inner.outer_exec_call_id.is_some()
            && (inner.first_output == FirstOutputPhase::Taken || !inner.admission_enabled)
    }

    pub(crate) fn is_waiting_for_first_output(&self, outer_exec_call_id: &str) -> bool {
        let inner = self.lock();
        inner.outer_exec_call_id.as_deref() == Some(outer_exec_call_id)
            && inner.first_output == FirstOutputPhase::Sealed
            && inner
                .slots
                .iter()
                .any(|slot| matches!(slot, NestedSpineSlot::InFlight(_)))
    }

    fn complete(&self, invocation_ordinal: u64, output: NestedSpineOutputV1) -> Result<(), String> {
        let mut inner = self.lock();
        let slot = inner
            .slots
            .iter_mut()
            .find(|slot| {
                matches!(
                    slot,
                    NestedSpineSlot::InFlight(request)
                        if request.invocation_ordinal == invocation_ordinal
                )
            })
            .ok_or_else(|| "Code Mode Spine admission is no longer in flight".to_string())?;
        let NestedSpineSlot::InFlight(request) = slot else {
            unreachable!();
        };
        let call = NestedSpineCallV1 {
            runtime_call_id: std::mem::take(&mut request.runtime_call_id),
            invocation_ordinal: request.invocation_ordinal,
            name: request.name,
            arguments: std::mem::take(&mut request.arguments),
            output,
        };
        *slot = NestedSpineSlot::Completed(call);
        drop(inner);
        self.changed.notify_one();
        Ok(())
    }

    fn abort(&self, invocation_ordinal: u64) {
        let mut inner = self.lock();
        let Some(slot) = inner.slots.iter_mut().find(|slot| {
            matches!(
                slot,
                NestedSpineSlot::InFlight(request)
                    if request.invocation_ordinal == invocation_ordinal
            )
        }) else {
            return;
        };
        *slot = NestedSpineSlot::Aborted;
        drop(inner);
        self.changed.notify_one();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CellSpineInner> {
        match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

pub(crate) struct NestedSpineAdmission {
    state: Arc<CellSpineState>,
    invocation_ordinal: u64,
    outer_exec_call_id: String,
    finished: bool,
}

impl NestedSpineAdmission {
    pub(crate) fn invocation_ordinal(&self) -> u64 {
        self.invocation_ordinal
    }

    pub(crate) fn outer_exec_call_id(&self) -> &str {
        &self.outer_exec_call_id
    }

    pub(crate) fn complete(mut self, success: bool, body: String) -> Result<(), String> {
        self.state.complete(
            self.invocation_ordinal,
            NestedSpineOutputV1 { success, body },
        )?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for NestedSpineAdmission {
    fn drop(&mut self) {
        if !self.finished {
            self.state.abort(self.invocation_ordinal);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_protocol::models::FunctionCallOutputContentItem;
    use serde_json::json;

    use super::*;

    fn completed_call(ordinal: u64, name: NestedSpineToolName) -> NestedSpineCallV1 {
        NestedSpineCallV1 {
            runtime_call_id: format!("runtime-{ordinal}"),
            invocation_ordinal: ordinal,
            name,
            arguments: "{}".to_string(),
            output: NestedSpineOutputV1 {
                success: true,
                body: "ok".to_string(),
            },
        }
    }

    #[test]
    fn carrier_round_trips_text_and_structured_bodies() {
        for visible_body in [
            FunctionCallOutputBody::Text("plain".to_string()),
            FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "structured".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AA==".to_string(),
                    detail: None,
                },
            ]),
        ] {
            let carrier = CodeModeOutputCarrierV1::new(
                visible_body,
                Some(true),
                "cell-1".to_string(),
                vec![completed_call(0, NestedSpineToolName::Trim)],
            )
            .expect("valid carrier");
            let body = FunctionCallOutputBody::Text(encode_carrier(&carrier).expect("encode"));
            let decoded = decode_marked_body(Some(CODE_MODE_SPINE_CARRIER_MARKER), &body)
                .expect("decode")
                .expect("marked carrier");
            assert_eq!(decoded, carrier);
        }
    }

    #[test]
    fn carrier_marker_prevents_spoofing_and_malformed_marked_data_fails() {
        let body = FunctionCallOutputBody::Text(
            json!({
                "schema": CODE_MODE_SPINE_CARRIER_MARKER,
                "visible_body": "spoof",
                "outer_success": true,
                "cell_id": "cell-1",
                "nested_spine_calls": []
            })
            .to_string(),
        );
        assert_eq!(decode_marked_body(None, &body).expect("unmarked"), None);
        assert!(decode_marked_body(Some(CODE_MODE_SPINE_CARRIER_MARKER), &body).is_ok());

        let unknown_field = FunctionCallOutputBody::Text(
            json!({
                "schema": CODE_MODE_SPINE_CARRIER_MARKER,
                "visible_body": "bad",
                "outer_success": true,
                "cell_id": "cell-1",
                "nested_spine_calls": [],
                "extra": true
            })
            .to_string(),
        );
        assert!(decode_marked_body(Some(CODE_MODE_SPINE_CARRIER_MARKER), &unknown_field).is_err());
        let missing_outer_success = FunctionCallOutputBody::Text(
            json!({
                "schema": CODE_MODE_SPINE_CARRIER_MARKER,
                "visible_body": "bad",
                "cell_id": "cell-1",
                "nested_spine_calls": []
            })
            .to_string(),
        );
        assert!(
            decode_marked_body(Some(CODE_MODE_SPINE_CARRIER_MARKER), &missing_outer_success,)
                .is_err()
        );
        assert!(
            decode_marked_body(
                Some(CODE_MODE_SPINE_CARRIER_MARKER),
                &FunctionCallOutputBody::ContentItems(Vec::new()),
            )
            .is_err()
        );
    }

    #[test]
    fn parser_transition_detection_uses_strict_marked_carriers() {
        for (tool, expected) in [
            (NestedSpineToolName::Open, true),
            (NestedSpineToolName::Close, true),
            (NestedSpineToolName::Next, true),
            (NestedSpineToolName::Trim, false),
            (NestedSpineToolName::Spawn, false),
        ] {
            let carrier = CodeModeOutputCarrierV1::new(
                FunctionCallOutputBody::Text("visible".to_string()),
                Some(true),
                "cell".to_string(),
                vec![completed_call(0, tool)],
            )
            .expect("valid carrier");
            let body = FunctionCallOutputBody::Text(encode_carrier(&carrier).expect("encode"));
            assert_eq!(
                marked_body_has_parser_transition(Some(CODE_MODE_SPINE_CARRIER_MARKER), &body),
                Ok(expected)
            );
        }

        let empty = CodeModeOutputCarrierV1::new(
            FunctionCallOutputBody::Text("visible".to_string()),
            Some(true),
            "cell".to_string(),
            Vec::new(),
        )
        .expect("valid carrier");
        let body = FunctionCallOutputBody::Text(encode_carrier(&empty).expect("encode"));
        assert_eq!(marked_body_has_parser_transition(None, &body), Ok(false));
        assert!(
            marked_body_has_parser_transition(
                Some(CODE_MODE_SPINE_CARRIER_MARKER),
                &FunctionCallOutputBody::Text("malformed".to_string()),
            )
            .is_err()
        );
    }

    #[test]
    fn carrier_rejects_invalid_order_and_exclusive_combinations() {
        assert!(
            CodeModeOutputCarrierV1::new(
                FunctionCallOutputBody::Text("body".to_string()),
                Some(true),
                "cell".to_string(),
                vec![
                    completed_call(1, NestedSpineToolName::Trim),
                    completed_call(1, NestedSpineToolName::Open),
                ],
            )
            .is_err()
        );
        assert!(
            CodeModeOutputCarrierV1::new(
                FunctionCallOutputBody::Text("body".to_string()),
                Some(true),
                "cell".to_string(),
                vec![
                    completed_call(0, NestedSpineToolName::Open),
                    completed_call(1, NestedSpineToolName::Spawn),
                ],
            )
            .is_err()
        );
        assert!(
            CodeModeOutputCarrierV1::new(
                FunctionCallOutputBody::Text("body".to_string()),
                Some(true),
                "cell".to_string(),
                vec![
                    completed_call(0, NestedSpineToolName::Spawn),
                    completed_call(1, NestedSpineToolName::Spawn),
                ],
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn seal_waits_for_in_flight_calls_and_preserves_admission_order() {
        let state = Arc::new(CellSpineState::default());
        state.register_outer_exec("exec-1", true).expect("register");
        let first = state
            .admit(
                "runtime-1".to_string(),
                NestedSpineToolName::Trim,
                r#"{"TRIM_ID":"trim_1","op":"snip"}"#.to_string(),
            )
            .expect("first admission");
        let second = state
            .admit(
                "runtime-2".to_string(),
                NestedSpineToolName::Open,
                r#"{"goal":"child"}"#.to_string(),
            )
            .expect("second admission");

        let seal = state.begin_first_output().expect("begin first output");
        let seal = tokio::spawn(async move { seal.finish().await });
        tokio::task::yield_now().await;
        second.complete(true, "open".to_string()).expect("complete");
        assert!(!seal.is_finished());
        first.complete(true, "trim".to_string()).expect("complete");

        let calls = seal.await.expect("join").expect("seal");
        assert_eq!(
            calls
                .iter()
                .map(|call| call.invocation_ordinal)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[tokio::test]
    async fn dropped_admission_unblocks_seal_and_sealed_cells_reject_calls() {
        let state = Arc::new(CellSpineState::default());
        state.register_outer_exec("exec-1", true).expect("register");
        let admission = state
            .admit(
                "runtime-1".to_string(),
                NestedSpineToolName::Trim,
                "{}".to_string(),
            )
            .expect("admission");
        let seal = state.begin_first_output().expect("begin first output");
        let seal = tokio::spawn(async move { seal.finish().await });
        tokio::task::yield_now().await;
        drop(admission);
        assert!(seal.await.expect("join").expect("seal").is_empty());
        assert!(
            state
                .admit(
                    "runtime-2".to_string(),
                    NestedSpineToolName::Trim,
                    "{}".to_string(),
                )
                .is_err()
        );
    }

    #[test]
    fn admission_enforces_control_spawn_algebra_but_allows_many_trims() {
        let state = Arc::new(CellSpineState::default());
        state.register_outer_exec("exec-1", true).expect("register");
        let trim_a = state
            .admit(
                "trim-a".to_string(),
                NestedSpineToolName::Trim,
                "{}".to_string(),
            )
            .expect("trim a");
        let trim_b = state
            .admit(
                "trim-b".to_string(),
                NestedSpineToolName::Trim,
                "{}".to_string(),
            )
            .expect("trim b");
        let control = state
            .admit(
                "open".to_string(),
                NestedSpineToolName::Open,
                "{}".to_string(),
            )
            .expect("control");
        assert!(
            state
                .admit(
                    "spawn".to_string(),
                    NestedSpineToolName::Spawn,
                    "{}".to_string(),
                )
                .is_err()
        );
        assert!(
            state
                .admit(
                    "close".to_string(),
                    NestedSpineToolName::Close,
                    "{}".to_string(),
                )
                .is_err()
        );
        drop((trim_a, trim_b, control));
    }

    #[test]
    fn outer_exec_registration_is_idempotent_and_mismatch_fails_closed() {
        let state = CellSpineState::default();
        state
            .register_outer_exec("exec-1", true)
            .expect("first registration");
        state
            .register_outer_exec("exec-1", true)
            .expect("same registration");
        assert_eq!(state.outer_exec_call_id().as_deref(), Some("exec-1"));
        assert!(state.register_outer_exec("exec-2", true).is_err());
    }

    #[test]
    fn outer_exec_registration_can_disable_nested_spine_admission() {
        let state = Arc::new(CellSpineState::default());
        state
            .register_outer_exec("exec-1", false)
            .expect("register disabled bridge");
        assert!(
            state
                .admit(
                    "runtime-1".to_string(),
                    NestedSpineToolName::Spawn,
                    r#"{"tasks":[]}"#.to_string(),
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn lifecycle_completes_in_either_close_order() {
        let first = Arc::new(CellSpineState::default());
        first.register_outer_exec("exec-1", true).expect("register");
        first.mark_runtime_closed();
        assert!(!first.lifecycle_complete());
        first
            .begin_first_output()
            .expect("begin first output")
            .finish()
            .await
            .expect("finish first output");
        assert!(first.lifecycle_complete());

        let second = Arc::new(CellSpineState::default());
        second
            .register_outer_exec("exec-2", true)
            .expect("register");
        second
            .begin_first_output()
            .expect("begin first output")
            .finish()
            .await
            .expect("finish first output");
        assert!(!second.lifecycle_complete());
        second.mark_runtime_closed();
        assert!(second.lifecycle_complete());
    }

    #[test]
    fn first_output_join_is_single_use_and_seals_admission() {
        let state = Arc::new(CellSpineState::default());
        state.register_outer_exec("exec-1", true).expect("register");

        let join = state.begin_first_output().expect("begin first output");

        assert!(state.begin_first_output().is_err());
        assert!(
            state
                .admit(
                    "runtime-1".to_string(),
                    NestedSpineToolName::Trim,
                    "{}".to_string(),
                )
                .is_err()
        );
        drop(join);
    }

    #[test]
    fn disabled_bridge_lifecycle_completes_on_runtime_close_without_seal() {
        let state = Arc::new(CellSpineState::default());
        state
            .register_outer_exec("exec-1", false)
            .expect("register disabled bridge");
        assert!(!state.lifecycle_complete());
        state.mark_runtime_closed();
        assert!(state.lifecycle_complete());
    }
}

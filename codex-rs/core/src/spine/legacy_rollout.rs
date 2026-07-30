use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Deserializer;

pub(crate) const LEGACY_CARRIER_MARKER: &str = "spine.code_mode.output.v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(rename_all = "snake_case")]
pub(crate) enum LegacyToolName {
    Open,
    Close,
    Next,
    Trim,
    Spawn,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyOutput {
    pub(crate) success: bool,
    pub(crate) body: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyCall {
    pub(crate) runtime_call_id: String,
    pub(crate) invocation_ordinal: u64,
    pub(crate) name: LegacyToolName,
    pub(crate) arguments: String,
    pub(crate) output: LegacyOutput,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyCarrier {
    schema: String,
    pub(crate) visible_body: FunctionCallOutputBody,
    #[serde(deserialize_with = "deserialize_required_outer_success")]
    pub(crate) outer_success: Option<bool>,
    pub(crate) cell_id: String,
    pub(crate) nested_spine_calls: Vec<LegacyCall>,
}

fn deserialize_required_outer_success<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<bool>::deserialize(deserializer)
}

pub(crate) fn decode_marked_body(
    output_name: Option<&str>,
    body: &FunctionCallOutputBody,
) -> Result<Option<LegacyCarrier>, String> {
    if output_name != Some(LEGACY_CARRIER_MARKER) {
        return Ok(None);
    }
    let FunctionCallOutputBody::Text(body) = body else {
        return Err("marked Code Mode Spine carrier must have a text body".to_string());
    };
    let carrier: LegacyCarrier = serde_json::from_str(body)
        .map_err(|error| format!("malformed Code Mode Spine carrier: {error}"))?;
    validate_carrier(&carrier)?;
    Ok(Some(carrier))
}

pub(crate) fn expand_response_item(item: &ResponseItem) -> Result<Vec<ResponseItem>, String> {
    let ResponseItem::CustomToolCallOutput {
        id,
        call_id,
        name,
        output,
        internal_chat_message_metadata_passthrough,
    } = item
    else {
        return Ok(vec![item.clone()]);
    };
    let Some(carrier) = decode_marked_body(name.as_deref(), &output.body)? else {
        return Ok(vec![item.clone()]);
    };

    let mut expanded = Vec::with_capacity(1 + carrier.nested_spine_calls.len() * 2);
    for call in &carrier.nested_spine_calls {
        let name = match call.name {
            LegacyToolName::Open => "open",
            LegacyToolName::Close => "close",
            LegacyToolName::Next => "next",
            LegacyToolName::Trim => "trim",
            LegacyToolName::Spawn => "spawn",
        };
        expanded.push(ResponseItem::FunctionCall {
            id: None,
            name: name.to_string(),
            namespace: Some("spine".to_string()),
            arguments: call.arguments.clone(),
            call_id: call.runtime_call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        });
    }
    for call in carrier.nested_spine_calls {
        expanded.push(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call.runtime_call_id,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(call.output.body),
                success: Some(call.output.success),
            },
            internal_chat_message_metadata_passthrough: None,
        });
    }
    expanded.push(ResponseItem::CustomToolCallOutput {
        id: id.clone(),
        call_id: call_id.clone(),
        name: None,
        output: FunctionCallOutputPayload {
            body: carrier.visible_body,
            success: carrier.outer_success,
        },
        internal_chat_message_metadata_passthrough: internal_chat_message_metadata_passthrough
            .clone(),
    });
    Ok(expanded)
}

pub(crate) fn expand_response_items(items: &[ResponseItem]) -> Vec<ResponseItem> {
    items
        .iter()
        .flat_map(|item| expand_response_item(item).unwrap_or_else(|_| vec![item.clone()]))
        .collect()
}

fn validate_carrier(carrier: &LegacyCarrier) -> Result<(), String> {
    if carrier.schema != LEGACY_CARRIER_MARKER {
        return Err(format!(
            "unsupported Code Mode Spine carrier schema `{}`",
            carrier.schema
        ));
    }
    if carrier.cell_id.is_empty() {
        return Err("Code Mode Spine cell id must not be empty".to_string());
    }
    validate_legacy_calls(&carrier.nested_spine_calls)
}

fn validate_legacy_calls(calls: &[LegacyCall]) -> Result<(), String> {
    if calls.iter().any(|call| call.runtime_call_id.is_empty()) {
        return Err("Code Mode Spine runtime call id must not be empty".to_string());
    }
    if calls
        .windows(2)
        .any(|pair| pair[1].invocation_ordinal <= pair[0].invocation_ordinal)
    {
        return Err("Code Mode Spine invocation ordinals must be strictly increasing".to_string());
    }
    if calls
        .iter()
        .filter(|call| call.name != LegacyToolName::Trim)
        .nth(1)
        .is_some()
    {
        return Err("Code Mode Spine carrier permits one parser control or one spawn".to_string());
    }
    Ok(())
}

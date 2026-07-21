use crate::context_manager::ContextManager;
use crate::context_manager::is_user_turn_boundary;
use crate::event_mapping::is_contextual_dev_message_content;
use crate::event_mapping::is_contextual_user_message_content;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use spine_core::ContextItem;
#[cfg(test)]
use spine_core::Feature;
use spine_core::MemorySlot;
use spine_core::Message;
use spine_core::MessageRole;
use spine_core::NativeItemRef;
use spine_core::NodeStatus;
use spine_core::RawBoundary;
use spine_core::RolloutEvent;
use spine_core::SpawnReceipt;
#[cfg(test)]
use spine_core::SpineCompiler;
#[cfg(test)]
use spine_core::SpineConfig;
use spine_core::SpineProjection;
use spine_core::ToolCallGroup;
use spine_core::ToolOutcome;
use spine_core::ToolUse;
use spine_core::TrimEdit;
use spine_core::TrimProjection;
use spine_core::TrimRequest;

pub(crate) mod host;
pub(crate) mod memory_projection;
pub(crate) mod pressure;
pub(crate) mod rollout_debug;
pub(crate) mod spawn;
pub(crate) mod spawn_salvage;
pub(crate) mod status;
pub(crate) mod tool_response;

pub(crate) const TOOL_RESULT_CLEARED_MESSAGE: &str = spine_core::TRIM_SNIPPED_BODY;

#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CodexSpineProjection {
    pub(crate) spine: SpineProjection,
    pub(crate) context: Vec<ResponseItem>,
}

pub(crate) fn closed_memory_projection_entries(
    projection: &SpineProjection,
) -> Vec<memory_projection::SpinetreeMemoryProjectionEntry> {
    spine_core::closed_memory_artifacts(projection)
        .into_iter()
        .map(
            |artifact| memory_projection::SpinetreeMemoryProjectionEntry {
                summary: artifact.summary,
                body: spine_core::render_memory_artifact(&artifact.node_id, &artifact.body),
                node_id: artifact.node_id.to_string(),
            },
        )
        .collect()
}

pub(crate) fn user_message_projection_entries(
    rollout: &[RolloutItem],
) -> Vec<memory_projection::SpinetreeUserMessageProjectionEntry> {
    let mut next_anchor = 1;
    effective_rollout(rollout)
        .into_iter()
        .filter_map(|(raw_index, item)| {
            let RolloutItem::ResponseItem(item) = item else {
                return None;
            };
            let message = message_from_response_item(raw_index, item);
            if message.role != MessageRole::User {
                return None;
            }
            let entry = memory_projection::SpinetreeUserMessageProjectionEntry {
                anchor: next_anchor,
                body: message.content,
            };
            next_anchor += 1;
            Some(entry)
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn derive_from_rollout(rollout: &[RolloutItem]) -> CodexSpineProjection {
    derive_from_rollout_with_features(rollout, true, false, true)
}

#[cfg(test)]
pub(crate) fn derive_from_rollout_with_features(
    rollout: &[RolloutItem],
    jit_enabled: bool,
    trim_enabled: bool,
    spawn_enabled: bool,
) -> CodexSpineProjection {
    let effective = effective_rollout(rollout);
    projection_from_effective_rollout(&effective, jit_enabled, trim_enabled, spawn_enabled, None)
}

#[cfg(test)]
fn projection_from_effective_rollout(
    effective: &[(usize, &RolloutItem)],
    jit_enabled: bool,
    trim_enabled: bool,
    spawn_enabled: bool,
    host_history: Option<&ContextManager>,
) -> CodexSpineProjection {
    let events = lex_rollout(effective, spawn_enabled);
    let trim = trim_enabled.then(|| TrimProjection::derive(&events));
    let spine = derive_spine_projection(jit_enabled.then_some(events.as_slice()).unwrap_or(&[]));
    let context = if jit_enabled {
        materialize_context(
            &spine.visible_context,
            effective,
            trim.as_ref(),
            host_history,
            spawn_enabled,
        )
        .expect("derived Spine context must resolve against the same rollout")
    } else {
        materialize_trim_only_context(effective, trim.as_ref(), host_history)
            .expect("trim projection must resolve against the same rollout")
    };
    CodexSpineProjection { spine, context }
}

#[cfg(test)]
fn derive_spine_projection(events: &[RolloutEvent]) -> SpineProjection {
    let config = SpineConfig::v1()
        .with_feature(Feature::Jit)
        .expect("JIT configuration is valid");
    let mut compiler = SpineCompiler::new(config).expect("the built-in Spine config is valid");
    compiler
        .replay(events.iter().cloned())
        .expect("Codex adapter emits monotonic event boundaries")
        .projection
}

pub(crate) fn validate_trim_request(
    rollout: &[RolloutItem],
    current_call_id: &str,
    request: &TrimRequest,
) -> Result<(), String> {
    let effective = effective_rollout(rollout);
    let events = lex_rollout(&effective, true);
    // The current trim request is already staged in the rollout, but has no
    // response yet. Its predecessor is the only eligible trim window.
    let current_group = events.iter().rposition(|event| {
        let RolloutEvent::ToolCall(group) = event else {
            return false;
        };
        group
            .calls
            .iter()
            .any(|call| call.call_id == current_call_id && call.name == "spine.trim")
    });
    let Some(current_group) = current_group else {
        return Err("spine.trim failed: current toolcall is unavailable; do not retry".to_string());
    };
    let events_before_current = &events[..current_group];
    let previous_completed_group = events_before_current
        .iter()
        .rev()
        .find(|event| matches!(event, RolloutEvent::ToolCall(group) if group.is_complete()));
    let projection = previous_completed_group
        .map(|event| TrimProjection::derive(std::slice::from_ref(event)))
        .unwrap_or_default();
    projection.validate(request)
}

fn effective_rollout(rollout: &[RolloutItem]) -> Vec<(usize, &RolloutItem)> {
    let mut effective: Vec<(usize, &RolloutItem)> = Vec::new();
    let mut response_ordinal = 0;
    for item in rollout {
        if let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) = item {
            let turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
            if turns == 0 {
                continue;
            }
            let user_boundaries: Vec<_> = effective
                .iter()
                .enumerate()
                .filter_map(|(effective_index, (_, item))| match item {
                    RolloutItem::ResponseItem(item) if is_user_turn_boundary(item) => {
                        Some(effective_index)
                    }
                    RolloutItem::InterAgentCommunication(_) => Some(effective_index),
                    _ => None,
                })
                .collect();
            if let Some(cut) = user_boundaries
                .len()
                .checked_sub(turns)
                .and_then(|position| user_boundaries.get(position))
                .copied()
                .or_else(|| user_boundaries.first().copied())
            {
                let first_user_boundary = user_boundaries.first().copied().unwrap_or(cut);
                effective.truncate(cut);
                // Native rollback trims contextual updates immediately above the removed
                // user-turn boundary. Keep the Spine selected prefix identical to that host
                // boundary so projection cannot reintroduce settings that rollback removed.
                let mut scan = effective.len();
                while scan > first_user_boundary {
                    let Some((_, item)) = effective.get(scan - 1) else {
                        break;
                    };
                    let trim = match item {
                        RolloutItem::ResponseItem(ResponseItem::Message {
                            role, content, ..
                        }) if role == "developer" => is_contextual_dev_message_content(content),
                        RolloutItem::ResponseItem(ResponseItem::Message {
                            role, content, ..
                        }) if role == "user" => is_contextual_user_message_content(content),
                        RolloutItem::EventMsg(EventMsg::TokenCount(_)) => {
                            scan -= 1;
                            continue;
                        }
                        _ => false,
                    };
                    if !trim {
                        break;
                    }
                    effective.remove(scan - 1);
                    scan -= 1;
                }
            }
            continue;
        }
        if is_spine_source_item(item) {
            effective.push((response_ordinal, item));
            response_ordinal += 1;
        } else if matches!(item, RolloutItem::EventMsg(EventMsg::TokenCount(_))) {
            effective.push((response_ordinal, item));
        }
    }
    effective
}

pub(crate) fn is_spine_source_item(item: &RolloutItem) -> bool {
    matches!(
        item,
        RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::Compacted(_)
    )
}

fn lex_rollout(effective: &[(usize, &RolloutItem)], spawn_enabled: bool) -> Vec<RolloutEvent> {
    let mut events = Vec::new();
    let mut index = 0;
    while index < effective.len() {
        let (raw_index, item) = effective[index];
        match item {
            RolloutItem::ResponseItem(response_item) => {
                if let Some((group, consumed)) =
                    completed_tool_group(effective, index, spawn_enabled)
                {
                    events.push(RolloutEvent::ToolCall(group));
                    index += consumed;
                    continue;
                }
                events.push(RolloutEvent::Message(message_from_response_item(
                    raw_index,
                    response_item,
                )));
            }
            RolloutItem::InterAgentCommunication(communication) => {
                events.push(RolloutEvent::Message(message_from_response_item(
                    raw_index,
                    &communication.to_model_input_item(),
                )));
            }
            RolloutItem::Compacted(compacted) => {
                let replacement_history = compacted
                    .replacement_history
                    .as_ref()
                    .map(|items| {
                        items
                            .iter()
                            .enumerate()
                            .map(|(replacement_index, _)| ContextItem::Native {
                                source: NativeItemRef::CompactReplacement {
                                    compact_boundary: RawBoundary(raw_index as u64),
                                    index: u32::try_from(replacement_index).unwrap_or(u32::MAX),
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_else(|| {
                        vec![ContextItem::Message {
                            message: Message {
                                boundary: RawBoundary(raw_index as u64),
                                role: MessageRole::Assistant,
                                content: compacted.message.clone(),
                            },
                            user_anchor: None,
                        }]
                    });
                events.push(RolloutEvent::Compact {
                    boundary: RawBoundary(raw_index as u64),
                    replacement_history,
                });
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::EventMsg(_) => {}
        }
        index += 1;
    }
    events
}

struct StableLex {
    events: Vec<RolloutEvent>,
    pending: Vec<NativeItemRef>,
}

fn stable_lex_rollout(effective: &[(usize, &RolloutItem)], spawn_enabled: bool) -> StableLex {
    let mut events = Vec::new();
    let mut index = 0;
    while index < effective.len() {
        let (raw_index, item) = effective[index];
        match item {
            RolloutItem::ResponseItem(response_item) => {
                if let Some((group, consumed)) =
                    completed_tool_group(effective, index, spawn_enabled)
                {
                    if !group.is_complete() {
                        let next = index + consumed;
                        if next == effective.len() {
                            break;
                        }
                        for (raw_index, item) in &effective[index..next] {
                            if let RolloutItem::ResponseItem(item) = item {
                                events.push(RolloutEvent::Message(message_from_response_item(
                                    *raw_index, item,
                                )));
                            }
                        }
                        index = next;
                        continue;
                    }
                    events.push(RolloutEvent::ToolCall(group));
                    index += consumed;
                    continue;
                }
                if is_leading_assistant_item(response_item)
                    && effective[index..]
                        .iter()
                        .all(|(_, item)| matches!(item, RolloutItem::ResponseItem(item) if is_leading_assistant_item(item)))
                {
                    break;
                }
                events.push(RolloutEvent::Message(message_from_response_item(
                    raw_index,
                    response_item,
                )));
            }
            RolloutItem::InterAgentCommunication(communication) => {
                events.push(RolloutEvent::Message(message_from_response_item(
                    raw_index,
                    &communication.to_model_input_item(),
                )));
            }
            RolloutItem::Compacted(compacted) => {
                let replacement_history = compacted
                    .replacement_history
                    .as_ref()
                    .map(|items| {
                        items
                            .iter()
                            .enumerate()
                            .map(|(replacement_index, _)| ContextItem::Native {
                                source: NativeItemRef::CompactReplacement {
                                    compact_boundary: RawBoundary(raw_index as u64),
                                    index: u32::try_from(replacement_index).unwrap_or(u32::MAX),
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_else(|| {
                        vec![ContextItem::Message {
                            message: Message {
                                boundary: RawBoundary(raw_index as u64),
                                role: MessageRole::Assistant,
                                content: compacted.message.clone(),
                            },
                            user_anchor: None,
                        }]
                    });
                events.push(RolloutEvent::Compact {
                    boundary: RawBoundary(raw_index as u64),
                    replacement_history,
                });
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::EventMsg(_) => {}
        }
        index += 1;
    }
    let pending = effective[index..]
        .iter()
        .filter_map(|(ordinal, item)| {
            is_spine_source_item(item).then_some(NativeItemRef::Rollout {
                ordinal: RawBoundary(*ordinal as u64),
            })
        })
        .collect();
    StableLex { events, pending }
}

fn completed_tool_group(
    effective: &[(usize, &RolloutItem)],
    start: usize,
    spawn_enabled: bool,
) -> Option<(ToolCallGroup, usize)> {
    let mut cursor = start;
    let mut leading = Vec::new();
    while let Some((raw_index, RolloutItem::ResponseItem(item))) = effective.get(cursor).copied() {
        if !is_leading_assistant_item(item) {
            break;
        }
        leading.push(message_from_response_item(raw_index, item));
        cursor += 1;
    }

    let first_call = cursor;
    let mut calls = Vec::new();
    while let Some((_, RolloutItem::ResponseItem(item))) = effective.get(cursor).copied() {
        let Some(call) = normalized_tool_request(item) else {
            break;
        };
        calls.push(call);
        cursor += 1;
    }
    if cursor == first_call {
        return None;
    }

    let mut last_group_index = cursor.saturating_sub(1);
    while let Some((raw_index, RolloutItem::ResponseItem(item))) = effective.get(cursor).copied() {
        let Some((call_id, output)) = normalized_tool_response(item) else {
            break;
        };
        let Some(call) = calls.iter_mut().find(|call| call.call_id == call_id) else {
            break;
        };
        call.outcome = Some(classify_tool_outcome(call, output, spawn_enabled));
        call.output = Some(output.body.to_text().unwrap_or_default());
        call.output_boundary = Some(RawBoundary(raw_index as u64));
        last_group_index = cursor;
        cursor += 1;
    }

    let raw_start = effective[start].0;
    let raw_end = effective[last_group_index].0;
    Some((
        ToolCallGroup {
            start: RawBoundary(raw_start as u64),
            end: RawBoundary(raw_end as u64),
            leading_assistant_messages: leading,
            calls,
        },
        last_group_index - start + 1,
    ))
}

// Carrier differences end at the rollout adapter; reducers consume one toolcall model.
fn normalized_tool_request(item: &ResponseItem) -> Option<ToolUse> {
    let (name, namespace, arguments, call_id) = match item {
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            call_id,
            ..
        } => (name, namespace.as_deref(), arguments, call_id),
        ResponseItem::CustomToolCall {
            name,
            namespace,
            input,
            call_id,
            ..
        } => (name, namespace.as_deref(), input, call_id),
        _ => return None,
    };
    Some(ToolUse {
        call_id: call_id.clone(),
        name: qualified_tool_name(namespace, name),
        arguments: arguments.clone(),
        outcome: None,
        output: None,
        output_boundary: None,
    })
}

fn normalized_tool_response(
    item: &ResponseItem,
) -> Option<(&str, &codex_protocol::models::FunctionCallOutputPayload)> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => Some((call_id, output)),
        _ => None,
    }
}

fn classify_tool_outcome(
    call: &ToolUse,
    output: &codex_protocol::models::FunctionCallOutputPayload,
    spawn_enabled: bool,
) -> ToolOutcome {
    if call.name == "spine.spawn" {
        if output.success == Some(false) {
            return ToolOutcome::Failed;
        }
        return if spawn_enabled && is_valid_spawn_success_carrier(call, &output.body) {
            ToolOutcome::Succeeded
        } else {
            ToolOutcome::Unknown
        };
    }
    tool_response::SpineToolResponse::outcome(&call.name, output)
}

fn is_valid_spawn_success_carrier(call: &ToolUse, body: &FunctionCallOutputBody) -> bool {
    let FunctionCallOutputBody::Text(body) = body else {
        return false;
    };
    let Ok(tasks) = spawn::parse_tasks(&call.arguments) else {
        return false;
    };
    let Ok(receipt) = SpawnReceipt::decode_json(body) else {
        return false;
    };
    receipt.validate_for(&tasks).is_ok()
}

fn is_leading_assistant_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Message { role, .. } if role == "assistant"
    ) || matches!(item, ResponseItem::Reasoning { .. })
}

fn qualified_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}.{name}"),
        _ => name.to_string(),
    }
}

fn message_from_response_item(raw_index: usize, item: &ResponseItem) -> Message {
    let (role, content) = match item {
        ResponseItem::Message { role, content, .. } => (
            match role.as_str() {
                "user" if is_contextual_user_message_content(content) => {
                    MessageRole::ContextualUser
                }
                "user" => MessageRole::User,
                "developer" => MessageRole::Developer,
                "system" => MessageRole::System,
                _ => MessageRole::Assistant,
            },
            content
                .iter()
                .filter_map(content_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => (
            MessageRole::Assistant,
            serde_json::to_string(item).unwrap_or_default(),
        ),
    };
    Message {
        boundary: RawBoundary(raw_index as u64),
        role,
        content,
    }
}

fn content_text(item: &ContentItem) -> Option<String> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text.clone()),
        ContentItem::InputImage { .. } => Some("<image>".to_string()),
    }
}

fn materialize_context(
    context: &[ContextItem],
    source: &[(usize, &RolloutItem)],
    trim: Option<&TrimProjection>,
    host_history: Option<&ContextManager>,
    spawn_enabled: bool,
) -> Result<Vec<ResponseItem>, String> {
    let mut materialized = Vec::new();
    for item in context {
        match item {
            ContextItem::Message {
                message,
                user_anchor,
            } => {
                let mut item = response_item_at(source, message.boundary, host_history)
                    .ok_or_else(|| {
                        format!(
                            "message at raw boundary {} has no native rollout source",
                            message.boundary.0
                        )
                    })?;
                if let Some(anchor) = user_anchor {
                    tag_user_message(&mut item, *anchor);
                }
                materialized.push(item);
            }
            ContextItem::ToolCall(group) => {
                for raw_index in group.start.0..=group.end.0 {
                    if let Some(item) =
                        response_item_at(source, RawBoundary(raw_index), host_history)
                    {
                        materialized.push(project_toolcall_item(
                            item,
                            group,
                            usize::try_from(raw_index).unwrap_or(usize::MAX),
                            trim,
                            spawn_enabled,
                        ));
                    }
                }
            }
            ContextItem::SyntheticNode {
                node_id,
                summary,
                status,
            } => materialized.push(text_message(
                MessageRole::Developer,
                format!(
                    "<spine_node id=\"{node_id}\" summary=\"{}\" status=\"{}\" />",
                    escape_attribute(summary),
                    status_name(*status),
                ),
            )),
            ContextItem::MemorySlot(slot) => match slot {
                MemorySlot::User {
                    message, anchor, ..
                } => {
                    // The reducer created this slot from the same immutable rollout.
                    let mut item = response_item_at(source, message.boundary, host_history)
                        .ok_or_else(|| {
                            format!(
                                "memory user slot at raw boundary {} has no native rollout source",
                                message.boundary.0
                            )
                        })?;
                    if !matches!(&item, ResponseItem::Message { role, .. } if role == "user") {
                        return Err(format!(
                            "memory user slot at raw boundary {} resolved to a non-user item",
                            message.boundary.0
                        ));
                    }
                    tag_user_message(&mut item, *anchor);
                    materialized.push(item);
                }
                MemorySlot::Summary {
                    owner_node, body, ..
                } => materialized.push(text_message(
                    MessageRole::ContextualUser,
                    format!("<spine_memory node_id=\"{owner_node}\">\n{body}\n</spine_memory>"),
                )),
                MemorySlot::SpawnEvidence {
                    owner_node,
                    task,
                    outcome,
                    diagnostic,
                    execution_ref,
                    ..
                } => materialized.push(text_message(
                    MessageRole::ContextualUser,
                    render_spawn_evidence(
                        owner_node,
                        task,
                        *outcome,
                        diagnostic.as_deref(),
                        execution_ref.as_deref(),
                    ),
                )),
            },
            ContextItem::Native { source: native_ref } => match native_ref {
                NativeItemRef::Rollout { ordinal } => {
                    let item =
                        response_item_at(source, *ordinal, host_history).ok_or_else(|| {
                            format!("native rollout source {} is unavailable", ordinal.0)
                        })?;
                    materialized.push(item);
                }
                NativeItemRef::CompactReplacement {
                    compact_boundary,
                    index,
                } => {
                    let item = compact_replacement_at(source, *compact_boundary, *index)
                        .ok_or_else(|| {
                            format!(
                                "compact replacement {}:{} is unavailable",
                                compact_boundary.0, index
                            )
                        })?;
                    materialized.push(item);
                }
            },
        }
    }
    Ok(materialized)
}

fn project_toolcall_item(
    mut item: ResponseItem,
    group: &ToolCallGroup,
    raw_ordinal: usize,
    trim: Option<&TrimProjection>,
    spawn_enabled: bool,
) -> ResponseItem {
    if spawn_enabled && group.is_complete() {
        if let ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } = &mut item
        {
            if let Some(call) = group
                .calls
                .iter()
                .find(|call| call.call_id == *call_id && call.name == "spine.spawn")
            {
                let conflicting = group.calls.iter().any(|call| {
                    matches!(
                        call.name.as_str(),
                        "spine.open" | "spine.close" | "spine.next"
                    )
                });
                let status = if !conflicting && call.outcome == Some(ToolOutcome::Succeeded) {
                    "success"
                } else {
                    "failure"
                };
                output.body =
                    FunctionCallOutputBody::Text(serde_json::json!({"status": status}).to_string());
                output.success = Some(status == "success");
                return item;
            }
        }
    }
    project_trim_item(item, raw_ordinal, trim)
}

fn materialize_trim_only_context(
    effective: &[(usize, &RolloutItem)],
    trim: Option<&TrimProjection>,
    host_history: Option<&ContextManager>,
) -> Result<Vec<ResponseItem>, String> {
    let start = effective
        .iter()
        .rposition(|(_, item)| matches!(item, RolloutItem::Compacted(_)))
        .unwrap_or(0);
    let mut context = Vec::new();
    for (raw_index, item) in effective.iter().skip(start) {
        match item {
            RolloutItem::ResponseItem(item) => {
                let item = host_history
                    .map(|history| history.canonical_projected_item(item))
                    .unwrap_or_else(|| item.clone());
                context.push(project_trim_item(item, *raw_index, trim))
            }
            RolloutItem::InterAgentCommunication(communication) => {
                context.push(communication.to_model_input_item())
            }
            RolloutItem::Compacted(compacted) => {
                if let Some(replacement) = &compacted.replacement_history {
                    context.extend(replacement.iter().map(|item| {
                        host_history
                            .map(|history| history.canonical_projected_item(item))
                            .unwrap_or_else(|| item.clone())
                    }));
                } else {
                    context.push(text_message(
                        MessageRole::Assistant,
                        compacted.message.clone(),
                    ));
                }
            }
            _ => {}
        }
    }
    if context.is_empty() && !effective.is_empty() {
        context.extend(
            effective
                .iter()
                .filter_map(|(_, item)| match item {
                    RolloutItem::ResponseItem(item) => Some(
                        host_history
                            .map(|history| history.canonical_projected_item(item))
                            .unwrap_or_else(|| item.clone()),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
    }
    Ok(context)
}

fn project_trim_item(
    mut item: ResponseItem,
    raw_ordinal: usize,
    trim: Option<&TrimProjection>,
) -> ResponseItem {
    let (call_id, body) = match &mut item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } => (call_id, &mut output.body),
        ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => (call_id, &mut output.body),
        _ => return item,
    };
    let Some(edit) =
        trim.and_then(|projection| projection.edit(RawBoundary(raw_ordinal as u64), call_id))
    else {
        return item;
    };
    let visible_body = match edit {
        TrimEdit::Tagged { trim_id, body, .. } => format!("[TRIM_ID: {trim_id}]\n{body}"),
        TrimEdit::Snipped => TOOL_RESULT_CLEARED_MESSAGE.to_string(),
        TrimEdit::Sliced(value) => value.clone(),
    };
    *body = FunctionCallOutputBody::Text(visible_body);
    item
}

fn response_item_at(
    source: &[(usize, &RolloutItem)],
    boundary: RawBoundary,
    host_history: Option<&ContextManager>,
) -> Option<ResponseItem> {
    let index = usize::try_from(boundary.0).ok()?;
    match source
        .iter()
        .filter(|(_, item)| is_spine_source_item(item))
        .find_map(|(ordinal, item)| (*ordinal == index).then_some(*item))?
    {
        RolloutItem::ResponseItem(item) => Some(
            host_history
                .map(|history| history.canonical_projected_item(item))
                .unwrap_or_else(|| item.clone()),
        ),
        RolloutItem::InterAgentCommunication(communication) => {
            Some(communication.to_model_input_item())
        }
        RolloutItem::Compacted(compacted) => Some(text_message(
            MessageRole::Assistant,
            compacted.message.clone(),
        )),
        _ => None,
    }
}

fn compact_replacement_at(
    source: &[(usize, &RolloutItem)],
    boundary: RawBoundary,
    replacement_index: u32,
) -> Option<ResponseItem> {
    let raw_index = usize::try_from(boundary.0).ok()?;
    let replacement_index = usize::try_from(replacement_index).ok()?;
    let RolloutItem::Compacted(compacted) = source
        .iter()
        .filter(|(_, item)| is_spine_source_item(item))
        .find_map(|(ordinal, item)| (*ordinal == raw_index).then_some(*item))?
    else {
        return None;
    };
    compacted
        .replacement_history
        .as_ref()?
        .get(replacement_index)
        .cloned()
}

fn tag_user_message(item: &mut ResponseItem, anchor: u64) {
    let ResponseItem::Message { role, content, .. } = item else {
        return;
    };
    if role != "user" {
        return;
    }
    let prefix = format!("[U{anchor}]\n");
    if let Some(ContentItem::InputText { text }) = content
        .iter_mut()
        .find(|item| matches!(item, ContentItem::InputText { .. }))
    {
        text.insert_str(0, &prefix);
    } else {
        content.insert(0, ContentItem::InputText { text: prefix });
    }
}

fn text_message(role: MessageRole, text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: match role {
            MessageRole::User => "user",
            MessageRole::ContextualUser => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Developer => "developer",
            MessageRole::System => "system",
        }
        .to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn render_spawn_evidence(
    owner_node: &spine_core::NodeId,
    task: &spine_core::SpawnTask,
    outcome: spine_core::SpawnOutcome,
    diagnostic: Option<&str>,
    execution_ref: Option<&str>,
) -> String {
    format!(
        "<spine_spawn_evidence node_id=\"{owner_node}\">\n{}\n</spine_spawn_evidence>",
        render_spawn_evidence_body(task, outcome, diagnostic, execution_ref)
    )
}

fn render_spawn_evidence_body(
    task: &spine_core::SpawnTask,
    outcome: spine_core::SpawnOutcome,
    diagnostic: Option<&str>,
    execution_ref: Option<&str>,
) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "summary": task.summary,
        "prompt": task.prompt,
        "outcome": outcome,
        "diagnostic": diagnostic,
        "execution_ref": execution_ref,
    }))
    .expect("spawn evidence fields serialize")
}

fn status_name(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Live => "live",
        NodeStatus::Opened => "opened",
        NodeStatus::Closed => "closed",
        NodeStatus::Compacted => "compacted",
    }
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests;

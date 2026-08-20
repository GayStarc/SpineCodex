use super::materialize_context;
use super::message_from_response_item;
use crate::context::validate_spine_model_item;
use crate::context_manager::ContextManager;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use spine_core::host::CellId;
use spine_core::host::ContextEvent;
use spine_core::host::ContextInsert;
use spine_core::host::ContextLabel;
use spine_core::host::ObservedOutput;
use spine_core::host::ParseCell;
use spine_core::host::ParseStack;
use spine_core::host::RawBoundary;
use spine_core::host::SourceObservation;
use spine_core::host::SpineChar;
use spine_core::host::SpineConfig;
use spine_core::host::SpineContextEventHandler;
use spine_core::host::TrimEdit;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Debug)]
pub(crate) struct CodexContextHandler {
    node_prompt: String,
    raw_cells: BTreeMap<CellId, ResponseItem>,
    cell_order: Vec<CellId>,
    staged_cells: BTreeMap<RawBoundary, ResponseItem>,
    history_size: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedCodexContext {
    items: Vec<ResponseItem>,
    raw_cells: BTreeMap<CellId, ResponseItem>,
    cell_order: Vec<CellId>,
    history_size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexContextError(pub(crate) String);

impl fmt::Display for CodexContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CodexContextError {}

impl CodexContextHandler {
    pub(crate) fn new(config: &SpineConfig) -> Self {
        Self {
            node_prompt: config.node_prompt().unwrap_or_default().to_string(),
            raw_cells: BTreeMap::new(),
            cell_order: Vec::new(),
            staged_cells: BTreeMap::new(),
            history_size: 0,
        }
    }

    pub(crate) fn reset_sources(&mut self) {
        self.raw_cells.clear();
        self.cell_order.clear();
        self.staged_cells.clear();
        self.history_size = 0;
    }

    pub(crate) fn stage_sources(
        &mut self,
        sources: impl IntoIterator<Item = (RawBoundary, ResponseItem)>,
    ) {
        self.staged_cells.extend(sources);
    }

    pub(crate) fn latest_turn_id(&self) -> Option<&str> {
        self.cell_order
            .iter()
            .rev()
            .filter_map(|cell_id| self.raw_cells.get(cell_id))
            .find_map(ResponseItem::turn_id)
    }

    pub(crate) fn user_message_projection_entries(
        &self,
        stack: &ParseStack,
    ) -> Vec<super::memory_projection::SpinetreeUserMessageProjectionEntry> {
        stack
            .cells()
            .iter()
            .filter_map(|cell| {
                let SpineChar::Message(message) = cell.character() else {
                    return None;
                };
                cell.labels().iter().find_map(|label| {
                    let ContextLabel::UserAnchor(anchor) = label else {
                        return None;
                    };
                    Some(
                        super::memory_projection::SpinetreeUserMessageProjectionEntry {
                            anchor: *anchor,
                            body: message.content.clone(),
                        },
                    )
                })
            })
            .collect()
    }

    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str) -> bool {
        self.raw_cells
            .values_mut()
            .rev()
            .any(|item| replace_images(item, placeholder))
    }
}

impl SpineContextEventHandler for CodexContextHandler {
    type History = ContextManager;
    type PreparedContext = PreparedCodexContext;
    type Error = CodexContextError;

    fn context_size(&self, history: &Self::History) -> usize {
        history.raw_items().len()
    }

    fn prepare_context(
        &self,
        history: &Self::History,
        stack: &ParseStack,
        events: &[ContextEvent],
    ) -> Result<Self::PreparedContext, Self::Error> {
        let source = history.raw_items().to_vec();
        let mut items = source.clone();
        for (offset, staged) in self.staged_cells.values().enumerate() {
            let index = self.history_size.saturating_add(offset);
            let item = items.get_mut(index).ok_or_else(|| {
                CodexContextError(format!("staged context source {index} is out of bounds"))
            })?;
            *item = staged.clone();
        }
        for event in events {
            match event {
                ContextEvent::Tag { index, label } => {
                    let item = items.get_mut(*index).ok_or_else(|| {
                        CodexContextError(format!("context tag index {index} is out of bounds"))
                    })?;
                    apply_label(item, label);
                    if matches!(
                        label,
                        ContextLabel::Output(_) | ContextLabel::SpawnOutput { .. }
                    ) {
                        validate_spine_model_item(item).map_err(CodexContextError)?;
                    }
                }
                ContextEvent::Splice {
                    start,
                    delete,
                    insert,
                } => {
                    let end = start.saturating_add(*delete);
                    if end > items.len() {
                        return Err(CodexContextError(format!(
                            "context splice {start}..{end} exceeds {} items",
                            items.len()
                        )));
                    }
                    let values = insert
                        .iter()
                        .map(|insert| {
                            let value = self.resolve_insert(insert, &source)?;
                            if matches!(insert, ContextInsert::Synthetic { .. }) {
                                validate_spine_model_item(&value).map_err(CodexContextError)?;
                            }
                            Ok(value)
                        })
                        .collect::<Result<Vec<_>, CodexContextError>>()?;
                    items.splice(*start..end, values);
                }
            }
        }
        if items.len() != stack.len() {
            return Err(CodexContextError(format!(
                "materialized context has {} items, expected {}",
                items.len(),
                stack.len()
            )));
        }
        let mut raw_cells = BTreeMap::new();
        for (index, cell) in stack.cells().iter().enumerate() {
            if let Some(raw) = self.raw_cells.get(&cell.id()) {
                raw_cells.insert(cell.id(), raw.clone());
            } else if let Some(raw) = self.resolve_cell(cell, &source) {
                raw_cells.insert(cell.id(), raw);
            }
            if index >= items.len() {
                return Err(CodexContextError("context cell order diverged".to_string()));
            }
        }
        Ok(PreparedCodexContext {
            items,
            raw_cells,
            cell_order: stack.cells().iter().map(ParseCell::id).collect(),
            history_size: stack.len(),
        })
    }

    fn commit_context(&mut self, history: &mut Self::History, prepared: Self::PreparedContext) {
        history.replace(prepared.items);
        self.raw_cells = prepared.raw_cells;
        self.cell_order = prepared.cell_order;
        self.staged_cells.clear();
        self.history_size = prepared.history_size;
    }
}

impl CodexContextHandler {
    fn resolve_insert(
        &self,
        insert: &ContextInsert,
        source: &[ResponseItem],
    ) -> Result<ResponseItem, CodexContextError> {
        match insert {
            ContextInsert::Existing { source_index, .. } => {
                if let Some(cell_id) = self.cell_order.get(*source_index)
                    && let Some(item) = self.raw_cells.get(cell_id)
                {
                    return Ok(item.clone());
                }
                source.get(*source_index).cloned().ok_or_else(|| {
                    CodexContextError(format!("missing source context item {source_index}"))
                })
            }
            ContextInsert::Synthetic { item, .. } => materialize_context(
                std::slice::from_ref(item),
                &[],
                None,
                None,
                &BTreeMap::new(),
                &self.node_prompt,
            )
            .map_err(CodexContextError)?
            .into_iter()
            .next()
            .ok_or_else(|| CodexContextError("synthetic context item rendered empty".to_string())),
        }
    }

    fn resolve_cell(&self, cell: &ParseCell, source: &[ResponseItem]) -> Option<ResponseItem> {
        let boundary = cell.character().boundary();
        self.staged_cells.get(&boundary).cloned().or_else(|| {
            source
                .iter()
                .skip(self.history_size.min(source.len()))
                .find(|item| response_item_matches_char(item, boundary, cell.character()))
                .cloned()
                .or_else(|| {
                    source
                        .iter()
                        .find(|item| response_item_matches_char(item, boundary, cell.character()))
                        .cloned()
                })
        })
    }
}

pub(crate) fn response_item_to_char(item: &ResponseItem, boundary: RawBoundary) -> SpineChar {
    response_item_to_char_and_source(item, boundary).0
}

pub(crate) fn response_item_to_char_and_source(
    item: &ResponseItem,
    boundary: RawBoundary,
) -> (SpineChar, ResponseItem) {
    let mut source_item = item.clone();
    if let ResponseItem::Reasoning { content, .. } = &mut source_item
        && content.is_none()
    {
        // Request serialization omits an empty reasoning content list. Preserve that wire shape
        // after rollout replay, where an omitted field deserializes as `None`.
        *content = Some(Vec::new());
    }
    let character = match item {
        ResponseItem::Message { .. } | ResponseItem::Reasoning { .. } => {
            SpineChar::Message(message_from_response_item(boundary.0 as usize, item))
        }
        _ => SpineChar::Opaque { boundary },
    };
    (character, source_item)
}

pub(crate) fn response_item_to_observation_and_source(
    item: &ResponseItem,
    boundary: RawBoundary,
) -> (SourceObservation, ResponseItem) {
    let (character, source) = response_item_to_char_and_source(item, boundary);
    let observation = match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            let body = match &output.body {
                FunctionCallOutputBody::Text(text) => text.clone(),
                FunctionCallOutputBody::ContentItems(items) => {
                    serde_json::to_string(items).unwrap_or_default()
                }
            };
            SourceObservation::new(character).with_output(ObservedOutput {
                execution_ref: call_id.clone(),
                body,
            })
        }
        _ => SourceObservation::new(character),
    };
    (observation, source)
}

fn response_item_matches_char(
    item: &ResponseItem,
    boundary: RawBoundary,
    character: &SpineChar,
) -> bool {
    match response_item_to_char(item, boundary) {
        SpineChar::Message(message) => {
            matches!(character, SpineChar::Message(expected) if message == *expected)
        }
        SpineChar::Opaque { boundary: actual } => {
            matches!(character, SpineChar::Opaque { boundary: expected } if actual == *expected)
        }
        SpineChar::Synthetic { .. } => false,
    }
}

pub(super) fn apply_label(item: &mut ResponseItem, label: &ContextLabel) {
    match label {
        ContextLabel::UserAnchor(anchor) => {
            crate::context::SpineUserAnchor::new(*anchor).prepend_to(item);
        }
        ContextLabel::Output(edit) => apply_trim_edit(item, edit),
        ContextLabel::SpawnOutput { succeeded } => {
            if let ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } = item
            {
                output.body = FunctionCallOutputBody::Text(
                    serde_json::json!({"status": if *succeeded {
                        "success"
                    } else {
                        "failure"
                    }})
                    .to_string(),
                );
                output.success = Some(*succeeded);
            }
        }
    }
}

fn apply_trim_edit(item: &mut ResponseItem, edit: &TrimEdit) {
    let output = match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return,
    };
    let body = match edit {
        TrimEdit::Tagged { trim_id, body, .. } => format!("[TRIM_ID: {trim_id}]\n{body}"),
        TrimEdit::Snipped => super::TOOL_RESULT_CLEARED_MESSAGE.to_string(),
        TrimEdit::Sliced(value) => value.clone(),
    };
    output.body = FunctionCallOutputBody::Text(body);
}

fn replace_images(item: &mut ResponseItem, placeholder: &str) -> bool {
    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            let text = match &output.body {
                FunctionCallOutputBody::Text(text) => text,
                _ => return false,
            };
            if !text.contains("data:image") {
                return false;
            }
            output.body = FunctionCallOutputBody::Text(placeholder.to_string());
            true
        }
        _ => false,
    }
}

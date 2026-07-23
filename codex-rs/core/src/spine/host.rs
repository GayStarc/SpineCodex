use super::canonical_projected_item;
use super::effective_rollout;
use super::effective_rollout_from_source;
use super::materialize_context;
use super::materialize_trim_only_context;
use super::project_trim_item;
use super::response_item_at;
use crate::context_manager::ContextManager;
use crate::context_manager::is_user_turn_boundary;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use spine_core::ContextItem;
use spine_core::ContextTransition;
use spine_core::HandlerCardinality;
use spine_core::HostStep;
use spine_core::MemorySlot;
use spine_core::NativeItemRef;
use spine_core::RawBoundary;
use spine_core::RuntimeProjection;
use spine_core::SpineEventHandlers;
use spine_core::SpineHost;
use spine_core::SpineObserverEvent;
use spine_core::SpineTransitionEvent;
use spine_core::TokenUsageSample;
use std::fmt;

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexSpineFrontier {
    source: Vec<CodexSpineInput>,
    emitted_events: usize,
}

impl CodexSpineFrontier {
    pub(super) fn last_item(&self) -> Option<&RolloutItem> {
        self.source.last().map(|input| &input.item)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CodexSpineInput {
    pub(crate) ordinal: usize,
    pub(crate) item: RolloutItem,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexSpineMaterialization {
    ledger: MaterializationLedger,
    source_len: usize,
    history_version: u64,
    stats: MaterializationStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MaterializationStats {
    pub(crate) full_rebuilds: usize,
    pub(crate) incremental_renders: usize,
}

#[derive(Clone, Debug)]
enum MaterializationLedger {
    Jit {
        entries: Vec<SemanticMaterialization>,
        pending: Vec<ResponseItem>,
    },
    TrimOnly {
        entries: Vec<NativeMaterialization>,
    },
}

#[derive(Clone, Debug)]
struct SemanticMaterialization {
    item: ContextItem,
    rendered: Vec<ResponseItem>,
}

#[derive(Clone, Debug)]
struct NativeMaterialization {
    input: CodexSpineInput,
    rendered: Vec<ResponseItem>,
}

impl CodexSpineMaterialization {
    fn empty(host: CodexSpineHost) -> Self {
        let ledger = if host.jit_enabled {
            MaterializationLedger::Jit {
                entries: Vec::new(),
                pending: Vec::new(),
            }
        } else {
            MaterializationLedger::TrimOnly {
                entries: Vec::new(),
            }
        };
        Self {
            ledger,
            source_len: 0,
            history_version: 0,
            stats: MaterializationStats::default(),
        }
    }

    pub(crate) fn projected_items(&self) -> Vec<ResponseItem> {
        match &self.ledger {
            MaterializationLedger::Jit { entries, pending } => entries
                .iter()
                .flat_map(|entry| entry.rendered.iter().cloned())
                .chain(pending.iter().cloned())
                .collect(),
            MaterializationLedger::TrimOnly { entries } => entries
                .iter()
                .flat_map(|entry| entry.rendered.iter().cloned())
                .collect(),
        }
    }

    fn inherit_stats(&mut self, previous: &Self) {
        self.stats.full_rebuilds = self
            .stats
            .full_rebuilds
            .saturating_add(previous.stats.full_rebuilds);
        self.stats.incremental_renders = self
            .stats
            .incremental_renders
            .saturating_add(previous.stats.incremental_renders);
    }
}

fn selected_inputs(rollout: &[RolloutItem]) -> Vec<CodexSpineInput> {
    effective_rollout(rollout)
        .into_iter()
        .map(|(ordinal, item)| CodexSpineInput {
            ordinal,
            item: item.clone(),
        })
        .collect()
}

pub(crate) fn replay_inputs(
    rollout: &[RolloutItem],
    history: &ContextManager,
) -> (Vec<CodexSpineInput>, usize) {
    let last_compaction = rollout
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)));
    let archived_rollout = last_compaction.map_or(&[][..], |index| &rollout[..=index]);
    let live_rollout = last_compaction.map_or(rollout, |index| &rollout[index + 1..]);
    let mut inputs = selected_inputs(archived_rollout);

    let archived_source_count = archived_rollout
        .iter()
        .filter(|item| super::is_spine_source_item(item))
        .count();
    let live_effective = effective_rollout(live_rollout);
    let history_prefix_len = inputs
        .iter()
        .rev()
        .find_map(|input| match &input.item {
            RolloutItem::Compacted(compacted) => compacted
                .replacement_history
                .as_ref()
                .filter(|replacement| history.raw_items().starts_with(replacement))
                .map(Vec::len),
            _ => None,
        })
        .unwrap_or(0);
    if let Some(CodexSpineInput {
        item: RolloutItem::Compacted(compacted),
        ..
    }) = inputs
        .iter_mut()
        .rfind(|input| matches!(input.item, RolloutItem::Compacted(_)))
    {
        compacted.replacement_history = Some(history.raw_items()[..history_prefix_len].to_vec());
    }
    let live_history = &history.raw_items()[history_prefix_len..];
    let live_sources = live_effective
        .iter()
        .filter_map(|(ordinal, item)| match item {
            RolloutItem::ResponseItem(item) => {
                Some((*ordinal, canonical_projected_item(history, item)))
            }
            RolloutItem::InterAgentCommunication(communication) => {
                let item = communication.to_model_input_item();
                Some((*ordinal, canonical_projected_item(history, &item)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut source_cursor = 0;
    let mapped_ordinals = live_history
        .iter()
        .map(|history_item| {
            let offset = live_sources[source_cursor..]
                .iter()
                .position(|(_, source_item)| source_item == history_item)?;
            source_cursor = source_cursor.saturating_add(offset + 1);
            Some(live_sources[source_cursor - 1].0)
        })
        .collect::<Option<Vec<_>>>();
    let canonical_next_ordinal = rollout
        .iter()
        .filter(|item| super::is_spine_source_item(item))
        .count();
    if let Some(mapped_ordinals) = mapped_ordinals {
        let mut mapped_history = mapped_ordinals.into_iter().zip(live_history).peekable();
        for (ordinal, item) in live_effective {
            let ordinal = archived_source_count.saturating_add(ordinal);
            match item {
                RolloutItem::EventMsg(EventMsg::TokenCount(_)) => {
                    inputs.push(CodexSpineInput {
                        ordinal,
                        item: item.clone(),
                    });
                }
                _ => {
                    let Some((_, history_item)) = mapped_history.next_if(|(mapped_ordinal, _)| {
                        archived_source_count.saturating_add(*mapped_ordinal) == ordinal
                    }) else {
                        continue;
                    };
                    inputs.push(CodexSpineInput {
                        ordinal,
                        item: RolloutItem::ResponseItem(history_item.clone()),
                    });
                }
            }
        }
        return (inputs, canonical_next_ordinal);
    }

    let live_source_count = live_effective
        .iter()
        .filter(|(_, item)| super::is_spine_source_item(item))
        .count();
    let fallback_prefix_len = live_history.len().saturating_sub(live_source_count);
    let mut usage = live_effective
        .into_iter()
        .filter_map(|(ordinal, item)| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(_)) => Some((
                fallback_prefix_len
                    .saturating_add(ordinal)
                    .min(live_history.len()),
                item.clone(),
            )),
            _ => None,
        })
        .peekable();
    let mut next_ordinal = archived_source_count;
    for (position, item) in live_history.iter().enumerate() {
        while let Some((_, item)) = usage.next_if(|(usage_position, _)| *usage_position <= position)
        {
            inputs.push(CodexSpineInput {
                ordinal: next_ordinal,
                item,
            });
        }
        inputs.push(CodexSpineInput {
            ordinal: next_ordinal,
            item: RolloutItem::ResponseItem(item.clone()),
        });
        next_ordinal = next_ordinal.saturating_add(1);
    }
    inputs.extend(usage.map(|(_, item)| CodexSpineInput {
        ordinal: next_ordinal,
        item,
    }));

    (inputs, next_ordinal.max(canonical_next_ordinal))
}

impl CodexSpineHost {
    pub(crate) fn user_message_projection_entries(
        &self,
        frontier: &CodexSpineFrontier,
    ) -> Vec<super::memory_projection::SpinetreeUserMessageProjectionEntry> {
        let source = frontier
            .source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        super::user_message_projection_entries_from_effective(&effective_rollout_from_source(
            &source,
        ))
    }

    pub(crate) fn validate_trim_request(
        &self,
        frontier: &CodexSpineFrontier,
        current_call_id: &str,
        request: &spine_core::TrimRequest,
    ) -> Result<(), String> {
        let source = frontier
            .source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        super::validate_trim_request_from_effective(
            &effective_rollout_from_source(&source),
            current_call_id,
            request,
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CodexSpineHost {
    pub(crate) jit_enabled: bool,
    pub(crate) spawn_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexSpineHostError(String);

impl fmt::Display for CodexSpineHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CodexSpineHostError {}

#[derive(Debug)]
pub(crate) struct CodexSpineEventHandlers {
    host: CodexSpineHost,
    memory_projection_enabled: bool,
    materialization: CodexSpineMaterialization,
    pending_observer_effect: Option<super::observer::CodexSpineObserverEffect>,
}

impl CodexSpineEventHandlers {
    pub(crate) fn new(host: CodexSpineHost, memory_projection_enabled: bool) -> Self {
        Self {
            host,
            memory_projection_enabled,
            materialization: CodexSpineMaterialization::empty(host),
            pending_observer_effect: None,
        }
    }

    pub(crate) fn take_observer_effect(
        &mut self,
    ) -> Option<super::observer::CodexSpineObserverEffect> {
        self.pending_observer_effect.take()
    }

    #[cfg(test)]
    pub(crate) fn materialization_stats(&self) -> MaterializationStats {
        self.materialization.stats.clone()
    }

    pub(crate) fn projected_items(&self) -> Vec<ResponseItem> {
        self.materialization.projected_items()
    }

    pub(crate) fn replace_last_turn_images(
        &mut self,
        placeholder: &str,
        history_version: u64,
    ) -> bool {
        let replaced = match &mut self.materialization.ledger {
            MaterializationLedger::Jit { entries, pending } => {
                pending.iter_mut().rev().any(|item| {
                    replace_last_turn_images_in(std::slice::from_mut(item), placeholder)
                }) || entries
                    .iter_mut()
                    .rev()
                    .any(|entry| replace_last_turn_images_in(&mut entry.rendered, placeholder))
            }
            MaterializationLedger::TrimOnly { entries } => entries
                .iter_mut()
                .rev()
                .any(|entry| replace_last_turn_images_in(&mut entry.rendered, placeholder)),
        };
        if replaced {
            self.materialization.history_version = history_version;
        }
        replaced
    }
}

impl SpineEventHandlers<CodexSpineFrontier> for CodexSpineEventHandlers {
    type History = ContextManager;
    type PreparedContext = CodexSpineMaterialization;
    type Error = CodexSpineHostError;

    fn cardinality(&self) -> HandlerCardinality {
        HandlerCardinality {
            context_owners: 1,
            observers: 1,
        }
    }

    fn prepare_context(
        &self,
        history: &Self::History,
        event: SpineTransitionEvent<'_, CodexSpineFrontier>,
    ) -> Result<Self::PreparedContext, Self::Error> {
        match event.transition {
            ContextTransition::Append(items) => {
                let visible = &event.runtime_projection.spine().visible_context;
                let start = visible.len().checked_sub(items.len()).ok_or_else(|| {
                    CodexSpineHostError("context append exceeds runtime projection".to_string())
                })?;
                let mut materialization = self.materialization.clone();
                self.host.update_materialization(
                    &mut materialization,
                    event.frontier,
                    history,
                    &spine_core::ContextEdit {
                        start,
                        delete: 0,
                        insert: items.to_vec(),
                    },
                    event.runtime_projection,
                )?;
                Ok(materialization)
            }
            ContextTransition::ContextEpochReset(context) => {
                if context != event.runtime_projection.spine().visible_context {
                    return Err(CodexSpineHostError(
                        "context epoch reset does not match the runtime projection".to_string(),
                    ));
                }
                let mut materialization = self.host.rebuild_materialization(
                    event.frontier,
                    history,
                    event.runtime_projection,
                )?;
                materialization.inherit_stats(&self.materialization);
                Ok(materialization)
            }
        }
    }

    fn commit_context(
        &mut self,
        history: &mut Self::History,
        mut materialization: Self::PreparedContext,
    ) {
        materialization.history_version = history.history_version();
        self.materialization = materialization;
    }

    fn notify_observers(&mut self, event: SpineObserverEvent<'_, CodexSpineFrontier>) {
        let effect = super::observer::CodexSpineObserverEffect::from_event(
            self.host,
            self.memory_projection_enabled,
            event,
        );
        if effect.is_empty() {
            return;
        }
        match &mut self.pending_observer_effect {
            Some(pending) => pending.merge(effect),
            None => self.pending_observer_effect = Some(effect),
        }
    }
}

impl SpineHost for CodexSpineHost {
    type Input = CodexSpineInput;
    type Frontier = CodexSpineFrontier;
    type Error = CodexSpineHostError;

    fn initial_frontier(&self) -> Self::Frontier {
        CodexSpineFrontier::default()
    }

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error> {
        let mut source = frontier.source.clone();
        source.push(input.clone());
        let effective = source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        let events = super::stable_lex_rollout(&effective, self.spawn_enabled);
        if events.events.len() < frontier.emitted_events {
            return Err(CodexSpineHostError(
                "rollout changed before runtime reset; replay the selected prefix".to_string(),
            ));
        }
        let new_events = events.events[frontier.emitted_events..].to_vec();
        let observed_boundary = effective
            .last()
            .map(|(ordinal, _)| RawBoundary(*ordinal as u64));
        let next_frontier = CodexSpineFrontier {
            source,
            emitted_events: events.events.len(),
        };
        let step = HostStep::new(next_frontier, new_events, events.pending, observed_boundary);
        let usage_sample = match &input.item {
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => event
                .info
                .as_ref()
                .map(|info| info.last_token_usage.input_tokens)
                .filter(|input_tokens| *input_tokens > 0)
                .map(|input_tokens| TokenUsageSample {
                    boundary: RawBoundary(input.ordinal as u64),
                    input_tokens,
                }),
            _ => None,
        };
        let step = match usage_sample {
            Some(sample) => step.with_usage_sample(sample),
            None => step,
        };
        Ok(step)
    }
}

impl CodexSpineHost {
    pub(crate) fn rebuild_materialization(
        self,
        frontier: &CodexSpineFrontier,
        base: &ContextManager,
        update: &RuntimeProjection,
    ) -> Result<CodexSpineMaterialization, CodexSpineHostError> {
        let effective = frontier
            .source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        let trim = update.trim_projection();
        let mut stats = MaterializationStats {
            full_rebuilds: 1,
            incremental_renders: 0,
        };
        let ledger = if self.jit_enabled {
            let entries = update
                .spine()
                .visible_context
                .iter()
                .map(|item| {
                    self.render_semantic_item(item, &effective, base, update, &mut stats)
                        .map(|rendered| SemanticMaterialization {
                            item: item.clone(),
                            rendered,
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            MaterializationLedger::Jit {
                entries,
                pending: Self::render_pending(&effective, base, update, &mut stats)?,
            }
        } else {
            let start = frontier
                .source
                .iter()
                .rposition(|input| matches!(input.item, RolloutItem::Compacted(_)))
                .unwrap_or(0);
            let entries = frontier.source[start..]
                .iter()
                .map(|input| {
                    Self::render_native_input(input, base, trim, &mut stats).map(|rendered| {
                        NativeMaterialization {
                            input: input.clone(),
                            rendered,
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            MaterializationLedger::TrimOnly { entries }
        };

        Ok(CodexSpineMaterialization {
            ledger,
            source_len: frontier.source.len(),
            history_version: base.history_version(),
            stats,
        })
    }

    pub(crate) fn update_materialization(
        self,
        materialization: &mut CodexSpineMaterialization,
        frontier: &CodexSpineFrontier,
        base: &ContextManager,
        edit: &spine_core::ContextEdit,
        update: &RuntimeProjection,
    ) -> Result<(), CodexSpineHostError> {
        if materialization.history_version != base.history_version() {
            let mut rebuilt = self.rebuild_materialization(frontier, base, update)?;
            rebuilt.inherit_stats(materialization);
            *materialization = rebuilt;
            return Ok(());
        }

        let effective = frontier
            .source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        match &mut materialization.ledger {
            MaterializationLedger::Jit { entries, pending } => {
                let end = edit.start.saturating_add(edit.delete);
                if end > entries.len() {
                    return Err(CodexSpineHostError(format!(
                        "context edit {}..{end} exceeds materialization length {}",
                        edit.start,
                        entries.len()
                    )));
                }
                let inserts = edit
                    .insert
                    .iter()
                    .map(|item| {
                        self.render_semantic_item(
                            item,
                            &effective,
                            base,
                            update,
                            &mut materialization.stats,
                        )
                        .map(|rendered| SemanticMaterialization {
                            item: item.clone(),
                            rendered,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                entries.splice(edit.start..end, inserts);

                let expected = &update.spine().visible_context;
                if !entries.iter().map(|entry| &entry.item).eq(expected.iter()) {
                    return Err(CodexSpineHostError(
                        "context edit did not reproduce the runtime projection".to_string(),
                    ));
                }

                for entry in entries.iter_mut().filter(|entry| {
                    update
                        .trim_changed_boundaries()
                        .iter()
                        .any(|boundary| item_references_boundary(&entry.item, *boundary))
                }) {
                    entry.rendered = self.render_semantic_item(
                        &entry.item,
                        &effective,
                        base,
                        update,
                        &mut materialization.stats,
                    )?;
                }
                *pending =
                    Self::render_pending(&effective, base, update, &mut materialization.stats)?;
            }
            MaterializationLedger::TrimOnly { entries } => {
                if materialization.source_len > frontier.source.len() {
                    *materialization = self.rebuild_materialization(frontier, base, update)?;
                    return Ok(());
                }
                for input in &frontier.source[materialization.source_len..] {
                    if matches!(input.item, RolloutItem::Compacted(_)) {
                        entries.clear();
                    }
                    entries.push(NativeMaterialization {
                        input: input.clone(),
                        rendered: Self::render_native_input(
                            input,
                            base,
                            update.trim_projection(),
                            &mut materialization.stats,
                        )?,
                    });
                }
                for entry in entries.iter_mut().filter(|entry| {
                    update
                        .trim_changed_boundaries()
                        .iter()
                        .any(|boundary| entry.input.ordinal as u64 == boundary.0)
                }) {
                    entry.rendered = Self::render_native_input(
                        &entry.input,
                        base,
                        update.trim_projection(),
                        &mut materialization.stats,
                    )?;
                }
            }
        }
        materialization.source_len = frontier.source.len();
        materialization.history_version = base.history_version();
        Ok(())
    }

    fn render_semantic_item(
        self,
        item: &ContextItem,
        effective: &[(usize, &RolloutItem)],
        base: &ContextManager,
        update: &RuntimeProjection,
        stats: &mut MaterializationStats,
    ) -> Result<Vec<ResponseItem>, CodexSpineHostError> {
        stats.incremental_renders = stats.incremental_renders.saturating_add(1);
        materialize_context(
            std::slice::from_ref(item),
            effective,
            update.trim_projection(),
            Some(base),
            self.spawn_enabled,
        )
        .map_err(CodexSpineHostError)
    }

    fn render_pending(
        effective: &[(usize, &RolloutItem)],
        base: &ContextManager,
        update: &RuntimeProjection,
        stats: &mut MaterializationStats,
    ) -> Result<Vec<ResponseItem>, CodexSpineHostError> {
        let mut pending = Vec::new();
        for source in update.pending() {
            let NativeItemRef::Rollout { ordinal } = source else {
                continue;
            };
            let raw = response_item_at(effective, *ordinal, Some(base)).ok_or_else(|| {
                CodexSpineHostError(format!("missing rollout source {}", ordinal.0))
            })?;
            stats.incremental_renders = stats.incremental_renders.saturating_add(1);
            pending.push(project_trim_item(
                raw,
                usize::try_from(ordinal.0).unwrap_or(usize::MAX),
                update.trim_projection(),
            ));
        }
        Ok(pending)
    }

    fn render_native_input(
        input: &CodexSpineInput,
        base: &ContextManager,
        trim: Option<&spine_core::TrimProjection>,
        stats: &mut MaterializationStats,
    ) -> Result<Vec<ResponseItem>, CodexSpineHostError> {
        stats.incremental_renders = stats.incremental_renders.saturating_add(1);
        materialize_trim_only_context(&[(input.ordinal, &input.item)], trim, Some(base))
            .map_err(CodexSpineHostError)
    }
}

fn item_references_boundary(item: &ContextItem, boundary: RawBoundary) -> bool {
    match item {
        ContextItem::Message { message, .. } => message.boundary == boundary,
        ContextItem::ToolCall(group) => group.start.0 <= boundary.0 && boundary.0 <= group.end.0,
        ContextItem::MemorySlot(MemorySlot::User { message, .. }) => message.boundary == boundary,
        ContextItem::Native {
            source: NativeItemRef::Rollout { ordinal },
        } => *ordinal == boundary,
        ContextItem::SyntheticNode { .. }
        | ContextItem::MemorySlot(MemorySlot::Summary { .. })
        | ContextItem::MemorySlot(MemorySlot::SpawnEvidence { .. })
        | ContextItem::Native {
            source: NativeItemRef::CompactReplacement { .. },
        } => false,
    }
}

fn replace_last_turn_images_in(items: &mut [ResponseItem], placeholder: &str) -> bool {
    let Some(index) = items.iter().rposition(|item| {
        matches!(item, ResponseItem::FunctionCallOutput { .. }) || is_user_turn_boundary(item)
    }) else {
        return false;
    };
    let ResponseItem::FunctionCallOutput { output, .. } = &mut items[index] else {
        return false;
    };
    let Some(content_items) = output.content_items_mut() else {
        return false;
    };
    let mut replaced = false;
    for item in content_items {
        if matches!(item, FunctionCallOutputContentItem::InputImage { .. }) {
            *item = FunctionCallOutputContentItem::InputText {
                text: placeholder.to_string(),
            };
            replaced = true;
        }
    }
    replaced
}

use super::effective_rollout;
use super::effective_rollout_from_source;
use super::materialize_context;
use super::materialize_trim_only_context;
use super::project_trim_item;
use super::response_item_at;
use crate::context_manager::ContextManager;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use spine_core::ContextItem;
use spine_core::HostStep;
use spine_core::MemorySlot;
use spine_core::NativeItemRef;
use spine_core::RawBoundary;
use spine_core::RuntimeProjection;
use spine_core::SpineHost;
use spine_core::SpineOutput;
use spine_core::TokenUsageSample;
use std::fmt;

#[derive(Clone, Debug, Default)]
pub(crate) struct CodexSpineFrontier {
    source: Vec<CodexSpineInput>,
    emitted_events: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexSpineInput {
    pub(crate) ordinal: usize,
    pub(crate) item: RolloutItem,
}

#[derive(Debug)]
pub(crate) struct CodexSpineMaterialization {
    ledger: MaterializationLedger,
    source_len: usize,
    history_version: u64,
}

#[derive(Debug)]
enum MaterializationLedger {
    Jit {
        entries: Vec<SemanticMaterialization>,
        pending: Vec<ResponseItem>,
    },
    TrimOnly {
        entries: Vec<NativeMaterialization>,
    },
}

#[derive(Debug)]
struct SemanticMaterialization {
    item: ContextItem,
    rendered: Vec<ResponseItem>,
}

#[derive(Debug)]
struct NativeMaterialization {
    input: CodexSpineInput,
    rendered: Vec<ResponseItem>,
}

impl CodexSpineMaterialization {
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
    let live_source_count = live_effective
        .iter()
        .filter(|(_, item)| super::is_spine_source_item(item))
        .count();
    let history_prefix_len = inputs
        .iter()
        .rev()
        .find_map(|input| match &input.item {
            RolloutItem::Compacted(compacted) => Some(
                compacted
                    .replacement_history
                    .as_ref()
                    .map_or_else(
                        || history.raw_items().len().saturating_sub(live_source_count),
                        Vec::len,
                    )
                    .min(history.raw_items().len()),
            ),
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
        // Keep reconstructed replacement history opaque; it already contains
        // the model-visible projection from the compacted epoch.
        compacted.replacement_history = Some(history.raw_items()[..history_prefix_len].to_vec());
    }
    let live_history = &history.raw_items()[history_prefix_len..];
    let live_sources = live_effective
        .iter()
        .filter_map(|(ordinal, item)| match item {
            RolloutItem::ResponseItem(item) => {
                Some((*ordinal, history.canonical_projected_item(item)))
            }
            RolloutItem::InterAgentCommunication(communication) => {
                let item = communication.to_model_input_item();
                Some((*ordinal, history.canonical_projected_item(&item)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut source_cursor = 0;
    let mapped_ordinals = live_history.iter()
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
        let mut mapped_history = mapped_ordinals
            .into_iter()
            .zip(live_history)
            .peekable();
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

    let mut usage = live_effective
        .into_iter()
        .filter_map(|(ordinal, item)| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(_)) => Some((
                ordinal.min(live_history.len()),
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
        let ledger = if self.jit_enabled {
            let entries = update
                .spine()
                .visible_context
                .iter()
                .map(|item| {
                    self.render_semantic_item(item, &effective, base, update)
                        .map(|rendered| SemanticMaterialization {
                            item: item.clone(),
                            rendered,
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            MaterializationLedger::Jit {
                entries,
                pending: Self::render_pending(&effective, base, update)?,
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
                    Self::render_native_input(input, base, trim).map(|rendered| {
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
        })
    }

    pub(crate) fn update_materialization(
        self,
        materialization: &mut CodexSpineMaterialization,
        frontier: &CodexSpineFrontier,
        base: &ContextManager,
        output: &SpineOutput,
    ) -> Result<(), CodexSpineHostError> {
        if materialization.history_version != base.history_version() {
            *materialization =
                self.rebuild_materialization(frontier, base, output.runtime_projection())?;
            return Ok(());
        }

        let effective = frontier
            .source
            .iter()
            .map(|input| (input.ordinal, &input.item))
            .collect::<Vec<_>>();
        match &mut materialization.ledger {
            MaterializationLedger::Jit { entries, pending } => {
                let edit = &output.delta().context_edit;
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
                            output.runtime_projection(),
                        )
                        .map(|rendered| SemanticMaterialization {
                            item: item.clone(),
                            rendered,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                entries.splice(edit.start..end, inserts);

                let expected = &output.runtime_projection().spine().visible_context;
                if !entries.iter().map(|entry| &entry.item).eq(expected.iter()) {
                    return Err(CodexSpineHostError(
                        "context edit did not reproduce the runtime projection".to_string(),
                    ));
                }

                for entry in entries.iter_mut().filter(|entry| {
                    output
                        .runtime_projection()
                        .trim_changed_boundaries()
                        .iter()
                        .any(|boundary| item_references_boundary(&entry.item, *boundary))
                }) {
                    entry.rendered = self.render_semantic_item(
                        &entry.item,
                        &effective,
                        base,
                        output.runtime_projection(),
                    )?;
                }
                *pending = Self::render_pending(&effective, base, output.runtime_projection())?;
            }
            MaterializationLedger::TrimOnly { entries } => {
                if materialization.source_len > frontier.source.len() {
                    *materialization =
                        self.rebuild_materialization(frontier, base, output.runtime_projection())?;
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
                            output.runtime_projection().trim_projection(),
                        )?,
                    });
                }
                for entry in entries.iter_mut().filter(|entry| {
                    output
                        .runtime_projection()
                        .trim_changed_boundaries()
                        .iter()
                        .any(|boundary| entry.input.ordinal as u64 == boundary.0)
                }) {
                    entry.rendered = Self::render_native_input(
                        &entry.input,
                        base,
                        output.runtime_projection().trim_projection(),
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
    ) -> Result<Vec<ResponseItem>, CodexSpineHostError> {
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
    ) -> Result<Vec<ResponseItem>, CodexSpineHostError> {
        let mut pending = Vec::new();
        for source in update.pending() {
            let NativeItemRef::Rollout { ordinal } = source else {
                continue;
            };
            let raw = response_item_at(effective, *ordinal, Some(base)).ok_or_else(|| {
                CodexSpineHostError(format!("missing rollout source {}", ordinal.0))
            })?;
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
    ) -> Result<Vec<ResponseItem>, CodexSpineHostError> {
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

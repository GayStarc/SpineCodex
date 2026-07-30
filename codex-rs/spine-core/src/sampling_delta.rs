use crate::CharParseError;
use crate::ExecutedSpineFact;
use crate::ExecutionId;
use crate::ExecutionOrigin;
use crate::RawBoundary;
use crate::RolloutEvent;
use crate::SourceCellId;
use crate::SourceSnapshot;
use crate::SpineCharParser;
use crate::SpineCompactBarrierV1;
use crate::SpineCompiler;
use crate::SpineOperationFact;
use crate::ToolCallGroup;
use crate::archive::FactSourceBinding;
use crate::compiler::SamplingCompileError;

#[derive(Debug)]
pub(crate) enum SamplingDeltaError {
    Parse(CharParseError),
    Compile(SamplingCompileError),
    MissingSourceBoundary(RawBoundary),
    MissingTrimSource(SourceCellId),
    FactHasNoSourceGroup(ExecutionId),
    FactSourceAppliedMoreThanOnce,
    FactSourceExecutionMismatch,
}

pub(crate) enum FactBindingMode<'a> {
    Derive,
    Verify(&'a [FactSourceBinding]),
}

/// Projects source observed so far without closing the active sampling boundary.
pub(crate) fn preview_source_delta(
    snapshot: &SourceSnapshot,
    committed_source_cells: usize,
    parser: &mut SpineCharParser,
    compiler: &mut SpineCompiler,
) -> Result<(), SamplingDeltaError> {
    for cell in &snapshot.cells()[committed_source_cells..] {
        let step = parser
            .eat(cell.character())
            .map_err(SamplingDeltaError::Parse)?;
        for event in step.events() {
            compiler
                .eat_source(event.clone())
                .map_err(SamplingDeltaError::Compile)?;
        }
    }
    Ok(())
}

/// Reduces exactly one sampling's source delta through the parser and compiler.
///
/// JIT derives stable fact/source bindings from execution origins. AoT verifies
/// the durable bindings, but both paths execute this same transition kernel.
pub(crate) fn reduce_sampling_delta(
    snapshot: &SourceSnapshot,
    committed_source_cells: usize,
    post_boundary: RawBoundary,
    parser: &mut SpineCharParser,
    compiler: &mut SpineCompiler,
    facts: &[ExecutedSpineFact],
    binding_mode: FactBindingMode<'_>,
) -> Result<Vec<FactSourceBinding>, SamplingDeltaError> {
    let expected = match binding_mode {
        FactBindingMode::Derive => None,
        FactBindingMode::Verify(bindings) => {
            if bindings.len() != facts.len()
                || facts
                    .iter()
                    .zip(bindings)
                    .any(|(fact, binding)| fact.execution_id != binding.execution_id)
            {
                return Err(SamplingDeltaError::FactSourceExecutionMismatch);
            }
            Some(bindings)
        }
    };
    let mut applied = vec![false; facts.len()];
    let mut bindings = vec![None; facts.len()];

    for cell in &snapshot.cells()[committed_source_cells..] {
        let step = parser
            .eat(cell.character())
            .map_err(SamplingDeltaError::Parse)?;
        for event in step.events() {
            match event {
                RolloutEvent::ToolCall(group) => {
                    let (start, end) = group_source_span(snapshot, group)?;
                    let mut matched_facts = Vec::new();
                    for (index, fact) in facts.iter().enumerate() {
                        let matches = expected.map_or_else(
                            || group_contains_origin(group, &fact.origin),
                            |expected| {
                                expected[index].start == start
                                    && expected[index].end == end
                                    && group_contains_origin(group, &fact.origin)
                            },
                        );
                        if !matches {
                            continue;
                        }
                        if applied[index] {
                            return Err(SamplingDeltaError::FactSourceAppliedMoreThanOnce);
                        }
                        applied[index] = true;
                        bindings[index] = Some(FactSourceBinding {
                            execution_id: fact.execution_id.clone(),
                            start: start.clone(),
                            end: end.clone(),
                        });
                        matched_facts.push(fact);
                    }
                    let trims = resolve_trim_boundaries(snapshot, &matched_facts)?;
                    compiler
                        .eat_sampling_group(group.clone(), &matched_facts, &trims)
                        .map_err(SamplingDeltaError::Compile)?;
                }
                event => {
                    compiler
                        .eat_source(event.clone())
                        .map_err(SamplingDeltaError::Compile)?;
                }
            }
        }
    }
    for event in parser
        .finish_sampling(post_boundary)
        .map_err(SamplingDeltaError::Parse)?
    {
        compiler
            .eat_source(event)
            .map_err(SamplingDeltaError::Compile)?;
    }

    facts
        .iter()
        .zip(applied)
        .zip(bindings)
        .map(|((fact, applied), binding)| {
            if !applied {
                return Err(SamplingDeltaError::FactHasNoSourceGroup(
                    fact.execution_id.clone(),
                ));
            }
            binding.ok_or(SamplingDeltaError::FactSourceExecutionMismatch)
        })
        .collect()
}

/// Reduces the source tail and closes the current epoch at one compact barrier.
///
/// JIT and AoT keep different source-of-truth inputs, but compact must perform
/// the same parser/compiler transition in both modes.
pub(crate) fn reduce_compact_delta(
    snapshot: &SourceSnapshot,
    committed_source_cells: usize,
    barrier: &SpineCompactBarrierV1,
    parser: &mut SpineCharParser,
    compiler: &mut SpineCompiler,
) -> Result<(), SamplingDeltaError> {
    if committed_source_cells != snapshot.cells().len() {
        let post_boundary = snapshot
            .last_boundary()
            .map(|boundary| RawBoundary(boundary.ordinal()))
            .unwrap_or(barrier.boundary);
        reduce_sampling_delta(
            snapshot,
            committed_source_cells,
            post_boundary,
            parser,
            compiler,
            &[],
            FactBindingMode::Derive,
        )?;
    }
    for event in parser
        .finish_epoch(barrier.boundary)
        .map_err(SamplingDeltaError::Parse)?
    {
        compiler
            .eat_source(event)
            .map_err(SamplingDeltaError::Compile)?;
    }
    compiler
        .eat_source(RolloutEvent::Compact {
            boundary: barrier.boundary,
            replacement_history: Vec::new(),
        })
        .map_err(SamplingDeltaError::Compile)?;
    *parser = SpineCharParser::default();
    for boundary in &barrier.replacement_boundaries {
        let step = parser
            .eat(crate::SpineChar::Opaque {
                boundary: *boundary,
            })
            .map_err(SamplingDeltaError::Parse)?;
        for event in step.events() {
            compiler
                .eat_source(event.clone())
                .map_err(SamplingDeltaError::Compile)?;
        }
    }
    Ok(())
}

fn group_contains_origin(group: &ToolCallGroup, origin: &ExecutionOrigin) -> bool {
    let call_id = match origin {
        ExecutionOrigin::Direct { call_id } => call_id,
        ExecutionOrigin::CodeMode { outer_call_id, .. } => outer_call_id,
    };
    group.calls.iter().any(|call| call.call_id == *call_id)
}

fn group_source_span(
    snapshot: &SourceSnapshot,
    group: &ToolCallGroup,
) -> Result<(SourceCellId, SourceCellId), SamplingDeltaError> {
    let start = snapshot
        .source_at_raw_boundary(group.start)
        .ok_or(SamplingDeltaError::MissingSourceBoundary(group.start))?
        .id
        .clone();
    let end = snapshot
        .source_at_raw_boundary(group.end)
        .ok_or(SamplingDeltaError::MissingSourceBoundary(group.end))?
        .id
        .clone();
    Ok((start, end))
}

fn resolve_trim_boundaries<'a>(
    source: &SourceSnapshot,
    facts: &[&'a ExecutedSpineFact],
) -> Result<Vec<(RawBoundary, &'a ExecutedSpineFact)>, SamplingDeltaError> {
    facts
        .iter()
        .filter_map(|fact| match &fact.operation {
            SpineOperationFact::Trim { target, .. } => Some(
                source
                    .boundary(&target.response)
                    .map(|boundary| (RawBoundary(boundary.ordinal()), *fact))
                    .ok_or_else(|| SamplingDeltaError::MissingTrimSource(target.response.clone())),
            ),
            SpineOperationFact::Open { .. }
            | SpineOperationFact::Close { .. }
            | SpineOperationFact::Next { .. }
            | SpineOperationFact::Spawn { .. } => None,
        })
        .collect()
}

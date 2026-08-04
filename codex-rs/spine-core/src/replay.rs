use crate::ContextEpoch;
use crate::ContextItem;
use crate::ContextPlanRecipe;
use crate::ContextPlanSource;
use crate::MemorySlot;
use crate::RawBoundary;
use crate::RecordDigest;
use crate::SamplingArchiveRecord;
use crate::SamplingAttemptId;
use crate::SamplingCommit;
use crate::SamplingCommitId;
use crate::SamplingStarted;
use crate::SourceLedger;
use crate::SourceLedgerError;
use crate::SpineChar;
use crate::SpineCharParser;
use crate::SpineCompiler;
use crate::SpineConfig;
use crate::SpineProjection;
use crate::ThreadNamespace;
use crate::TokenUsageSample;
use crate::TreeSnapshot;
use crate::archive::ArchiveError;
use crate::archive::FactSourceBinding;
use crate::context_plan::ContextPlanError;
use crate::planner::RecoveredPlannerState;
use crate::pressure::InputPressureState;
use crate::tree_snapshot;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

mod apply;
mod compact;

use crate::sampling_delta::FactBindingMode;
use crate::sampling_delta::SamplingDeltaError;
use crate::sampling_delta::reduce_compact_delta;
use crate::sampling_delta::reduce_sampling_delta;
use apply::map_compile_error;
use apply::projection_memory_slots;
use apply::verify_memory_slots;
pub use compact::SpineCompactBarrierV1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayInput {
    Source(SpineChar),
    Archive(SamplingArchiveRecord),
    Compact(SpineCompactBarrierV1),
    Usage(TokenUsageSample),
}

#[derive(Clone, Debug)]
pub struct CanonicalReplay {
    thread: ThreadNamespace,
    runtime_config: SpineConfig,
}

pub struct PreparedReplay {
    pub projection: SpineProjection,
    pub live_context: Vec<ContextItem>,
    pub live_plan: Option<ContextPlanRecipe>,
    pub memory_slots: Vec<MemorySlot>,
    pub usage_samples: Vec<TokenUsageSample>,
    pub tree: TreeSnapshot,
    pub applied_commits: Vec<SamplingCommitId>,
    pub final_epoch: ContextEpoch,
    candidate: crate::SamplingRuntime,
}

#[derive(Debug)]
pub enum ReplayError {
    Initialize(crate::InitError),
    Archive(ArchiveError),
    Source(SourceLedgerError),
    Parse(crate::CharParseError),
    Compile(crate::SpineError),
    Transition(String),
    ContextPlan(ContextPlanError),
    Planner(crate::PlannerError),
    CommitWithoutStarted,
    ConflictingSamplingStarted,
    SamplingStartedMismatch,
    IdentityScopeMismatch,
    InvalidCommitChain,
    InvalidPreBoundaryChain,
    PostBoundaryIsNotSourceTail,
    SourceDigestMismatch,
    FactSourceMissing,
    FactSourceAppliedMoreThanOnce,
    MemorySlotMismatch,
    CompactWhileSourceIsPending,
    InvalidCompactBarrier(&'static str),
    Serialize(String),
}

impl CanonicalReplay {
    pub fn new(thread: ThreadNamespace) -> Result<Self, crate::InitError> {
        Ok(Self {
            thread,
            runtime_config: SpineConfig::default().with_features([
                crate::Feature::Jit,
                crate::Feature::Trim,
                crate::Feature::Spawn,
            ])?,
        })
    }

    pub fn with_runtime_config(
        mut self,
        runtime_config: SpineConfig,
    ) -> Result<Self, crate::InitError> {
        SpineCompiler::new(runtime_config.clone())?;
        self.runtime_config = runtime_config;
        Ok(self)
    }

    pub fn prepare<I>(&self, input: I) -> Result<PreparedReplay, ReplayError>
    where
        I: IntoIterator<Item = ReplayInput>,
    {
        let config = self.runtime_config.clone();
        let compiler = SpineCompiler::new(config.clone()).map_err(ReplayError::Initialize)?;
        let source = SourceLedger::new(self.thread.clone(), ContextEpoch::ZERO)
            .map_err(ReplayError::Source)?;
        let mut state = ReplayState {
            thread: self.thread.clone(),
            epoch: ContextEpoch::ZERO,
            source,
            parser: SpineCharParser::default(),
            compiler,
            epoch_start_boundary: crate::BoundaryId::new(
                self.thread.clone(),
                ContextEpoch::ZERO,
                0,
            ),
            committed_source_cells: 0,
            previous_pre_boundary: None,
            previous_commit_id: None,
            live_plan: None,
            started: BTreeMap::new(),
            commits: BTreeMap::new(),
            attempt_ids: BTreeSet::new(),
            applied_commits: Vec::new(),
            usage_samples: Vec::new(),
            input_pressure: InputPressureState::default(),
            next_projection_ordinal: 0,
            spawn_enabled: config.is_enabled(crate::Feature::Spawn),
        };

        for item in input {
            match item {
                ReplayInput::Source(character) => {
                    state
                        .source
                        .append([character])
                        .map_err(ReplayError::Source)?;
                }
                ReplayInput::Archive(record) => state.apply_archive(record)?,
                ReplayInput::Compact(barrier) => state.apply_compact(barrier)?,
                ReplayInput::Usage(sample) => state.usage_samples.push(sample),
            }
        }
        state.finish(self.runtime_config.clone())
    }
}

impl PreparedReplay {
    pub fn into_runtime(self) -> crate::SamplingRuntime {
        self.candidate
    }

    #[cfg(test)]
    pub(crate) fn into_planner(self) -> crate::SamplingPlanner {
        self.candidate.into_planner()
    }
}

struct ReplayState {
    thread: ThreadNamespace,
    epoch: ContextEpoch,
    source: SourceLedger,
    parser: SpineCharParser,
    compiler: SpineCompiler,
    epoch_start_boundary: crate::BoundaryId,
    committed_source_cells: usize,
    previous_pre_boundary: Option<crate::BoundaryId>,
    previous_commit_id: Option<SamplingCommitId>,
    live_plan: Option<ContextPlanRecipe>,
    started: BTreeMap<SamplingAttemptId, SamplingStarted>,
    commits: BTreeMap<SamplingCommitId, RecordDigest>,
    attempt_ids: BTreeSet<SamplingAttemptId>,
    applied_commits: Vec<SamplingCommitId>,
    usage_samples: Vec<TokenUsageSample>,
    input_pressure: InputPressureState,
    next_projection_ordinal: u64,
    spawn_enabled: bool,
}

struct ReplayCommit {
    attempt_id: crate::SamplingAttemptId,
    started_record_digest: RecordDigest,
    commit_id: SamplingCommitId,
    epoch: ContextEpoch,
    previous_pre_boundary: Option<crate::BoundaryId>,
    pre_boundary: crate::BoundaryId,
    post_boundary: crate::BoundaryId,
    previous_commit_id: Option<SamplingCommitId>,
    input_tokens: Option<u64>,
    facts: Vec<crate::ExecutedSpineFact>,
    fact_sources: Vec<FactSourceBinding>,
    source_digest: RecordDigest,
    record_digest: RecordDigest,
}

impl From<SamplingCommit> for ReplayCommit {
    fn from(record: SamplingCommit) -> Self {
        let (facts, fact_sources) = record
            .executions
            .into_iter()
            .map(|execution| {
                let execution_id = execution.execution_id;
                (
                    crate::ExecutedSpineFact {
                        execution_id: execution_id.clone(),
                        ordinal: execution.ordinal,
                        origin: execution.origin,
                        operation: execution.operation,
                    },
                    FactSourceBinding {
                        execution_id,
                        start: execution.source_span.start,
                        end: execution.source_span.end,
                    },
                )
            })
            .unzip();
        Self {
            attempt_id: record.attempt_id,
            started_record_digest: record.started_record_digest,
            commit_id: record.commit_id,
            epoch: record.epoch,
            previous_pre_boundary: record.previous_pre_boundary,
            pre_boundary: record.pre_boundary,
            post_boundary: record.post_boundary,
            previous_commit_id: record.previous_commit_id,
            input_tokens: record.input_tokens,
            facts,
            fact_sources,
            source_digest: record.source_digest,
            record_digest: record.record_digest,
        }
    }
}

impl ReplayState {
    fn apply_archive(&mut self, record: SamplingArchiveRecord) -> Result<(), ReplayError> {
        record.validate().map_err(ReplayError::Archive)?;
        match record {
            SamplingArchiveRecord::SamplingStarted(record) => {
                self.attempt_ids.insert(record.attempt_id.clone());
                if record.attempt_id.thread() != &self.thread {
                    if record.previous_commit_id != self.previous_commit_id {
                        return Err(ReplayError::InvalidCommitChain);
                    }
                    self.started.clear();
                    let continuation = record.attempt_id.thread().clone();
                    self.source
                        .continue_in_namespace(continuation.clone(), self.committed_source_cells)
                        .map_err(ReplayError::Source)?;
                    self.thread = continuation;
                    self.epoch_start_boundary = self.source.current_boundary();
                    self.previous_pre_boundary = None;
                }
                if record.epoch != self.epoch
                    || record.previous_commit_id != self.previous_commit_id
                    || record.pre_boundary != self.source.current_boundary()
                    || record.source_digest != *self.source.digest()
                {
                    return Err(ReplayError::SamplingStartedMismatch);
                }
                match self.started.get(&record.attempt_id) {
                    Some(started) if started.record_digest == record.record_digest => Ok(()),
                    Some(_) => Err(ReplayError::ConflictingSamplingStarted),
                    None => {
                        self.started.insert(record.attempt_id.clone(), record);
                        Ok(())
                    }
                }
            }
            SamplingArchiveRecord::SamplingCommit(record) => self.apply_commit(record.into()),
        }
    }

    fn apply_commit(&mut self, record: ReplayCommit) -> Result<(), ReplayError> {
        if let Some(digest) = self.commits.get(&record.commit_id) {
            return if digest == &record.record_digest {
                Ok(())
            } else {
                Err(ReplayError::Archive(ArchiveError::ConflictingCommit {
                    commit_id: record.commit_id,
                }))
            };
        }
        if record.attempt_id.thread() != &self.thread {
            return Err(ReplayError::IdentityScopeMismatch);
        }
        if record.epoch != self.epoch {
            return Err(ReplayError::IdentityScopeMismatch);
        }
        let started = self
            .started
            .get(&record.attempt_id)
            .ok_or(ReplayError::CommitWithoutStarted)?;
        if started.epoch != record.epoch
            || started.pre_boundary != record.pre_boundary
            || started.previous_commit_id != record.previous_commit_id
            || started.record_digest != record.started_record_digest
        {
            return Err(ReplayError::SamplingStartedMismatch);
        }
        if record.previous_commit_id != self.previous_commit_id {
            return Err(ReplayError::InvalidCommitChain);
        }
        if record.previous_pre_boundary != self.previous_pre_boundary {
            return Err(ReplayError::InvalidPreBoundaryChain);
        }
        let snapshot = self.source.snapshot();
        if snapshot.digest() != &record.source_digest {
            return Err(ReplayError::SourceDigestMismatch);
        }
        if snapshot.last_boundary() != Some(&record.post_boundary) {
            return Err(ReplayError::PostBoundaryIsNotSourceTail);
        }

        let mut parser = self.parser.clone();
        let mut compiler = self.compiler.clone();
        reduce_sampling_delta(
            &snapshot,
            self.committed_source_cells,
            RawBoundary(record.post_boundary.ordinal()),
            &mut parser,
            &mut compiler,
            &record.facts,
            FactBindingMode::Verify(&record.fact_sources),
        )
        .map_err(map_sampling_delta_error)?;
        let plan = crate::planner::build_context_plan(
            &snapshot,
            compiler.projection(),
            compiler.trim_projection(),
            &parser.pending_boundaries(),
            self.live_plan.as_ref(),
            &mut self.next_projection_ordinal,
            self.spawn_enabled,
        )
        .map_err(ReplayError::Planner)?;
        self.input_pressure.apply_sampling(
            record.input_tokens,
            record.facts.iter().map(|fact| &fact.operation),
        );

        self.parser = parser;
        self.compiler = compiler;
        self.committed_source_cells = snapshot.cells().len();
        self.previous_pre_boundary = Some(record.pre_boundary.clone());
        self.previous_commit_id = Some(record.commit_id.clone());
        self.live_plan = Some(plan);
        self.started.clear();
        self.commits
            .insert(record.commit_id.clone(), record.record_digest);
        self.applied_commits.push(record.commit_id);
        Ok(())
    }

    fn apply_compact(&mut self, barrier: SpineCompactBarrierV1) -> Result<(), ReplayError> {
        barrier.validate()?;
        if barrier.thread != self.thread
            || barrier.previous_epoch != self.epoch
            || barrier.next_epoch != self.epoch.checked_next().unwrap_or(self.epoch)
        {
            return Err(ReplayError::IdentityScopeMismatch);
        }
        let snapshot = self.source.snapshot();
        reduce_compact_delta(
            &snapshot,
            self.committed_source_cells,
            &barrier,
            &mut self.parser,
            &mut self.compiler,
        )
        .map_err(map_sampling_delta_error)?;
        self.source
            .advance_epoch(barrier.next_epoch)
            .map_err(ReplayError::Source)?;
        self.source
            .append(
                barrier
                    .replacement_boundaries
                    .iter()
                    .copied()
                    .map(|boundary| SpineChar::Opaque { boundary }),
            )
            .map_err(ReplayError::Source)?;
        self.epoch = barrier.next_epoch;
        self.epoch_start_boundary = self.source.current_boundary();
        self.committed_source_cells = self.source.snapshot().cells().len();
        self.previous_pre_boundary = None;
        self.started.clear();
        self.input_pressure.compact();
        self.next_projection_ordinal = 0;
        let snapshot = self.source.snapshot();
        self.live_plan = Some(
            crate::planner::build_context_plan(
                &snapshot,
                self.compiler.projection(),
                self.compiler.trim_projection(),
                &self.parser.pending_boundaries(),
                None,
                &mut self.next_projection_ordinal,
                self.spawn_enabled,
            )
            .map_err(ReplayError::Planner)?,
        );
        Ok(())
    }

    fn finish(mut self, runtime_config: SpineConfig) -> Result<PreparedReplay, ReplayError> {
        let snapshot = self.source.snapshot();
        let has_pending_source = self.committed_source_cells != snapshot.cells().len();
        if has_pending_source && self.started.is_empty() {
            return Err(ReplayError::CompactWhileSourceIsPending);
        }
        let projection = self.compiler.projection().clone();
        let tree = tree_snapshot(&projection, &self.usage_samples);
        let committed_plan = self.live_plan.take();
        let planner = crate::SamplingPlanner::from_replay_state(
            RecoveredPlannerState {
                thread: self.thread.clone(),
                epoch: self.epoch,
                source: self.source,
                epoch_start_boundary: self.epoch_start_boundary,
                parser: self.parser,
                compiler: self.compiler,
                committed_source_cells: self.committed_source_cells,
                previous_pre_boundary: self.previous_pre_boundary,
                previous_commit_id: self.previous_commit_id,
                committed_plan: committed_plan.clone(),
                input_pressure: self.input_pressure,
            },
            runtime_config,
        )
        .map_err(ReplayError::Planner)?;
        let live_plan = if has_pending_source {
            Some(
                planner
                    .preview_context_plan()
                    .map_err(ReplayError::Planner)?,
            )
        } else {
            committed_plan
        };
        let (live_context, memory_slots) = match &live_plan {
            Some(plan) => {
                let resolved = plan.resolve(&snapshot).map_err(ReplayError::ContextPlan)?;
                verify_memory_slots(&projection, &resolved)?;
                (
                    resolved.cells.into_iter().map(|cell| cell.item).collect(),
                    resolved.memory_slots,
                )
            }
            None => (
                projection.visible_context.clone(),
                projection_memory_slots(&projection),
            ),
        };
        let next_attempt = u64::try_from(
            self.attempt_ids
                .iter()
                .filter(|attempt| attempt.thread() == &self.thread)
                .count(),
        )
        .unwrap_or(u64::MAX);
        let next_commit = u64::try_from(
            self.commits
                .keys()
                .filter(|commit| commit.thread() == &self.thread)
                .count(),
        )
        .unwrap_or(u64::MAX);
        let candidate = crate::SamplingRuntime::from_replay(planner, next_attempt, next_commit);
        Ok(PreparedReplay {
            projection,
            live_context,
            live_plan,
            memory_slots,
            usage_samples: self.usage_samples,
            tree,
            applied_commits: self.applied_commits,
            final_epoch: self.epoch,
            candidate,
        })
    }
}

fn map_sampling_delta_error(error: SamplingDeltaError) -> ReplayError {
    match error {
        SamplingDeltaError::Parse(error) => ReplayError::Parse(error),
        SamplingDeltaError::Compile(error) => map_compile_error(error),
        SamplingDeltaError::MissingSourceBoundary(_)
        | SamplingDeltaError::MissingTrimSource(_)
        | SamplingDeltaError::FactHasNoSourceGroup(_)
        | SamplingDeltaError::FactSourceExecutionMismatch => ReplayError::FactSourceMissing,
        SamplingDeltaError::FactSourceAppliedMoreThanOnce => {
            ReplayError::FactSourceAppliedMoreThanOnce
        }
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialize(error) => write!(formatter, "failed to initialize replay: {error}"),
            Self::Archive(error) => write!(formatter, "invalid replay archive: {error}"),
            Self::Source(error) => write!(formatter, "invalid replay source: {error}"),
            Self::Parse(error) => write!(formatter, "failed to parse replay source: {error}"),
            Self::Compile(error) => write!(formatter, "failed to compile replay source: {error}"),
            Self::Transition(error) => write!(formatter, "invalid replay transition: {error}"),
            Self::ContextPlan(error) => write!(formatter, "invalid replay context plan: {error}"),
            Self::Planner(error) => write!(formatter, "failed to restore replay runtime: {error}"),
            Self::CommitWithoutStarted => {
                formatter.write_str("canonical commit has no matching sampling-started record")
            }
            Self::ConflictingSamplingStarted => {
                formatter.write_str("sampling-started record conflicts with a prior record")
            }
            Self::SamplingStartedMismatch => {
                formatter.write_str("sampling commit does not match its sampling-started record")
            }
            Self::IdentityScopeMismatch => {
                formatter.write_str("replay input belongs to another thread or epoch")
            }
            Self::InvalidCommitChain => {
                formatter.write_str("replay commit chain is not contiguous")
            }
            Self::InvalidPreBoundaryChain => {
                formatter.write_str("replay pre-boundary chain is not contiguous")
            }
            Self::PostBoundaryIsNotSourceTail => {
                formatter.write_str("replay commit post-boundary is not the source tail")
            }
            Self::SourceDigestMismatch => {
                formatter.write_str("replay source digest does not match the commit")
            }
            Self::FactSourceMissing => {
                formatter.write_str("replay fact has no matching stable source span")
            }
            Self::FactSourceAppliedMoreThanOnce => {
                formatter.write_str("replay fact source span was applied more than once")
            }
            Self::MemorySlotMismatch => {
                formatter.write_str("replayed memory differs from the durable memory sequence")
            }
            Self::CompactWhileSourceIsPending => {
                formatter.write_str("compact or replay end has uncommitted source cells")
            }
            Self::InvalidCompactBarrier(reason) => {
                write!(formatter, "invalid compact barrier: {reason}")
            }
            Self::Serialize(error) => write!(formatter, "failed to encode replay input: {error}"),
        }
    }
}

impl std::error::Error for ReplayError {}

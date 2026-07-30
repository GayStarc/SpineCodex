use super::SamplingPlanner;
use crate::BoundaryId;
use crate::ContextEpoch;
use crate::ContextPlanRecipe;
use crate::RecordDigest;
use crate::SamplingCommit;
use crate::SamplingCommitId;
use crate::SourceLedger;
use crate::SpineCharParser;
use crate::SpineCompactBarrierV1;
use crate::SpineCompiler;
use crate::SpineProjection;
use crate::ThreadNamespace;

pub struct PreparedSamplingCommit {
    pub(super) record: SamplingCommit,
    pub(super) plan: ContextPlanRecipe,
    pub(super) projection: SpineProjection,
    pub(super) candidate: CandidatePlannerState,
}

impl PreparedSamplingCommit {
    pub fn durable_record(&self) -> &SamplingCommit {
        &self.record
    }

    pub fn context_plan(&self) -> &ContextPlanRecipe {
        &self.plan
    }

    pub fn projection(&self) -> &SpineProjection {
        &self.projection
    }
}

pub struct PreparedCompactBarrier {
    pub(super) barrier: SpineCompactBarrierV1,
    pub(super) base_source_digest: RecordDigest,
    pub(super) candidate: SamplingPlanner,
}

pub(crate) struct RecoveredPlannerState {
    pub thread: ThreadNamespace,
    pub epoch: ContextEpoch,
    pub source: SourceLedger,
    pub epoch_start_boundary: BoundaryId,
    pub parser: SpineCharParser,
    pub compiler: SpineCompiler,
    pub committed_source_cells: usize,
    pub previous_pre_boundary: Option<BoundaryId>,
    pub previous_commit_id: Option<SamplingCommitId>,
    pub committed_plan: Option<ContextPlanRecipe>,
}

pub(super) struct CandidatePlannerState {
    pub(super) base_commit_id: Option<SamplingCommitId>,
    pub(super) base_source_cells: usize,
    pub(super) parser: SpineCharParser,
    pub(super) compiler: SpineCompiler,
    pub(super) committed_source_cells: usize,
    pub(super) previous_pre_boundary: Option<BoundaryId>,
    pub(super) previous_commit_id: Option<SamplingCommitId>,
    pub(super) next_projection_ordinal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamplingCommitOutput {
    pub record: SamplingCommit,
    pub plan: ContextPlanRecipe,
    pub projection: SpineProjection,
}

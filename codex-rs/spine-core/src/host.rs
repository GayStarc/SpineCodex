use crate::ContextEdit;
use crate::ContextTransition;
use crate::NativeItemRef;
use crate::RawBoundary;
use crate::RolloutEvent;
use crate::SpineCompiler;
use crate::SpineConfig;
use crate::SpineError;
use crate::SpineEventHandlers;
use crate::SpineObserverCause;
use crate::SpineObserverEvent;
use crate::SpineProjection;
use crate::SpineTransitionEvent;
use crate::TokenUsageSample;
use crate::ToolCatalog;
use crate::TrimProjection;
use crate::bootstrap::InitError;
use std::fmt;

/// Adapts an ordered native input stream into host-neutral Spine events.
///
/// Since `spine-core` 0.2, implementations own only incremental ingestion and
/// their frontier. Native rollout persistence and rendered model context stay
/// with the host. [`SpineRuntime`] publishes typed transitions to registered
/// handlers, which update the short-lived mutable history supplied to `eat`
/// or `replay` before the runtime commits.
pub trait SpineHost {
    type Input;
    type Frontier;
    type Error: std::error::Error;

    fn initial_frontier(&self) -> Self::Frontier;

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostStep<F> {
    frontier: F,
    events: Vec<RolloutEvent>,
    pending: Vec<NativeItemRef>,
    observed_boundary: Option<RawBoundary>,
    usage_sample: Option<TokenUsageSample>,
}

impl<F> HostStep<F> {
    pub fn new(
        frontier: F,
        events: Vec<RolloutEvent>,
        pending: Vec<NativeItemRef>,
        observed_boundary: Option<RawBoundary>,
    ) -> Self {
        Self {
            frontier,
            events,
            pending,
            observed_boundary,
            usage_sample: None,
        }
    }

    pub fn with_usage_sample(mut self, sample: TokenUsageSample) -> Self {
        self.usage_sample = Some(sample);
        self
    }

    pub fn usage_sample(&self) -> Option<TokenUsageSample> {
        self.usage_sample
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeProjection {
    spine: SpineProjection,
    pending: Vec<NativeItemRef>,
    observed_boundary: Option<RawBoundary>,
    usage_samples: Vec<TokenUsageSample>,
    trim_projection: Option<TrimProjection>,
    trim_changed_boundaries: Vec<RawBoundary>,
}

impl RuntimeProjection {
    pub fn spine(&self) -> &SpineProjection {
        &self.spine
    }

    pub fn pending(&self) -> &[NativeItemRef] {
        &self.pending
    }

    pub fn observed_boundary(&self) -> Option<RawBoundary> {
        self.observed_boundary
    }

    pub fn usage_samples(&self) -> &[TokenUsageSample] {
        &self.usage_samples
    }

    pub fn trim_projection(&self) -> Option<&TrimProjection> {
        self.trim_projection.as_ref()
    }

    pub fn trim_changed_boundaries(&self) -> &[RawBoundary] {
        &self.trim_changed_boundaries
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineOutput {
    context_edit: ContextEdit,
    runtime_projection: RuntimeProjection,
}

impl SpineOutput {
    pub fn context_edit(&self) -> &ContextEdit {
        &self.context_edit
    }

    pub fn runtime_projection(&self) -> &RuntimeProjection {
        &self.runtime_projection
    }
}

pub struct SpineRuntime<H, D>
where
    H: SpineHost,
    D: SpineEventHandlers<H::Frontier, Error = H::Error>,
{
    host: H,
    handlers: D,
    frontier: Option<H::Frontier>,
    compiler: SpineCompiler,
    runtime_projection: RuntimeProjection,
    tools: ToolCatalog,
}

impl<H, D> SpineRuntime<H, D>
where
    H: SpineHost,
    D: SpineEventHandlers<H::Frontier, Error = H::Error>,
{
    pub fn new(config: SpineConfig, host: H, handlers: D) -> Result<Self, InitError> {
        let tools = ToolCatalog::new(&config)?;
        let compiler = SpineCompiler::new(config)?;
        let active = !compiler.config_is_feature_off();
        let cardinality = handlers.cardinality();
        if active && !cardinality.is_valid() {
            return Err(InitError::InvalidHandlerCardinality {
                context_owners: cardinality.context_owners,
                observers: cardinality.observers,
            });
        }
        let frontier = active.then(|| host.initial_frontier());
        let runtime_projection = RuntimeProjection {
            spine: compiler.projection().clone(),
            pending: Vec::new(),
            observed_boundary: None,
            usage_samples: Vec::new(),
            trim_projection: compiler.trim_projection().cloned(),
            trim_changed_boundaries: Vec::new(),
        };
        Ok(Self {
            host,
            handlers,
            frontier,
            compiler,
            runtime_projection,
            tools,
        })
    }

    pub fn eat(
        &mut self,
        input: &H::Input,
        history: &mut D::History,
    ) -> Result<SpineOutput, RuntimeError<H::Error>> {
        if self.compiler.config_is_feature_off() {
            let projection = self.compiler.projection().clone();
            let context_edit =
                ContextEdit::between(&projection.visible_context, &projection.visible_context);
            return Ok(SpineOutput {
                context_edit,
                runtime_projection: self.runtime_projection.clone(),
            });
        }

        let Some(frontier) = self.frontier.as_ref() else {
            return Err(RuntimeError::Invariant(
                "active Spine runtime has no host frontier",
            ));
        };
        let step = self
            .host
            .ingest(frontier, input)
            .map_err(RuntimeError::Host)?;
        let usage_sample = step.usage_sample();
        let before = self.compiler.projection().visible_context.clone();
        let previous_trim = self.compiler.trim_projection().cloned();
        let mut candidate = self.compiler.clone();
        for event in step.events {
            candidate.eat(event).map_err(RuntimeError::Spine)?;
        }
        let projection = candidate.projection().clone();
        let mut usage_samples = self.runtime_projection.usage_samples.clone();
        if let Some(sample) = usage_sample.filter(|sample| sample.input_tokens > 0) {
            usage_samples.push(sample);
        }
        retain_relevant_usage_samples(&projection, &mut usage_samples);
        let trim_projection = candidate.trim_projection().cloned();
        let trim_changed_boundaries = match (&previous_trim, &trim_projection) {
            (Some(previous), Some(current)) => current.changed_boundaries_since(previous),
            (None, Some(current)) => current.changed_boundaries_since(&TrimProjection::default()),
            (Some(previous), None) => TrimProjection::default().changed_boundaries_since(previous),
            (None, None) => Vec::new(),
        };
        let context_edit = ContextEdit::between(&before, &projection.visible_context);
        let runtime_projection = RuntimeProjection {
            spine: projection,
            pending: step.pending,
            observed_boundary: step.observed_boundary,
            usage_samples,
            trim_projection,
            trim_changed_boundaries,
        };

        let transition = if context_edit.start == before.len() && context_edit.delete == 0 {
            ContextTransition::Append(&context_edit.insert)
        } else {
            ContextTransition::ContextEpochReset(&runtime_projection.spine.visible_context)
        };
        let event = SpineTransitionEvent {
            transition,
            frontier: &step.frontier,
            runtime_projection: &runtime_projection,
        };
        let prepared = self
            .handlers
            .prepare_context(history, event)
            .map_err(RuntimeError::Host)?;
        self.frontier = Some(step.frontier);
        self.compiler = candidate;
        self.runtime_projection = runtime_projection.clone();
        self.handlers.commit_context(history, prepared);
        let frontier = self
            .frontier
            .as_ref()
            .expect("active committed Spine runtime must have a host frontier");
        self.handlers.notify_observers(SpineObserverEvent {
            cause: SpineObserverCause::Live,
            frontier,
            runtime_projection: &self.runtime_projection,
        });
        Ok(SpineOutput {
            context_edit,
            runtime_projection,
        })
    }

    fn empty_runtime_state(&self) -> (Option<H::Frontier>, SpineCompiler, RuntimeProjection) {
        let mut compiler = self.compiler.clone();
        compiler.reset();
        let frontier = (!compiler.config_is_feature_off()).then(|| self.host.initial_frontier());
        let runtime_projection = RuntimeProjection {
            spine: compiler.projection().clone(),
            pending: Vec::new(),
            observed_boundary: None,
            usage_samples: Vec::new(),
            trim_projection: compiler.trim_projection().cloned(),
            trim_changed_boundaries: Vec::new(),
        };
        (frontier, compiler, runtime_projection)
    }

    pub fn projection(&self) -> &SpineProjection {
        self.compiler.projection()
    }

    pub fn runtime_projection(&self) -> &RuntimeProjection {
        &self.runtime_projection
    }

    pub fn replay<'a, I>(
        &mut self,
        inputs: I,
        history: &mut D::History,
    ) -> Result<SpineOutput, RuntimeError<H::Error>>
    where
        I: IntoIterator<Item = &'a H::Input>,
        H::Input: 'a,
    {
        let before = self.compiler.projection().visible_context.clone();
        let previous_trim = self.runtime_projection.trim_projection.clone();
        let (mut frontier, mut compiler, mut runtime_projection) = self.empty_runtime_state();
        for input in inputs {
            let Some(current_frontier) = frontier.as_ref() else {
                continue;
            };
            let step = self
                .host
                .ingest(current_frontier, input)
                .map_err(RuntimeError::Host)?;
            let usage_sample = step.usage_sample();
            let previous_step_trim = runtime_projection.trim_projection.clone();
            let mut candidate = compiler.clone();
            for event in step.events {
                candidate.eat(event).map_err(RuntimeError::Spine)?;
            }
            let projection = candidate.projection().clone();
            let mut usage_samples = runtime_projection.usage_samples;
            if let Some(sample) = usage_sample.filter(|sample| sample.input_tokens > 0) {
                usage_samples.push(sample);
            }
            retain_relevant_usage_samples(&projection, &mut usage_samples);
            let trim_projection = candidate.trim_projection().cloned();
            let trim_changed_boundaries =
                changed_trim_boundaries(previous_step_trim.as_ref(), trim_projection.as_ref());
            frontier = Some(step.frontier);
            compiler = candidate;
            runtime_projection = RuntimeProjection {
                spine: projection,
                pending: step.pending,
                observed_boundary: step.observed_boundary,
                usage_samples,
                trim_projection,
                trim_changed_boundaries,
            };
        }
        let context_edit = ContextEdit::between(&before, &runtime_projection.spine.visible_context);
        runtime_projection.trim_changed_boundaries = changed_trim_boundaries(
            previous_trim.as_ref(),
            runtime_projection.trim_projection.as_ref(),
        );
        let prepared = if let Some(final_frontier) = frontier.as_ref() {
            let event = SpineTransitionEvent {
                transition: ContextTransition::ContextEpochReset(
                    &runtime_projection.spine.visible_context,
                ),
                frontier: final_frontier,
                runtime_projection: &runtime_projection,
            };
            Some(
                self.handlers
                    .prepare_context(history, event)
                    .map_err(RuntimeError::Host)?,
            )
        } else {
            None
        };
        self.frontier = frontier;
        self.compiler = compiler;
        self.runtime_projection = runtime_projection.clone();
        if let Some(prepared) = prepared.into_iter().next() {
            self.handlers.commit_context(history, prepared);
        }
        if let Some(frontier) = self.frontier.as_ref() {
            self.handlers.notify_observers(SpineObserverEvent {
                cause: SpineObserverCause::Replay,
                frontier,
                runtime_projection: &self.runtime_projection,
            });
        }
        Ok(SpineOutput {
            context_edit,
            runtime_projection,
        })
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn handlers(&self) -> &D {
        &self.handlers
    }

    pub fn handlers_mut(&mut self) -> &mut D {
        &mut self.handlers
    }

    pub fn frontier(&self) -> Option<&H::Frontier> {
        self.frontier.as_ref()
    }

    pub fn tools(&self) -> &ToolCatalog {
        &self.tools
    }

    pub fn extend_system_prompt(&self, base: &str) -> String {
        self.compiler.extend_system_prompt(base)
    }
}

fn retain_relevant_usage_samples(
    projection: &SpineProjection,
    samples: &mut Vec<TokenUsageSample>,
) {
    let mut retain = vec![false; samples.len()];
    for node in projection.nodes.iter().filter(|node| {
        matches!(
            node.status,
            crate::NodeStatus::Live | crate::NodeStatus::Opened
        )
    }) {
        if let Some(index) = samples
            .iter()
            .position(|sample| sample.boundary.0 > node.start.0)
        {
            retain[index] = true;
        }
    }
    if let Some(last) = retain.last_mut() {
        *last = true;
    }
    let mut index = 0;
    samples.retain(|_| {
        let keep = retain[index];
        index += 1;
        keep
    });
}

fn changed_trim_boundaries(
    previous: Option<&TrimProjection>,
    current: Option<&TrimProjection>,
) -> Vec<RawBoundary> {
    match (previous, current) {
        (Some(previous), Some(current)) => current.changed_boundaries_since(previous),
        (None, Some(current)) => current.changed_boundaries_since(&TrimProjection::default()),
        (Some(previous), None) => TrimProjection::default().changed_boundaries_since(previous),
        (None, None) => Vec::new(),
    }
}

#[derive(Debug)]
pub enum RuntimeError<E> {
    Spine(SpineError),
    Host(E),
    Invariant(&'static str),
}

impl<E: fmt::Display> fmt::Display for RuntimeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spine(error) => error.fmt(formatter),
            Self::Host(error) => error.fmt(formatter),
            Self::Invariant(message) => formatter.write_str(message),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for RuntimeError<E> {}

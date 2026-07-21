use crate::ContextEdit;
use crate::NativeItemRef;
use crate::ProjectionDelta;
use crate::RawBoundary;
use crate::RolloutEvent;
use crate::SpineCompiler;
use crate::SpineConfig;
use crate::SpineError;
use crate::SpineProjection;
use crate::TokenUsageSample;
use crate::ToolCatalog;
use crate::bootstrap::InitError;
use std::fmt;

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
    usage_sample: Option<TokenUsageSample>,
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

    pub fn usage_sample(&self) -> Option<TokenUsageSample> {
        self.usage_sample
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineOutput {
    delta: ProjectionDelta,
    runtime_projection: RuntimeProjection,
}

impl SpineOutput {
    pub fn delta(&self) -> &ProjectionDelta {
        &self.delta
    }

    pub fn runtime_projection(&self) -> &RuntimeProjection {
        &self.runtime_projection
    }
}

pub struct SpineRuntime<H: SpineHost> {
    host: H,
    frontier: Option<H::Frontier>,
    compiler: SpineCompiler,
    runtime_projection: RuntimeProjection,
    tools: ToolCatalog,
}

impl<H: SpineHost> SpineRuntime<H> {
    pub fn new(config: SpineConfig, host: H) -> Result<Self, InitError> {
        let tools = ToolCatalog::new(&config)?;
        let compiler = SpineCompiler::new(config)?;
        let active = !compiler.config_is_feature_off();
        let frontier = active.then(|| host.initial_frontier());
        let runtime_projection = RuntimeProjection {
            spine: compiler.projection().clone(),
            pending: Vec::new(),
            observed_boundary: None,
            usage_sample: None,
        };
        Ok(Self {
            host,
            frontier,
            compiler,
            runtime_projection,
            tools,
        })
    }

    pub fn eat(&mut self, input: &H::Input) -> Result<SpineOutput, RuntimeError<H::Error>> {
        if self.compiler.config_is_feature_off() {
            let projection = self.compiler.projection().clone();
            let delta = ProjectionDelta {
                context_edit: ContextEdit::between(
                    &projection.visible_context,
                    &projection.visible_context,
                ),
                projection,
            };
            return Ok(SpineOutput {
                delta,
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
        let mut candidate = self.compiler.clone();
        for event in step.events {
            candidate.eat(event).map_err(RuntimeError::Spine)?;
        }
        let projection = candidate.projection().clone();
        let delta = ProjectionDelta {
            context_edit: ContextEdit::between(&before, &projection.visible_context),
            projection: projection.clone(),
        };
        let runtime_projection = RuntimeProjection {
            spine: projection,
            pending: step.pending,
            observed_boundary: step.observed_boundary,
            usage_sample,
        };

        self.frontier = Some(step.frontier);
        self.compiler = candidate;
        self.runtime_projection = runtime_projection.clone();
        Ok(SpineOutput {
            delta,
            runtime_projection,
        })
    }

    pub fn reset(&mut self) {
        self.compiler.reset();
        self.frontier =
            (!self.compiler.config_is_feature_off()).then(|| self.host.initial_frontier());
        self.runtime_projection = RuntimeProjection {
            spine: self.compiler.projection().clone(),
            pending: Vec::new(),
            observed_boundary: None,
            usage_sample: None,
        };
    }

    pub fn projection(&self) -> &SpineProjection {
        self.compiler.projection()
    }

    pub fn replay<'a, I>(&mut self, inputs: I) -> Result<SpineOutput, RuntimeError<H::Error>>
    where
        I: IntoIterator<Item = &'a H::Input>,
        H::Input: 'a,
    {
        self.reset();
        let mut output = None;
        for input in inputs {
            output = Some(self.eat(input)?);
        }
        Ok(output.unwrap_or_else(|| self.output()))
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn tools(&self) -> &ToolCatalog {
        &self.tools
    }

    pub fn extend_system_prompt(&self, base: &str) -> String {
        self.compiler.extend_system_prompt(base)
    }

    fn output(&self) -> SpineOutput {
        let projection = self.compiler.projection().clone();
        SpineOutput {
            delta: ProjectionDelta {
                context_edit: ContextEdit::between(
                    &projection.visible_context,
                    &projection.visible_context,
                ),
                projection,
            },
            runtime_projection: self.runtime_projection.clone(),
        }
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

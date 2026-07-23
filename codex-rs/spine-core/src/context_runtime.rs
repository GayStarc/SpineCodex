mod projection;

use self::projection::context_events_between;
use self::projection::project_jit_stack;
use self::projection::project_trim_stack;
use crate::CharParseError;
use crate::ContextEvent;
use crate::ContextEventError;
use crate::Feature;
use crate::ParseStack;
use crate::RawBoundary;
use crate::SpineChar;
use crate::SpineCharParser;
use crate::SpineCompiler;
use crate::SpineConfig;
use crate::SpineContextEventHandler;
use crate::SpineError;
use crate::SpineProjection;
use crate::TokenUsageSample;
use crate::ToolCatalog;
use crate::TrimProjection;
use crate::bootstrap::InitError;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineContextProjection {
    spine: SpineProjection,
    stack: ParseStack,
    usage_samples: Vec<TokenUsageSample>,
    trim_projection: Option<TrimProjection>,
}

impl SpineContextProjection {
    pub fn spine(&self) -> &SpineProjection {
        &self.spine
    }

    pub fn stack(&self) -> &ParseStack {
        &self.stack
    }

    pub fn usage_samples(&self) -> &[TokenUsageSample] {
        &self.usage_samples
    }

    pub fn trim_projection(&self) -> Option<&TrimProjection> {
        self.trim_projection.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineContextOutput {
    events: Vec<ContextEvent>,
    projection: SpineContextProjection,
}

impl SpineContextOutput {
    pub fn events(&self) -> &[ContextEvent] {
        &self.events
    }

    pub fn projection(&self) -> &SpineContextProjection {
        &self.projection
    }
}

/// Compiles a live host context from one-cell [`SpineChar`] inputs.
///
/// The host appends native items before calling [`Self::append`]. The runtime
/// verifies the appended size, prepares all parsing and context mutations on
/// cloned state, then synchronously commits through the registered context
/// handler.
pub struct SpineContextRuntime<D>
where
    D: SpineContextEventHandler,
{
    handler: D,
    parser: SpineCharParser,
    compiler: SpineCompiler,
    projection: SpineContextProjection,
    tools: ToolCatalog,
    jit_enabled: bool,
    spawn_enabled: bool,
}

impl<D> SpineContextRuntime<D>
where
    D: SpineContextEventHandler,
{
    pub fn new(config: SpineConfig, handler: D) -> Result<Self, InitError> {
        let tools = ToolCatalog::new(&config)?;
        let jit_enabled = config.is_enabled(Feature::Jit);
        let spawn_enabled = config.is_enabled(Feature::Spawn);
        let compiler = SpineCompiler::new(config)?;
        let parser = SpineCharParser::default();
        let projection = SpineContextProjection {
            spine: compiler.projection().clone(),
            stack: parser.stack().clone(),
            usage_samples: Vec::new(),
            trim_projection: compiler.trim_projection().cloned(),
        };
        Ok(Self {
            handler,
            parser,
            compiler,
            projection,
            tools,
            jit_enabled,
            spawn_enabled,
        })
    }

    pub fn append<I>(
        &mut self,
        characters: I,
        history: &mut D::History,
    ) -> Result<SpineContextOutput, SpineContextRuntimeError<D::Error>>
    where
        I: IntoIterator<Item = SpineChar>,
    {
        let characters = characters.into_iter().collect::<Vec<_>>();
        let expected_before = self.parser.stack().len().saturating_add(characters.len());
        let actual_before = self.handler.context_size(history);
        if actual_before != expected_before {
            return Err(SpineContextRuntimeError::ContextSizeMismatch {
                phase: ContextSizePhase::BeforePrepare,
                expected: expected_before,
                actual: actual_before,
            });
        }

        let mut parser = self.parser.clone();
        let mut compiler = self.compiler.clone();
        let mut pending_boundaries = parser.pending_boundaries();
        for character in characters {
            let step = parser
                .eat(character)
                .map_err(SpineContextRuntimeError::Parse)?;
            pending_boundaries = step.pending_boundaries().to_vec();
            for event in step.events().iter().cloned() {
                compiler
                    .eat(event)
                    .map_err(SpineContextRuntimeError::Spine)?;
            }
        }

        let raw_stack = parser.stack().clone();
        let target_stack = if self.jit_enabled {
            project_jit_stack::<D::Error>(
                &mut parser,
                compiler.projection(),
                compiler.trim_projection(),
                &pending_boundaries,
                self.spawn_enabled,
            )?
        } else {
            project_trim_stack(parser.stack(), compiler.trim_projection())
        };
        let events = context_events_between(&raw_stack, &target_stack)?;
        let expected_after = ContextEvent::resulting_size(actual_before, &events)
            .map_err(SpineContextRuntimeError::ContextEvent)?;
        if expected_after != target_stack.len() {
            return Err(SpineContextRuntimeError::ContextSizeMismatch {
                phase: ContextSizePhase::BeforeCommit,
                expected: target_stack.len(),
                actual: expected_after,
            });
        }

        let prepared = self
            .handler
            .prepare_context(history, &target_stack, &events)
            .map_err(SpineContextRuntimeError::Handler)?;
        parser.replace_stack(target_stack.clone());
        let projection = SpineContextProjection {
            spine: compiler.projection().clone(),
            stack: target_stack,
            usage_samples: self.projection.usage_samples.clone(),
            trim_projection: compiler.trim_projection().cloned(),
        };
        self.parser = parser;
        self.compiler = compiler;
        self.projection = projection.clone();
        self.handler.commit_context(history, prepared);
        assert_eq!(
            self.handler.context_size(history),
            self.parser.stack().len(),
            "committed host context diverged from the Spine parse stack"
        );
        Ok(SpineContextOutput { events, projection })
    }

    pub fn observe_usage(&mut self, sample: TokenUsageSample) -> SpineContextOutput {
        if sample.input_tokens > 0 {
            self.projection.usage_samples.push(sample);
            retain_relevant_usage_samples(
                &self.projection.spine,
                &mut self.projection.usage_samples,
            );
        }
        SpineContextOutput {
            events: Vec::new(),
            projection: self.projection.clone(),
        }
    }

    pub fn projection(&self) -> &SpineContextProjection {
        &self.projection
    }

    pub fn handler(&self) -> &D {
        &self.handler
    }

    pub fn handler_mut(&mut self) -> &mut D {
        &mut self.handler
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSizePhase {
    BeforePrepare,
    BeforeCommit,
}

#[derive(Debug)]
pub enum SpineContextRuntimeError<E> {
    Parse(CharParseError),
    Spine(SpineError),
    ContextEvent(ContextEventError),
    Handler(E),
    ContextSizeMismatch {
        phase: ContextSizePhase,
        expected: usize,
        actual: usize,
    },
    MissingCell {
        boundary: RawBoundary,
    },
    ArchivedSourceInLiveContext,
}

impl<E: fmt::Display> fmt::Display for SpineContextRuntimeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(formatter),
            Self::Spine(error) => error.fmt(formatter),
            Self::ContextEvent(error) => write!(formatter, "{error:?}"),
            Self::Handler(error) => error.fmt(formatter),
            Self::ContextSizeMismatch {
                phase,
                expected,
                actual,
            } => write!(
                formatter,
                "Spine context size mismatch at {phase:?}: expected {expected}, found {actual}"
            ),
            Self::MissingCell { boundary } => {
                write!(formatter, "Spine projection has no cell at {}", boundary.0)
            }
            Self::ArchivedSourceInLiveContext => {
                formatter.write_str("archived compact source appeared in live context")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SpineContextRuntimeError<E> {}

#[cfg(test)]
#[path = "context_runtime_tests.rs"]
mod tests;

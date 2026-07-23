use crate::ContextEdit;
use crate::ProjectionDelta;
use crate::RawBoundary;
use crate::RolloutEvent;
use crate::SpineConfig;
use crate::SpineProjection;
use crate::TrimProjection;
use crate::bootstrap::InitError;
use crate::reducer::SpineReducer;
use crate::reducer::TrimReducer;
use std::fmt;

pub const MAX_RAW_EVENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_VISIBLE_CONTEXT_ITEMS: usize = 4096;
pub const MAX_SYNTHETIC_CONTEXT_BYTES: usize = 1024 * 1024;
pub const MAX_TREE_NODES: usize = 4096;

#[derive(Clone, Debug)]
pub struct SpineCompiler {
    config: SpineConfig,
    reducer: SpineReducer,
    trim_reducer: Option<TrimReducer>,
    projection: SpineProjection,
}

impl SpineCompiler {
    pub fn new(config: SpineConfig) -> Result<Self, InitError> {
        config.validate()?;
        let reducer = SpineReducer::new();
        let trim_reducer = config
            .is_enabled(crate::Feature::Trim)
            .then(|| TrimReducer::new(config.trim_threshold_bytes()));
        let projection = reducer.projection();
        Ok(Self {
            config,
            reducer,
            trim_reducer,
            projection,
        })
    }

    pub fn eat(&mut self, event: RolloutEvent) -> Result<ProjectionDelta, SpineError> {
        let retained_bytes = event.retained_bytes();
        if retained_bytes > MAX_RAW_EVENT_BYTES {
            return Err(SpineError::ContextLimit {
                kind: "raw event bytes",
                max: MAX_RAW_EVENT_BYTES,
                actual: retained_bytes,
            });
        }
        let boundary = event.boundary();
        if let Some(previous) = self.projection.last_boundary
            && boundary < previous
        {
            return Err(SpineError::NonMonotonicBoundary {
                previous,
                next: boundary,
            });
        }
        let mut trim_reducer = self.trim_reducer.clone();
        if let Some(trim_reducer) = &mut trim_reducer {
            trim_reducer.apply(&event);
        }
        let mut reducer = self.reducer.clone();
        let delta = reducer.apply(event);
        validate_projection(&delta.projection)?;
        self.reducer = reducer;
        self.trim_reducer = trim_reducer;
        self.projection = delta.projection.clone();
        Ok(delta)
    }

    pub fn replay<I>(&mut self, events: I) -> Result<ProjectionDelta, SpineError>
    where
        I: IntoIterator<Item = RolloutEvent>,
    {
        let before = self.projection().visible_context.clone();
        let mut candidate = self.clone();
        candidate.reset();
        for event in events {
            candidate.eat(event)?;
        }
        let projection = candidate.projection().clone();
        *self = candidate;
        Ok(ProjectionDelta {
            context_edit: ContextEdit::between(&before, &projection.visible_context),
            projection,
        })
    }

    pub fn reset(&mut self) {
        self.reducer = SpineReducer::new();
        self.trim_reducer = self
            .config
            .is_enabled(crate::Feature::Trim)
            .then(|| TrimReducer::new(self.config.trim_threshold_bytes()));
        self.projection = self.reducer.projection();
    }

    pub fn projection(&self) -> &SpineProjection {
        &self.projection
    }

    pub(crate) fn trim_projection(&self) -> Option<&TrimProjection> {
        self.trim_reducer.as_ref().map(TrimReducer::projection)
    }

    pub fn extend_system_prompt(&self, base: &str) -> String {
        crate::prompt::extend(base.to_owned(), &self.config)
    }

    pub(crate) fn config_is_feature_off(&self) -> bool {
        self.config.is_feature_off()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpineError {
    NonMonotonicBoundary {
        previous: RawBoundary,
        next: RawBoundary,
    },
    ContextLimit {
        kind: &'static str,
        max: usize,
        actual: usize,
    },
}

impl fmt::Display for SpineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonMonotonicBoundary { previous, next } => write!(
                formatter,
                "Spine event boundary {} precedes {}",
                next.0, previous.0
            ),
            Self::ContextLimit { kind, max, actual } => {
                write!(formatter, "Spine {kind} is {actual}; maximum is {max}")
            }
        }
    }
}

impl std::error::Error for SpineError {}

fn validate_projection(projection: &SpineProjection) -> Result<(), SpineError> {
    for (kind, actual, max) in [
        (
            "visible context items",
            projection.visible_context.len(),
            MAX_VISIBLE_CONTEXT_ITEMS,
        ),
        ("tree nodes", projection.nodes.len(), MAX_TREE_NODES),
        (
            "synthetic context bytes",
            projection
                .visible_context
                .iter()
                .map(crate::ContextItem::retained_synthetic_bytes)
                .fold(0usize, usize::saturating_add),
            MAX_SYNTHETIC_CONTEXT_BYTES,
        ),
    ] {
        if actual > max {
            return Err(SpineError::ContextLimit { kind, max, actual });
        }
    }
    Ok(())
}

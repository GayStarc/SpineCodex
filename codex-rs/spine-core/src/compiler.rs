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
        let boundary = event.boundary();
        if let Some(previous) = self.projection.last_boundary
            && boundary < previous
        {
            return Err(SpineError::NonMonotonicBoundary {
                previous,
                next: boundary,
            });
        }
        if let Some(trim_reducer) = &mut self.trim_reducer {
            trim_reducer.apply(&event);
        }
        let delta = self.reducer.apply(event);
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
}

impl fmt::Display for SpineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonMonotonicBoundary { previous, next } => write!(
                formatter,
                "Spine event boundary {} precedes {}",
                next.0, previous.0
            ),
        }
    }
}

impl std::error::Error for SpineError {}

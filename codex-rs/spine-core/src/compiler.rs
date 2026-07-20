use crate::ContextEdit;
use crate::ProjectionDelta;
use crate::RawBoundary;
use crate::RolloutEvent;
use crate::SpineConfig;
use crate::SpineProjection;
use crate::SpineRegistration;
use crate::bootstrap::InitError;
use crate::reducer::SpineReducer;
use std::fmt;

#[derive(Clone, Debug)]
pub struct SpineCompiler {
    _config: SpineConfig,
    registration: SpineRegistration,
    reducer: SpineReducer,
    projection: SpineProjection,
}

impl SpineCompiler {
    pub fn new(config: SpineConfig, registration: SpineRegistration) -> Result<Self, InitError> {
        if config.schema_version() != 1 {
            return Err(InitError::UnsupportedConfigVersion(config.schema_version()));
        }
        config.validate_registration(&registration)?;
        let reducer = SpineReducer::new();
        let projection = reducer.projection();
        Ok(Self {
            _config: config,
            registration,
            reducer,
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
        self.projection = self.reducer.projection();
    }

    pub fn projection(&self) -> &SpineProjection {
        &self.projection
    }

    pub fn extend_system_prompt(&self, base: &str) -> String {
        crate::prompt::extend(base.to_owned(), &self._config, &self.registration)
    }

    pub(crate) fn registration(&self) -> &SpineRegistration {
        &self.registration
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

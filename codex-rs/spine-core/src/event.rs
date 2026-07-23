use crate::ContextItem;
use crate::RuntimeProjection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandlerCardinality {
    pub context_owners: usize,
    pub observers: usize,
}

impl HandlerCardinality {
    pub(crate) fn is_valid(self) -> bool {
        self.context_owners == 1 && self.observers >= 1 && self.total() > 1
    }

    pub fn total(self) -> usize {
        self.context_owners.saturating_add(self.observers)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextTransition<'a> {
    Append(&'a [ContextItem]),
    ContextEpochReset(&'a [ContextItem]),
}

#[derive(Clone, Copy, Debug)]
pub struct SpineTransitionEvent<'a, F> {
    pub transition: ContextTransition<'a>,
    pub frontier: &'a F,
    pub runtime_projection: &'a RuntimeProjection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpineObserverCause {
    Live,
    Replay,
}

#[derive(Clone, Copy, Debug)]
pub struct SpineObserverEvent<'a, F> {
    pub cause: SpineObserverCause,
    pub frontier: &'a F,
    pub runtime_projection: &'a RuntimeProjection,
}

/// Registry of handlers for integrated Spine runtime transitions.
///
/// Implementations must expose exactly one authoritative context owner and at
/// least one observer. `prepare_context` may fail but must not mutate committed
/// handler state or the supplied history. Once preparation succeeds,
/// `commit_context` installs the prepared value and must be infallible.
/// `notify_observers` runs after the SDK and context owner commit; implementations
/// may queue effects but must not perform fallible external I/O.
pub trait SpineEventHandlers<F> {
    type History;
    type PreparedContext;
    type Error: std::error::Error;

    fn cardinality(&self) -> HandlerCardinality;

    fn prepare_context(
        &self,
        history: &Self::History,
        event: SpineTransitionEvent<'_, F>,
    ) -> Result<Self::PreparedContext, Self::Error>;

    fn commit_context(&mut self, history: &mut Self::History, prepared: Self::PreparedContext);

    fn notify_observers(&mut self, event: SpineObserverEvent<'_, F>);
}

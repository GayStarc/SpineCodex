use crate::CellId;
use crate::ContextItem;
use crate::TrimEdit;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextLabel {
    UserAnchor(u64),
    ToolOutput(TrimEdit),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextInsert {
    Existing {
        cell_id: CellId,
        source_index: usize,
    },
    Synthetic(ContextItem),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextEvent {
    Tag {
        index: usize,
        label: ContextLabel,
    },
    Splice {
        start: usize,
        delete: usize,
        insert: Vec<ContextInsert>,
    },
}

impl ContextEvent {
    pub fn resulting_size(
        initial_size: usize,
        events: &[Self],
    ) -> Result<usize, ContextEventError> {
        events
            .iter()
            .try_fold(initial_size, |size, event| match event {
                Self::Tag { index, .. } => {
                    if *index >= size {
                        return Err(ContextEventError::IndexOutOfBounds {
                            index: *index,
                            size,
                        });
                    }
                    Ok(size)
                }
                Self::Splice {
                    start,
                    delete,
                    insert,
                } => {
                    let end =
                        start
                            .checked_add(*delete)
                            .ok_or(ContextEventError::RangeOutOfBounds {
                                start: *start,
                                delete: *delete,
                                size,
                            })?;
                    if end > size {
                        return Err(ContextEventError::RangeOutOfBounds {
                            start: *start,
                            delete: *delete,
                            size,
                        });
                    }
                    Ok(size - *delete + insert.len())
                }
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextEventError {
    IndexOutOfBounds {
        index: usize,
        size: usize,
    },
    RangeOutOfBounds {
        start: usize,
        delete: usize,
        size: usize,
    },
}

/// Applies Spine's context events to one authoritative host context.
///
/// `prepare_context` must be side-effect free. Once it succeeds,
/// `commit_context` must be infallible. The runtime verifies event sizes before
/// preparation and checks that the committed context still matches its parse
/// stack.
pub trait SpineContextEventHandler {
    type History;
    type PreparedContext;
    type Error: std::error::Error;

    fn context_size(&self, history: &Self::History) -> usize;

    fn prepare_context(
        &self,
        history: &Self::History,
        events: &[ContextEvent],
    ) -> Result<Self::PreparedContext, Self::Error>;

    fn commit_context(&mut self, history: &mut Self::History, prepared: Self::PreparedContext);
}

#[cfg(test)]
#[path = "context_event_tests.rs"]
mod tests;

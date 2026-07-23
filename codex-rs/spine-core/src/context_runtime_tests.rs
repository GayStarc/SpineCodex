use super::*;
use crate::CellId;
use crate::ContextEvent;
use crate::ContextInsert;
use crate::ContextItem;
use crate::ContextLabel;
use crate::Feature;
use crate::Message;
use crate::MessageRole;
use crate::NodeId;
use crate::NodeStatus;
use crate::ParseCell;
use crate::SpineConfig;
use crate::SpineContextEventHandler;
use crate::SpineRecoveryInput;
use crate::SpineSignal;
use crate::ToolOutcome;
use crate::ToolRequestChar;
use crate::ToolResponseChar;
use pretty_assertions::assert_eq;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestError;

impl fmt::Display for TestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("test handler rejected context")
    }
}

impl std::error::Error for TestError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TestHistory {
    cells: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
struct TestHandler {
    reject: bool,
}

impl SpineContextEventHandler for TestHandler {
    type History = TestHistory;
    type PreparedContext = TestHistory;
    type Error = TestError;

    fn context_size(&self, history: &Self::History) -> usize {
        history.cells.len()
    }

    fn prepare_context(
        &self,
        history: &Self::History,
        stack: &ParseStack,
        events: &[ContextEvent],
    ) -> Result<Self::PreparedContext, Self::Error> {
        if self.reject {
            return Err(TestError);
        }
        let mut prepared = history.clone();
        for event in events {
            match event {
                ContextEvent::Tag { .. } => {}
                ContextEvent::Splice {
                    start,
                    delete,
                    insert,
                } => {
                    let values = insert
                        .iter()
                        .map(|insert| match insert {
                            ContextInsert::Existing { source_index, .. } => {
                                prepared.cells[*source_index]
                            }
                            ContextInsert::Synthetic { cell_id, .. } => cell_id.value(),
                        })
                        .collect::<Vec<_>>();
                    prepared.cells.splice(*start..start + delete, values);
                }
            }
        }
        assert_eq!(prepared.cells.len(), stack.len());
        Ok(prepared)
    }

    fn commit_context(&mut self, history: &mut Self::History, prepared: Self::PreparedContext) {
        *history = prepared;
    }
}

fn config(features: &[Feature]) -> SpineConfig {
    SpineConfig::v1()
        .with_features(features.iter().copied())
        .expect("valid test configuration")
}

fn message(boundary: u64, role: MessageRole, content: &str) -> SpineChar {
    SpineChar::Message(Message {
        boundary: RawBoundary(boundary),
        role,
        content: content.to_string(),
    })
}

fn request(boundary: u64, call_id: &str, name: &str, arguments: &str) -> SpineChar {
    SpineChar::ToolRequest(ToolRequestChar {
        boundary: RawBoundary(boundary),
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    })
}

fn response(boundary: u64, call_id: &str, output: &str) -> SpineChar {
    SpineChar::ToolResponse(ToolResponseChar {
        boundary: RawBoundary(boundary),
        call_id: call_id.to_string(),
        outcome: ToolOutcome::Succeeded,
        output: output.to_string(),
    })
}

#[test]
fn live_append_tags_user_and_preserves_one_cell_per_input() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();

    let output = runtime
        .append([message(1, MessageRole::User, "hello")], &mut history)
        .unwrap();

    assert_eq!(runtime.projection().stack().len(), 1);
    assert_eq!(history.cells.len(), 1);
    assert_eq!(
        output.events(),
        &[ContextEvent::Tag {
            index: 0,
            label: ContextLabel::UserAnchor(1),
        }]
    );
}

#[test]
fn pending_tool_group_stays_in_the_live_stack_until_completion() {
    let mut history = TestHistory { cells: vec![0, 1] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();

    runtime
        .append(
            [
                message(1, MessageRole::Assistant, "working"),
                request(2, "call", "shell", "{}"),
            ],
            &mut history,
        )
        .unwrap();
    assert_eq!(runtime.projection().stack().len(), 2);
    assert!(runtime.projection().spine().visible_context.is_empty());

    history.cells.push(2);
    runtime
        .append([response(3, "call", "done")], &mut history)
        .unwrap();
    assert_eq!(runtime.projection().stack().len(), 3);
    assert_eq!(history.cells.len(), 3);
}

#[test]
fn open_inserts_a_synthetic_cell_without_rebuilding_previous_cells() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();

    runtime
        .append([message(1, MessageRole::User, "hello")], &mut history)
        .unwrap();
    history.cells.extend([1, 2]);
    runtime
        .append(
            [
                request(2, "open", "spine.open", r#"{"summary":"task"}"#),
                response(3, "open", "accepted"),
            ],
            &mut history,
        )
        .unwrap();

    assert!(
        runtime
            .projection()
            .stack()
            .cells()
            .iter()
            .any(|cell| matches!(cell.character(), SpineChar::Synthetic { .. }))
    );
    assert!(
        runtime
            .projection()
            .spine()
            .visible_context
            .iter()
            .any(|item| matches!(item, ContextItem::SyntheticNode { .. }))
    );
}

#[test]
fn trim_labels_only_the_completed_tool_response_cell() {
    let mut history = TestHistory { cells: vec![0, 1] };
    let mut runtime = SpineContextRuntime::new(
        config(&[Feature::Jit, Feature::Trim]),
        TestHandler::default(),
    )
    .unwrap();

    let output = runtime
        .append(
            [
                request(1, "call", "shell", "{}"),
                response(2, "call", &"large".repeat(3_000)),
            ],
            &mut history,
        )
        .unwrap();

    assert!(matches!(
        output.events(),
        [ContextEvent::Tag {
            index: 1,
            label: ContextLabel::ToolOutput(crate::TrimEdit::Tagged { .. }),
        }]
    ));
    assert!(runtime.projection().stack().cells()[0].labels().is_empty());
    assert!(matches!(
        runtime.projection().stack().cells()[1].labels(),
        [ContextLabel::ToolOutput(crate::TrimEdit::Tagged { .. })]
    ));
}

#[test]
fn handler_rejection_does_not_commit_runtime_state() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();
    runtime
        .append([message(1, MessageRole::User, "before")], &mut history)
        .unwrap();
    let before_projection = runtime.projection().clone();
    let before_history = history.clone();
    runtime.handler_mut().reject = true;
    history.cells.push(99);

    let result = runtime.append([message(2, MessageRole::User, "after")], &mut history);

    assert!(matches!(
        result,
        Err(SpineContextRuntimeError::Handler(TestError))
    ));
    assert_eq!(runtime.projection(), &before_projection);
    assert_eq!(history, TestHistory { cells: vec![0, 99] });
    assert_ne!(history, before_history);
}

#[test]
fn label_reset_after_structural_splice_uses_original_source_index() {
    let first = ParseCell::new(CellId::new(0), message(1, MessageRole::User, "first"))
        .with_labels(vec![ContextLabel::UserAnchor(1)]);
    let removed = ParseCell::new(CellId::new(1), message(2, MessageRole::User, "removed"));
    let moved = ParseCell::new(CellId::new(2), message(3, MessageRole::User, "moved"))
        .with_labels(vec![ContextLabel::UserAnchor(2)]);
    let before = ParseStack::from_cells(vec![first.clone(), removed, moved.clone()]);
    let after = ParseStack::from_cells(vec![first, moved.with_labels(Vec::new())]);

    let events = context_events_between::<TestError>(&before, &after).unwrap();

    assert_eq!(
        events,
        vec![
            ContextEvent::Splice {
                start: 1,
                delete: 1,
                insert: Vec::new(),
            },
            ContextEvent::Splice {
                start: 1,
                delete: 1,
                insert: vec![ContextInsert::Existing {
                    cell_id: CellId::new(2),
                    source_index: 2,
                }],
            },
        ]
    );
}

#[test]
fn compact_live_archives_the_old_root_and_compiles_the_installed_context() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();
    runtime
        .append([message(1, MessageRole::User, "before")], &mut history)
        .unwrap();

    history.cells = vec![10];
    runtime
        .compact_live(
            RawBoundary(2),
            [SpineChar::Opaque {
                boundary: RawBoundary(2),
            }],
            &mut history,
        )
        .unwrap();
    history.cells.push(11);
    let output = runtime
        .append([message(3, MessageRole::User, "after")], &mut history)
        .unwrap();

    assert_eq!(
        output
            .projection()
            .spine()
            .nodes
            .iter()
            .map(|node| (&node.id, node.status))
            .collect::<Vec<_>>(),
        vec![
            (&NodeId::root_epoch(1), NodeStatus::Compacted),
            (&NodeId::root_epoch(2), NodeStatus::Live),
        ]
    );
    assert!(output.events().contains(&ContextEvent::Tag {
        index: 1,
        label: ContextLabel::UserAnchor(2),
    }));
}

#[test]
fn archived_recovery_matches_live_compact_projection() {
    let mut live_history = TestHistory { cells: vec![0] };
    let mut live =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();
    live.append([message(1, MessageRole::User, "before")], &mut live_history)
        .unwrap();
    live_history.cells = vec![10];
    live.compact_live(
        RawBoundary(2),
        [SpineChar::Opaque {
            boundary: RawBoundary(2),
        }],
        &mut live_history,
    )
    .unwrap();
    live_history.cells.push(11);
    live.append([message(3, MessageRole::User, "after")], &mut live_history)
        .unwrap();

    let mut recovered_history = TestHistory {
        cells: vec![20, 21],
    };
    let mut recovered =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();
    recovered
        .recover(
            [
                SpineRecoveryInput::Char(message(1, MessageRole::User, "before")),
                SpineRecoveryInput::Signal(SpineSignal::Compact {
                    boundary: RawBoundary(2),
                }),
            ],
            [
                SpineChar::Opaque {
                    boundary: RawBoundary(2),
                },
                message(3, MessageRole::User, "after"),
            ],
            &mut recovered_history,
        )
        .unwrap();

    assert_eq!(recovered.projection().spine(), live.projection().spine());
    assert_eq!(recovered_history.cells.len(), 2);
}

#[test]
fn recovery_rejects_archived_live_tail_without_committing() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();
    let before = runtime.projection().clone();

    let result = runtime.recover(
        [SpineRecoveryInput::Char(message(
            1,
            MessageRole::User,
            "live tail",
        ))],
        [message(1, MessageRole::User, "installed")],
        &mut history,
    );

    assert!(matches!(
        result,
        Err(SpineContextRuntimeError::ArchivedTraceHasLiveTail)
    ));
    assert_eq!(runtime.projection(), &before);
}

#[test]
fn recovery_restores_usage_after_the_archived_compact() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();

    runtime
        .recover(
            [
                SpineRecoveryInput::Signal(SpineSignal::Compact {
                    boundary: RawBoundary(1),
                }),
                SpineRecoveryInput::Signal(SpineSignal::Usage(TokenUsageSample {
                    boundary: RawBoundary(2),
                    input_tokens: 42,
                })),
            ],
            [SpineChar::Opaque {
                boundary: RawBoundary(1),
            }],
            &mut history,
        )
        .unwrap();

    assert_eq!(
        runtime.projection().usage_samples(),
        &[TokenUsageSample {
            boundary: RawBoundary(2),
            input_tokens: 42,
        }]
    );
}

#[test]
fn recovery_handler_failure_preserves_runtime_state() {
    let mut history = TestHistory { cells: vec![0] };
    let mut runtime =
        SpineContextRuntime::new(config(&[Feature::Jit]), TestHandler::default()).unwrap();
    runtime
        .append([message(1, MessageRole::User, "before")], &mut history)
        .unwrap();
    let before = runtime.projection().clone();
    runtime.handler_mut().reject = true;

    let result = runtime.recover(
        std::iter::empty(),
        [message(1, MessageRole::User, "installed")],
        &mut history,
    );

    assert!(matches!(
        result,
        Err(SpineContextRuntimeError::Handler(TestError))
    ));
    assert_eq!(runtime.projection(), &before);
}

use spine_core::ContextItem;
use spine_core::ContextTransition;
use spine_core::Feature;
use spine_core::HandlerCardinality;
use spine_core::HostStep;
use spine_core::Message;
use spine_core::MessageRole;
use spine_core::RawBoundary;
use spine_core::RolloutEvent;
use spine_core::SpineCompiler;
use spine_core::SpineConfig;
use spine_core::SpineEventHandlers;
use spine_core::SpineHost;
use spine_core::SpineObserverEvent;
use spine_core::SpineRuntime;
use spine_core::TokenUsageSample;
use spine_core::ToolCallGroup;
use spine_core::ToolOutcome;
use spine_core::ToolUse;
use spine_core::TrimEdit;
use spine_core::TrimProjection;
use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct TestHandlers<E> {
    prepares: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    observers: Arc<AtomicUsize>,
    failure: Option<E>,
    cardinality: HandlerCardinality,
    marker: PhantomData<E>,
}

impl<E> TestHandlers<E> {
    fn valid() -> Self {
        Self {
            prepares: Arc::new(AtomicUsize::new(0)),
            resets: Arc::new(AtomicUsize::new(0)),
            commits: Arc::new(AtomicUsize::new(0)),
            observers: Arc::new(AtomicUsize::new(0)),
            failure: None,
            cardinality: HandlerCardinality {
                context_owners: 1,
                observers: 1,
            },
            marker: PhantomData,
        }
    }
}

impl<F, E> SpineEventHandlers<F> for TestHandlers<E>
where
    E: std::error::Error + Clone,
{
    type History = ();
    type PreparedContext = bool;
    type Error = E;

    fn cardinality(&self) -> HandlerCardinality {
        self.cardinality
    }

    fn prepare_context(
        &self,
        _history: &Self::History,
        event: spine_core::SpineTransitionEvent<'_, F>,
    ) -> Result<Self::PreparedContext, Self::Error> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        Ok(matches!(
            event.transition,
            ContextTransition::ContextEpochReset(_)
        ))
    }

    fn commit_context(&mut self, _history: &mut Self::History, reset: Self::PreparedContext) {
        self.prepares.fetch_add(1, Ordering::Relaxed);
        if reset {
            self.resets.fetch_add(1, Ordering::Relaxed);
        }
        self.commits.fetch_add(1, Ordering::Relaxed);
    }

    fn notify_observers(&mut self, _event: SpineObserverEvent<'_, F>) {
        self.observers
            .fetch_add(self.cardinality.observers, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct SyntheticHost {
    calls: Arc<AtomicUsize>,
}

struct SilentHost;

struct FallibleSilentHost;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MutableHistoryHandlers {
    failure: bool,
    prepares: usize,
    resets: usize,
    commits: usize,
    observers: usize,
}

impl<F> SpineEventHandlers<F> for MutableHistoryHandlers {
    type History = Vec<ContextItem>;
    type PreparedContext = (Vec<ContextItem>, bool);
    type Error = SyntheticError;

    fn cardinality(&self) -> HandlerCardinality {
        HandlerCardinality {
            context_owners: 1,
            observers: 1,
        }
    }

    fn prepare_context(
        &self,
        history: &Self::History,
        event: spine_core::SpineTransitionEvent<'_, F>,
    ) -> Result<Self::PreparedContext, Self::Error> {
        if self.failure {
            return Err(SyntheticError);
        }
        let mut prepared = history.clone();
        let reset = match event.transition {
            ContextTransition::Append(items) => {
                prepared.extend_from_slice(items);
                false
            }
            ContextTransition::ContextEpochReset(context) => {
                prepared = context.to_vec();
                true
            }
        };
        Ok((prepared, reset))
    }

    fn commit_context(
        &mut self,
        history: &mut Self::History,
        (prepared, reset): Self::PreparedContext,
    ) {
        self.prepares += 1;
        self.resets += usize::from(reset);
        self.commits += 1;
        *history = prepared;
    }

    fn notify_observers(&mut self, _event: SpineObserverEvent<'_, F>) {
        self.observers += 1;
    }
}

impl SpineHost for SilentHost {
    type Input = ();
    type Frontier = usize;
    type Error = Infallible;

    fn initial_frontier(&self) -> Self::Frontier {
        0
    }

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        _input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error> {
        Ok(HostStep::new(frontier + 1, Vec::new(), Vec::new(), None))
    }
}

impl SpineHost for FallibleSilentHost {
    type Input = ();
    type Frontier = usize;
    type Error = SyntheticError;

    fn initial_frontier(&self) -> Self::Frontier {
        0
    }

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        _input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error> {
        Ok(HostStep::new(frontier + 1, Vec::new(), Vec::new(), None))
    }
}

impl SpineHost for SyntheticHost {
    type Input = RolloutEvent;
    type Frontier = usize;
    type Error = Infallible;

    fn initial_frontier(&self) -> Self::Frontier {
        self.calls.fetch_add(1, Ordering::Relaxed);
        0
    }

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(HostStep::new(
            frontier + 1,
            vec![input.clone()],
            Vec::new(),
            Some(input.boundary()),
        )
        .with_usage_sample(TokenUsageSample {
            boundary: input.boundary(),
            input_tokens: 100,
        }))
    }
}

fn config(features: &[Feature]) -> SpineConfig {
    SpineConfig::v1()
        .with_features(features.iter().copied())
        .expect("valid test configuration")
}

fn message(boundary: u64, role: MessageRole, content: &str) -> RolloutEvent {
    RolloutEvent::Message(Message {
        boundary: RawBoundary(boundary),
        role,
        content: content.to_owned(),
    })
}

fn tool_group(
    start: u64,
    call_id: &str,
    name: &str,
    arguments: &str,
    output: &str,
) -> RolloutEvent {
    RolloutEvent::ToolCall(ToolCallGroup {
        start: RawBoundary(start),
        end: RawBoundary(start + 1),
        leading_assistant_messages: Vec::new(),
        calls: vec![ToolUse {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            outcome: Some(ToolOutcome::Succeeded),
            output: Some(output.to_string()),
            output_boundary: Some(RawBoundary(start + 1)),
        }],
    })
}

#[test]
fn runtime_replays_aot_prefix_then_continues_jit_with_direct_compiler_parity() {
    let events = vec![
        message(1, MessageRole::User, "request"),
        message(2, MessageRole::Assistant, "working"),
        message(3, MessageRole::User, "continue"),
    ];
    let calls = Arc::new(AtomicUsize::new(0));
    let host = SyntheticHost {
        calls: Arc::clone(&calls),
    };
    let mut runtime =
        SpineRuntime::new(config(&[Feature::Jit]), host, TestHandlers::valid()).unwrap();

    let aot = runtime.replay(events[..2].iter(), &mut ()).unwrap();
    assert_eq!(aot.runtime_projection().spine().visible_context.len(), 2);
    let jit = runtime.eat(&events[2], &mut ()).unwrap();

    let mut direct = SpineCompiler::new(config(&[Feature::Jit])).unwrap();
    let expected = direct.replay(events).unwrap();
    assert_eq!(jit.runtime_projection().spine(), &expected.projection);
    assert_eq!(jit.runtime_projection().spine(), &expected.projection);
    assert!(calls.load(Ordering::Relaxed) > 0);
}

#[test]
fn replay_returns_a_complete_edit_and_clears_trim_state_at_compact() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut runtime = SpineRuntime::new(
        config(&[Feature::Jit, Feature::Trim]),
        host,
        TestHandlers::valid(),
    )
    .unwrap();
    runtime
        .eat(&message(1, MessageRole::User, "old"), &mut ())
        .unwrap();
    let candidate = tool_group(2, "shell", "shell", "{}", &"evidence".repeat(2_000));
    runtime.eat(&candidate, &mut ()).unwrap();
    let mut installed = runtime.projection().visible_context.clone();

    let replacement = message(4, MessageRole::User, "replacement");
    let compact = RolloutEvent::Compact {
        boundary: RawBoundary(3),
        replacement_history: Vec::new(),
    };
    let output = runtime
        .replay([compact, replacement].iter(), &mut ())
        .unwrap();
    output.context_edit().apply(&mut installed);

    assert_eq!(
        installed,
        output.runtime_projection().spine().visible_context
    );
    assert_eq!(
        output.runtime_projection().trim_changed_boundaries(),
        &[RawBoundary(3)]
    );
    assert_eq!(
        output.runtime_projection().trim_projection(),
        Some(&TrimProjection::default())
    );

    let empty = runtime.replay(std::iter::empty(), &mut ()).unwrap();
    empty.context_edit().apply(&mut installed);
    assert!(installed.is_empty());
}

#[derive(Clone, Debug)]
enum FallibleInput {
    Event(RolloutEvent),
    Fail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SyntheticError;

impl fmt::Display for SyntheticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("synthetic host failure")
    }
}

impl std::error::Error for SyntheticError {}

struct FallibleHost;

impl SpineHost for FallibleHost {
    type Input = FallibleInput;
    type Frontier = usize;
    type Error = SyntheticError;

    fn initial_frontier(&self) -> Self::Frontier {
        0
    }

    fn ingest(
        &self,
        frontier: &Self::Frontier,
        input: &Self::Input,
    ) -> Result<HostStep<Self::Frontier>, Self::Error> {
        match input {
            FallibleInput::Event(event) => Ok(HostStep::new(
                frontier + 1,
                vec![event.clone()],
                Vec::new(),
                Some(event.boundary()),
            )),
            FallibleInput::Fail => Err(SyntheticError),
        }
    }
}

#[test]
fn failed_replay_restores_the_previous_runtime_state() {
    let mut runtime =
        SpineRuntime::new(config(&[Feature::Jit]), FallibleHost, TestHandlers::valid()).unwrap();
    runtime
        .eat(
            &FallibleInput::Event(message(1, MessageRole::User, "installed")),
            &mut (),
        )
        .unwrap();
    let previous_projection = runtime.projection().clone();
    let previous_runtime_projection = runtime.runtime_projection().clone();
    let previous_frontier = *runtime.frontier().unwrap();

    let result = runtime.replay(
        [
            FallibleInput::Event(message(2, MessageRole::User, "partial")),
            FallibleInput::Fail,
        ]
        .iter(),
        &mut (),
    );

    assert!(result.is_err());
    assert_eq!(runtime.projection(), &previous_projection);
    assert_eq!(runtime.runtime_projection(), &previous_runtime_projection);
    assert_eq!(runtime.frontier(), Some(&previous_frontier));
}

#[test]
fn feature_off_is_identity_and_never_calls_host() {
    let calls = Arc::new(AtomicUsize::new(0));
    let host = SyntheticHost {
        calls: Arc::clone(&calls),
    };
    let mut runtime =
        SpineRuntime::new(config(&[]), host, TestHandlers::<Infallible>::valid()).unwrap();
    let event = message(8, MessageRole::User, "ignored");

    let output = runtime.eat(&event, &mut ()).unwrap();

    assert_eq!(output.context_edit().delete, 0);
    assert!(output.context_edit().insert.is_empty());
    assert!(
        output
            .runtime_projection()
            .spine()
            .visible_context
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn runtime_publishes_host_usage_observation() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut runtime =
        SpineRuntime::new(config(&[Feature::Jit]), host, TestHandlers::valid()).unwrap();
    let event = message(8, MessageRole::User, "observed");

    let output = runtime.eat(&event, &mut ()).unwrap();

    assert_eq!(
        output.runtime_projection().usage_samples(),
        &[TokenUsageSample {
            boundary: RawBoundary(8),
            input_tokens: 100,
        }]
    );
}

#[test]
fn runtime_retains_only_pressure_relevant_usage_samples() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut runtime =
        SpineRuntime::new(config(&[Feature::Jit]), host, TestHandlers::valid()).unwrap();
    let events = [
        message(1, MessageRole::User, "first"),
        message(2, MessageRole::Assistant, "intermediate"),
        message(3, MessageRole::User, "latest"),
    ];

    let output = runtime.replay(events.iter(), &mut ()).unwrap();

    assert_eq!(
        output.runtime_projection().usage_samples(),
        &[
            TokenUsageSample {
                boundary: RawBoundary(1),
                input_tokens: 100,
            },
            TokenUsageSample {
                boundary: RawBoundary(3),
                input_tokens: 100,
            },
        ]
    );
}

#[test]
fn runtime_publishes_incremental_trim_projection_and_render_invalidations() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut runtime =
        SpineRuntime::new(config(&[Feature::Trim]), host, TestHandlers::valid()).unwrap();
    let candidate = tool_group(1, "shell-call", "shell", "{}", &"evidence".repeat(2_000));

    let tagged = runtime.eat(&candidate, &mut ()).unwrap();

    assert_eq!(
        tagged.runtime_projection().trim_changed_boundaries(),
        &[RawBoundary(2)]
    );
    assert!(matches!(
        tagged
            .runtime_projection()
            .trim_projection()
            .and_then(|projection| projection.edit(RawBoundary(2), "shell-call")),
        Some(TrimEdit::Tagged { trim_id, .. }) if trim_id == "trim_2"
    ));

    let request = tool_group(
        3,
        "trim-call",
        "spine.trim",
        r#"{"TRIM_ID":"trim_2","op":"snip"}"#,
        "accepted",
    );
    let trimmed = runtime.eat(&request, &mut ()).unwrap();

    assert_eq!(
        trimmed.runtime_projection().trim_changed_boundaries(),
        &[RawBoundary(2)]
    );
    assert!(matches!(
        trimmed
            .runtime_projection()
            .trim_projection()
            .and_then(|projection| projection.edit(RawBoundary(2), "shell-call")),
        Some(TrimEdit::Snipped)
    ));
    assert_eq!(
        trimmed.runtime_projection().trim_projection(),
        Some(&TrimProjection::derive(&[candidate, request]))
    );
}

#[test]
fn active_runtime_rejects_invalid_handler_cardinality() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut missing_observer = TestHandlers::<Infallible>::valid();
    missing_observer.cardinality.observers = 0;

    let result = SpineRuntime::new(config(&[Feature::Jit]), host, missing_observer);

    assert!(matches!(
        result,
        Err(spine_core::InitError::InvalidHandlerCardinality {
            context_owners: 1,
            observers: 0,
        })
    ));

    for context_owners in [0, 2] {
        let host = SyntheticHost {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let mut handlers = TestHandlers::<Infallible>::valid();
        handlers.cardinality.context_owners = context_owners;
        assert!(matches!(
            SpineRuntime::new(config(&[Feature::Jit]), host, handlers),
            Err(spine_core::InitError::InvalidHandlerCardinality {
                context_owners: actual,
                observers: 1,
            }) if actual == context_owners
        ));
    }
}

#[test]
fn active_runtime_accepts_and_notifies_multiple_observers() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut handlers = TestHandlers::<Infallible>::valid();
    handlers.cardinality.observers = 2;
    let observers = Arc::clone(&handlers.observers);
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), host, handlers).unwrap();

    runtime
        .eat(&message(1, MessageRole::User, "request"), &mut ())
        .unwrap();

    assert_eq!(observers.load(Ordering::Relaxed), 2);
}

#[test]
fn handler_failure_leaves_runtime_and_handler_state_uncommitted() {
    let mut handlers = TestHandlers::valid();
    handlers.failure = Some(SyntheticError);
    let commits = Arc::clone(&handlers.commits);
    let observers = Arc::clone(&handlers.observers);
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), FallibleHost, handlers).unwrap();
    let previous_projection = runtime.projection().clone();
    let previous_runtime_projection = runtime.runtime_projection().clone();
    let previous_frontier = *runtime.frontier().unwrap();

    let result = runtime.eat(
        &FallibleInput::Event(message(1, MessageRole::User, "rejected")),
        &mut (),
    );

    assert!(result.is_err());
    assert_eq!(runtime.projection(), &previous_projection);
    assert_eq!(runtime.runtime_projection(), &previous_runtime_projection);
    assert_eq!(runtime.frontier(), Some(&previous_frontier));
    assert_eq!(commits.load(Ordering::Relaxed), 0);
    assert_eq!(observers.load(Ordering::Relaxed), 0);
}

#[test]
fn replay_publishes_one_reset_for_ten_thousand_inputs() {
    let handlers = TestHandlers::<Infallible>::valid();
    let prepares = Arc::clone(&handlers.prepares);
    let resets = Arc::clone(&handlers.resets);
    let commits = Arc::clone(&handlers.commits);
    let observers = Arc::clone(&handlers.observers);
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), SilentHost, handlers).unwrap();
    let events = [(); 10_000];

    runtime.replay(events.iter(), &mut ()).unwrap();

    assert_eq!(prepares.load(Ordering::Relaxed), 1);
    assert_eq!(resets.load(Ordering::Relaxed), 1);
    assert_eq!(commits.load(Ordering::Relaxed), 1);
    assert_eq!(observers.load(Ordering::Relaxed), 1);
}

#[test]
fn mutable_history_handler_installs_append_and_epoch_reset() {
    let mut runtime = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut history = Vec::new();
    runtime
        .eat(
            &FallibleInput::Event(message(1, MessageRole::User, "request")),
            &mut history,
        )
        .unwrap();
    assert_eq!(history, runtime.projection().visible_context);

    runtime.replay(std::iter::empty(), &mut history).unwrap();
    assert!(history.is_empty());
}

#[test]
fn live_context_replacement_is_an_explicit_epoch_reset() {
    let mut runtime = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut history = Vec::new();
    runtime
        .eat(
            &FallibleInput::Event(message(1, MessageRole::User, "request")),
            &mut history,
        )
        .unwrap();
    let first_epoch = history.clone();
    runtime
        .eat(
            &FallibleInput::Event(tool_group(
                2,
                "open",
                "spine.open",
                r#"{"summary":"child"}"#,
                "Spine open accepted.",
            )),
            &mut history,
        )
        .unwrap();
    assert!(history.starts_with(&first_epoch));
    assert_eq!(runtime.handlers().resets, 0);

    runtime
        .eat(
            &FallibleInput::Event(tool_group(
                4,
                "close",
                "spine.close",
                r#"{"memory":"done"}"#,
                "Spine close accepted.",
            )),
            &mut history,
        )
        .unwrap();

    assert_eq!(runtime.handlers().resets, 1);
    assert_eq!(history, runtime.projection().visible_context);
}

#[test]
fn mutable_history_prepare_failure_preserves_history() {
    let handlers = MutableHistoryHandlers {
        failure: true,
        ..Default::default()
    };
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), FallibleHost, handlers).unwrap();
    let mut history = vec![ContextItem::Message {
        message: Message {
            boundary: RawBoundary(0),
            role: MessageRole::User,
            content: "old".to_string(),
        },
        user_anchor: Some(1),
    }];
    let before = history.clone();
    let before_handlers = runtime.handlers().clone();
    assert!(
        runtime
            .eat(
                &FallibleInput::Event(message(1, MessageRole::User, "rejected")),
                &mut history,
            )
            .is_err()
    );
    assert_eq!(history, before);
    assert_eq!(runtime.handlers(), &before_handlers);
}

#[test]
fn mutable_history_replay_scales_from_empty_to_ten_thousand_inputs() {
    for count in [0, 1, 10_000] {
        let mut runtime = SpineRuntime::new(
            config(&[Feature::Jit]),
            FallibleSilentHost,
            MutableHistoryHandlers::default(),
        )
        .unwrap();
        let mut history = Vec::new();
        let inputs = vec![(); count];
        runtime.replay(inputs.iter(), &mut history).unwrap();
        assert!(history.is_empty());
        assert_eq!(runtime.handlers().prepares, 1);
        assert_eq!(runtime.handlers().resets, 1);
        assert_eq!(runtime.handlers().commits, 1);
        assert_eq!(runtime.handlers().observers, 1);
    }
}

#[test]
fn mutable_history_live_fold_equals_batch_replay() {
    let events = [
        FallibleInput::Event(message(1, MessageRole::User, "first")),
        FallibleInput::Event(message(2, MessageRole::Assistant, "second")),
    ];
    let mut live = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut live_history = Vec::new();
    for event in &events {
        live.eat(event, &mut live_history).unwrap();
    }
    let mut replay = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut replay_history = Vec::new();
    replay.replay(events.iter(), &mut replay_history).unwrap();

    assert_eq!(replay.runtime_projection(), live.runtime_projection());
    assert_eq!(replay_history, live_history);
}

#[test]
fn mutable_history_live_fold_equals_replay_for_structural_trim_events() {
    let candidate = tool_group(9, "shell-call", "shell", "{}", &"evidence".repeat(2_000));
    let events = [
        FallibleInput::Event(message(1, MessageRole::User, "root")),
        FallibleInput::Event(tool_group(
            2,
            "open-call",
            "spine.open",
            r#"{"summary":"child"}"#,
            "Spine open accepted.",
        )),
        FallibleInput::Event(message(4, MessageRole::Assistant, "inside")),
        FallibleInput::Event(tool_group(
            5,
            "close-call",
            "spine.close",
            r#"{"memory":"closed"}"#,
            "Spine close accepted.",
        )),
        FallibleInput::Event(tool_group(
            7,
            "next-call",
            "spine.next",
            r#"{"summary":"sibling","memory":"next"}"#,
            "Spine next accepted.",
        )),
        FallibleInput::Event(candidate),
        FallibleInput::Event(tool_group(
            11,
            "trim-call",
            "spine.trim",
            r#"{"TRIM_ID":"trim_10","op":"snip"}"#,
            "Spine trim accepted.",
        )),
    ];
    let mut live = SpineRuntime::new(
        config(&[Feature::Jit, Feature::Trim]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut live_history = Vec::new();
    for event in &events {
        live.eat(event, &mut live_history).unwrap();
    }
    let mut replay = SpineRuntime::new(
        config(&[Feature::Jit, Feature::Trim]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut replay_history = Vec::new();
    replay.replay(events.iter(), &mut replay_history).unwrap();

    assert_eq!(replay.runtime_projection(), live.runtime_projection());
    assert_eq!(replay_history, live_history);
    assert!(matches!(
        live.runtime_projection()
            .trim_projection()
            .and_then(|projection| projection.edit(RawBoundary(10), "shell-call")),
        Some(TrimEdit::Snipped)
    ));
}

#[test]
fn mutable_history_live_fold_equals_replay_across_compact_epoch() {
    let events = [
        FallibleInput::Event(message(1, MessageRole::User, "before")),
        FallibleInput::Event(RolloutEvent::Compact {
            boundary: RawBoundary(2),
            replacement_history: Vec::new(),
        }),
        FallibleInput::Event(message(3, MessageRole::User, "after")),
    ];
    let mut live = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut live_history = Vec::new();
    for event in &events {
        live.eat(event, &mut live_history).unwrap();
    }
    let mut replay = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let mut replay_history = Vec::new();
    replay.replay(events.iter(), &mut replay_history).unwrap();

    assert_eq!(replay.runtime_projection(), live.runtime_projection());
    assert_eq!(replay_history, live_history);
}

#[test]
fn mutable_history_replay_handler_failure_is_atomic() {
    let mut runtime = SpineRuntime::new(
        config(&[Feature::Jit]),
        FallibleHost,
        MutableHistoryHandlers::default(),
    )
    .unwrap();
    let installed = FallibleInput::Event(message(1, MessageRole::User, "installed"));
    let mut history = Vec::new();
    runtime.eat(&installed, &mut history).unwrap();
    let before_history = history.clone();
    let before_projection = runtime.runtime_projection().clone();
    let before_frontier = *runtime.frontier().unwrap();
    runtime.handlers_mut().failure = true;
    let before_handlers = runtime.handlers().clone();
    let replacement = [FallibleInput::Event(message(
        2,
        MessageRole::User,
        "rejected",
    ))];

    assert!(runtime.replay(replacement.iter(), &mut history).is_err());
    assert_eq!(history, before_history);
    assert_eq!(runtime.runtime_projection(), &before_projection);
    assert_eq!(runtime.frontier(), Some(&before_frontier));
    assert_eq!(runtime.handlers(), &before_handlers);
}

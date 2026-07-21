use spine_core::Feature;
use spine_core::HostStep;
use spine_core::Message;
use spine_core::MessageRole;
use spine_core::RawBoundary;
use spine_core::RolloutEvent;
use spine_core::SpineCompiler;
use spine_core::SpineConfig;
use spine_core::SpineHost;
use spine_core::SpineRuntime;
use spine_core::TokenUsageSample;
use spine_core::ToolCallGroup;
use spine_core::ToolOutcome;
use spine_core::ToolUse;
use spine_core::TrimEdit;
use spine_core::TrimProjection;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Clone)]
struct SyntheticHost {
    calls: Arc<AtomicUsize>,
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
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), host).unwrap();

    let aot = runtime.replay(events[..2].iter()).unwrap();
    assert_eq!(aot.delta().projection.visible_context.len(), 2);
    let jit = runtime.eat(&events[2]).unwrap();

    let mut direct = SpineCompiler::new(config(&[Feature::Jit])).unwrap();
    let expected = direct.replay(events).unwrap();
    assert_eq!(jit.runtime_projection().spine(), &expected.projection);
    assert_eq!(jit.delta().projection, expected.projection);
    assert!(calls.load(Ordering::Relaxed) > 0);
}

#[test]
fn feature_off_is_identity_and_never_calls_host() {
    let calls = Arc::new(AtomicUsize::new(0));
    let host = SyntheticHost {
        calls: Arc::clone(&calls),
    };
    let mut runtime = SpineRuntime::new(config(&[]), host).unwrap();
    let event = message(8, MessageRole::User, "ignored");

    let output = runtime.eat(&event).unwrap();

    assert_eq!(output.delta().context_edit.delete, 0);
    assert!(output.delta().context_edit.insert.is_empty());
    assert!(output.delta().projection.visible_context.is_empty());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn runtime_publishes_host_usage_observation() {
    let host = SyntheticHost {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), host).unwrap();
    let event = message(8, MessageRole::User, "observed");

    let output = runtime.eat(&event).unwrap();

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
    let mut runtime = SpineRuntime::new(config(&[Feature::Jit]), host).unwrap();
    let events = vec![
        message(1, MessageRole::User, "first"),
        message(2, MessageRole::Assistant, "intermediate"),
        message(3, MessageRole::User, "latest"),
    ];

    let output = runtime.replay(events.iter()).unwrap();

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
    let mut runtime = SpineRuntime::new(config(&[Feature::Trim]), host).unwrap();
    let candidate = tool_group(1, "shell-call", "shell", "{}", &"evidence".repeat(2_000));

    let tagged = runtime.eat(&candidate).unwrap();

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
    let trimmed = runtime.eat(&request).unwrap();

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

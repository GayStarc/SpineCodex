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
        output.runtime_projection().usage_sample(),
        Some(TokenUsageSample {
            boundary: RawBoundary(8),
            input_tokens: 100,
        })
    );
}

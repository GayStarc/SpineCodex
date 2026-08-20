use spine_core::host::CanonicalReplay;
use spine_core::host::ContextEpoch;
use spine_core::host::ContextLabel;
use spine_core::host::ContextPlanCell;
use spine_core::host::ExecutionOrigin;
use spine_core::host::Feature;
use spine_core::host::Message;
use spine_core::host::MessageRole;
use spine_core::host::ObservedOutput;
use spine_core::host::RawBoundary;
use spine_core::host::RecordDigest;
use spine_core::host::ReplayInput;
use spine_core::host::SamplingArchiveRecord;
use spine_core::host::SamplingFinish;
use spine_core::host::SamplingRuntime;
use spine_core::host::SamplingTerminal;
use spine_core::host::SourceObservation;
use spine_core::host::SpineChar;
use spine_core::host::SpineConfig;
use spine_core::host::ThreadNamespace;
use spine_core::host::TrimEdit;
use spine_core::host::TrimRequest;

#[test]
fn native_items_are_opaque_to_the_sampling_host_contract() {
    let thread = ThreadNamespace::parse("host-sampling").expect("valid thread");
    let mut runtime = SamplingRuntime::new(thread, ContextEpoch::new(0), SpineConfig::v1())
        .expect("valid runtime");
    runtime
        .observe_source([
            SpineChar::Opaque {
                boundary: RawBoundary(1),
            },
            SpineChar::Message(Message {
                boundary: RawBoundary(2),
                role: MessageRole::User,
                content: "interrupting user message".to_string(),
            }),
            SpineChar::Opaque {
                boundary: RawBoundary(3),
            },
        ])
        .expect("native tool items must not create a parser group");
    assert_eq!(runtime.source_snapshot().cells().len(), 3);
}

#[test]
fn trim_uses_sampling_output_metadata_without_a_tool_group() {
    let thread = ThreadNamespace::parse("host-trim").expect("valid thread");
    let config = SpineConfig::v1()
        .with_features([Feature::Jit, Feature::Trim])
        .expect("valid config");
    let mut runtime =
        SamplingRuntime::new(thread, ContextEpoch::new(0), config).expect("valid runtime");
    runtime
        .observe_source([SpineChar::Message(Message {
            boundary: RawBoundary(1),
            role: MessageRole::User,
            content: "produce output".to_string(),
        })])
        .expect("user source");

    let source_sampling = runtime.begin_sampling().expect("begin output sampling");
    runtime
        .sampling_started_record(&source_sampling, RecordDigest::digest(b"output-prompt"))
        .expect("output sampling started");
    let output_id = runtime
        .observe_source_observations([SourceObservation::new(SpineChar::Opaque {
            boundary: RawBoundary(2),
        })
        .with_output(ObservedOutput {
            execution_ref: "host-output".to_string(),
            body: "x".repeat(10_001),
        })])
        .expect("output source")[0]
        .clone();
    let SamplingFinish::Prepared(prepared) = runtime
        .finish_sampling(source_sampling, SamplingTerminal::Completed)
        .expect("prepare output sampling")
    else {
        panic!("output sampling must prepare");
    };
    runtime
        .install_prepared(prepared)
        .expect("install output sampling");

    let request =
        TrimRequest::parse(r#"{"TRIM_ID":"trim_2","op":"snip"}"#).expect("valid trim request");
    let operation = runtime
        .validated_trim_fact(&request)
        .expect("output metadata creates a stable trim target");

    let trim_sampling = runtime.begin_sampling().expect("begin trim sampling");
    runtime
        .sampling_started_record(&trim_sampling, RecordDigest::digest(b"trim-prompt"))
        .expect("trim sampling started");
    runtime
        .register_execution("trim-execution")
        .expect("register trim");
    runtime
        .stage_execution(
            "trim-execution",
            ExecutionOrigin::Direct {
                execution_ref: "trim-execution".to_string(),
            },
            operation,
        )
        .expect("stage trim");
    runtime
        .observe_source_observations([SourceObservation::new(SpineChar::Opaque {
            boundary: RawBoundary(3),
        })
        .with_output(ObservedOutput {
            execution_ref: "trim-execution".to_string(),
            body: "trim accepted".to_string(),
        })])
        .expect("trim source");
    runtime
        .finish_execution("trim-execution", true)
        .expect("finish trim");
    let SamplingFinish::Prepared(prepared) = runtime
        .finish_sampling(trim_sampling, SamplingTerminal::Completed)
        .expect("prepare trim sampling")
    else {
        panic!("trim sampling must prepare");
    };
    assert!(prepared.context_plan().cells.iter().any(|cell| {
        matches!(
            cell,
            ContextPlanCell::Source { source_id, labels }
                if source_id == &output_id
                    && labels == &[ContextLabel::Output(TrimEdit::Snipped)]
        )
    }));
}

#[test]
fn output_metadata_survives_canonical_replay() {
    let thread = ThreadNamespace::parse("host-trim-replay").expect("valid thread");
    let config = SpineConfig::v1()
        .with_features([Feature::Jit, Feature::Trim])
        .expect("valid config");
    let user = SourceObservation::new(SpineChar::Message(Message {
        boundary: RawBoundary(1),
        role: MessageRole::User,
        content: "produce durable output".to_string(),
    }));
    let output = SourceObservation::new(SpineChar::Opaque {
        boundary: RawBoundary(2),
    })
    .with_output(ObservedOutput {
        execution_ref: "durable-output".to_string(),
        body: "x".repeat(10_001),
    });

    let mut runtime = SamplingRuntime::new(thread.clone(), ContextEpoch::new(0), config.clone())
        .expect("valid runtime");
    runtime
        .observe_source_observations([user.clone()])
        .expect("user source");
    let sampling = runtime.begin_sampling().expect("begin sampling");
    let started = runtime
        .sampling_started_record(&sampling, RecordDigest::digest(b"durable-prompt"))
        .expect("sampling started");
    runtime
        .observe_source_observations([output.clone()])
        .expect("output source");
    let SamplingFinish::Prepared(prepared) = runtime
        .finish_sampling(sampling, SamplingTerminal::Completed)
        .expect("prepare sampling")
    else {
        panic!("completed sampling must prepare");
    };
    let committed = SamplingArchiveRecord::SamplingCommit(prepared.durable_record().clone());
    runtime
        .install_prepared(prepared)
        .expect("install sampling");

    let request =
        TrimRequest::parse(r#"{"TRIM_ID":"trim_2","op":"snip"}"#).expect("valid trim request");
    let expected = runtime
        .validated_trim_fact(&request)
        .expect("live output target");
    let replayed = CanonicalReplay::new(thread)
        .expect("replay")
        .with_runtime_config(config)
        .expect("replay config")
        .prepare([
            ReplayInput::Source(user),
            ReplayInput::Archive(started),
            ReplayInput::Source(output),
            ReplayInput::Archive(committed),
        ])
        .expect("canonical replay")
        .into_runtime();
    assert_eq!(
        replayed
            .validated_trim_fact(&request)
            .expect("replayed output target"),
        expected
    );
}

use super::*;
use crate::MessageRole;
use pretty_assertions::assert_eq;

fn message(boundary: u64, role: MessageRole, content: &str) -> SpineChar {
    SpineChar::Message(Message {
        boundary: RawBoundary(boundary),
        role,
        content: content.to_string(),
    })
}

fn request(boundary: u64, call_id: &str, name: &str) -> SpineChar {
    SpineChar::ToolRequest(ToolRequestChar {
        boundary: RawBoundary(boundary),
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments: "{}".to_string(),
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
fn one_item_character_adds_exactly_one_stack_cell() {
    let mut parser = SpineCharParser::default();

    let step = parser
        .eat(message(1, MessageRole::User, "request"))
        .unwrap();

    assert_eq!(step.stack_size(), 1);
    assert_eq!(parser.stack().len(), 1);
    assert_eq!(
        step.events(),
        &[RolloutEvent::Message(Message {
            boundary: RawBoundary(1),
            role: MessageRole::User,
            content: "request".to_string(),
        })]
    );
}

#[test]
fn assistant_prefix_waits_and_joins_the_following_tool_group() {
    let mut parser = SpineCharParser::default();

    let assistant = parser
        .eat(message(1, MessageRole::Assistant, "working"))
        .unwrap();
    assert!(assistant.events().is_empty());
    assert_eq!(assistant.pending_boundaries(), &[RawBoundary(1)]);

    let request = parser.eat(request(2, "call", "shell")).unwrap();
    assert!(request.events().is_empty());
    assert_eq!(
        request.pending_boundaries(),
        &[RawBoundary(1), RawBoundary(2)]
    );

    let completed = parser.eat(response(3, "call", "done")).unwrap();
    assert_eq!(completed.pending_boundaries(), &[]);
    assert_eq!(
        completed.events(),
        &[RolloutEvent::ToolCall(ToolCallGroup {
            start: RawBoundary(1),
            end: RawBoundary(3),
            leading_assistant_messages: vec![Message {
                boundary: RawBoundary(1),
                role: MessageRole::Assistant,
                content: "working".to_string(),
            }],
            calls: vec![ToolUse {
                call_id: "call".to_string(),
                name: "shell".to_string(),
                arguments: "{}".to_string(),
                outcome: Some(ToolOutcome::Succeeded),
                output: Some("done".to_string()),
                output_boundary: Some(RawBoundary(3)),
            }],
        })]
    );
    assert_eq!(parser.stack().len(), 3);
}

#[test]
fn parallel_tool_group_reduces_only_after_every_response() {
    let mut parser = SpineCharParser::default();
    parser.eat(request(1, "a", "shell")).unwrap();
    parser.eat(request(2, "b", "shell")).unwrap();

    let partial = parser.eat(response(3, "b", "second")).unwrap();
    assert!(partial.events().is_empty());

    let complete = parser.eat(response(4, "a", "first")).unwrap();
    let [RolloutEvent::ToolCall(group)] = complete.events() else {
        panic!("expected one completed tool group");
    };
    assert!(group.is_complete());
    assert_eq!(group.start, RawBoundary(1));
    assert_eq!(group.end, RawBoundary(4));
    assert_eq!(parser.stack().len(), 4);
}

#[test]
fn usage_is_zero_width_and_compact_resets_the_live_stack() {
    let mut parser = SpineCharParser::default();
    parser.eat(message(1, MessageRole::User, "before")).unwrap();

    let usage = parser
        .eat(SpineChar::Usage(TokenUsageSample {
            boundary: RawBoundary(1),
            input_tokens: 42,
        }))
        .unwrap();
    assert_eq!(usage.stack_size(), 1);
    assert_eq!(usage.usage_sample().unwrap().input_tokens, 42);

    let replacement = ContextItem::Message {
        message: Message {
            boundary: RawBoundary(2),
            role: MessageRole::Assistant,
            content: "replacement".to_string(),
        },
        user_anchor: None,
    };
    let compact = parser
        .eat(SpineChar::Compact {
            boundary: RawBoundary(2),
            replacement_history: vec![replacement.clone()],
        })
        .unwrap();
    assert_eq!(compact.stack_size(), 1);
    assert_eq!(
        parser.stack().cells()[0].character(),
        &SpineChar::Synthetic {
            boundary: RawBoundary(2),
            item: replacement.clone(),
        }
    );
    assert_eq!(
        compact.events(),
        &[RolloutEvent::Compact {
            boundary: RawBoundary(2),
            replacement_history: vec![replacement],
        }]
    );
}

#[test]
fn pending_boundaries_keep_context_order_when_responses_are_out_of_call_order() {
    let mut parser = SpineCharParser::default();
    parser.eat(request(1, "a", "shell")).unwrap();
    parser.eat(request(2, "b", "shell")).unwrap();

    let partial = parser.eat(response(3, "a", "first")).unwrap();

    assert_eq!(
        partial.pending_boundaries(),
        &[RawBoundary(1), RawBoundary(2), RawBoundary(3)]
    );
}

#[test]
fn failed_character_does_not_commit_parser_state() {
    let mut parser = SpineCharParser::default();
    parser.eat(request(1, "call", "shell")).unwrap();
    let before = parser.clone();

    let result = parser.eat(message(2, MessageRole::User, "interrupt"));

    assert!(matches!(
        result,
        Err(CharParseError::IncompleteToolGroup {
            boundary: RawBoundary(2)
        })
    ));
    assert_eq!(parser, before);
}

use crate::spine::spawn::MIN_SPAWN_TASKS;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const SPINE_NAMESPACE: &str = "spine";
pub(crate) const SPINE_OPEN: &str = "open";
pub(crate) const SPINE_CLOSE: &str = "close";
pub(crate) const SPINE_NEXT: &str = "next";
pub(crate) const SPINE_SPAWN: &str = "spawn";
pub(crate) const SPINE_TRIM: &str = "trim";

const NODE_MEMORY_DESCRIPTION: &str = concat!(
    "Model-authored continuation state for replacing the finalized node's local working context. ",
    "Preserve only what later work needs beyond inherited context: completed or confirmed progress, confirmed findings, decisions and constraints, validation results, bounded unresolved factual gaps or risks, remaining work that can proceed from this memory and inherited context without reconstructing the replaced working context, and the logic linking evidence and findings to decisions and next steps. ",
    "Include compact supporting evidence or precise, recoverable references when needed. ",
    "For source code, cite exact paths and lines; for commands, cite the exact command and decisive output or result, so continuation need not replay the work. ",
    "Runtime preserves user messages and child memories. ",
    "Use existing `[U#]` anchors only to bind approvals, corrections, rejections, clarifications, and elliptical replies to their referents and record the resulting continuation-relevant semantic deltas in task scope, decisions, constraints, progress, and remaining obligations; the underlying user messages remain available independently of these references."
);

const OPEN_SUMMARY_DESCRIPTION: &str = "Concise, actionable, completable goal for a direct child that will own one distinct body of work and local working context with an independently completable lifecycle. The call carrying it remains in the child's context.";
const NEXT_SUMMARY_DESCRIPTION: &str = "Concise, actionable, completable goal for a true sibling that will own one distinct body of work and local working context with an independently completable lifecycle. The call carrying it remains in the sibling's context; finalized-node state belongs in memory.";
const TRIM_DESCRIPTION: &str = "Conservatively trim one tagged tool-result projection without changing the Spine tree or creating memory. A TRIM_ID is valid only for the immediately preceding tool-result batch and expires after the next assistant tool request; after a miss, do not retry it. Use slice to retain needed evidence, use snip only after useful facts are preserved, and otherwise leave the result unchanged.";
const SPAWN_DESCRIPTION: &str = concat!(
    "Fission the current work into two or more concurrent peer branches created from the current full history. ",
    "Each branch receives a differentiated assignment and must own a semantically independent direction: either resolve a concrete uncertainty or produce an independently verifiable outcome, with an explicit scope, evidence boundary, and completion predicate. ",
    "A branch may investigate, review, or implement directly and must return one terminal final memory. ",
    "Give every branch a concise summary that is unique within this spawn call; the runtime uses it as the branch's public identity. ",
    "Independent execution is the default. When coordination is required, provision and verify one task-local writable coordination directory before calling spine.spawn. Every affected task prompt must repeat the same `Shared collaboration directory: <path>` line, include a distinct `My collaboration file: <path>` inside it, identify relevant peer summaries and roles, and declare the artifact format, update/read protocol, required synchronization points, and a bounded fallback for unavailable or incomplete peer state. ",
    "Absent an environment-provided locking or atomic-append protocol, each branch artifact must be append-only and single-writer. Within that directory, affected branches may write only their own coordination artifact, may read named peer artifacts through the declared protocol even when the investigated source or evidence is otherwise read-only, and must not write a peer artifact. ",
    "Coordinate through the collaboration directory to minimize unnecessary duplicate work: respect assigned scopes, share reusable evidence early, and independently verify load-bearing or disputed claims. Peer coordination must never become a dependency, correctness-critical state, or an expansion of the assignment. ",
    "For exploration or review, treat inherited analytical conclusions as hypotheses to verify, refine, or falsify against primary evidence. ",
    "The original continuation is suspended during the fission; no supervisory model remains active. ",
    "Join waits for every branch, records their terminal results as closed task nodes under the current Spine scope atomically in input order, and then resumes the original continuation. ",
    "Call spine.spawn at most once in one model response; place every concurrent branch in that call's tasks array. ",
    "Use spine.spawn only when the current work has at least two substantial, self-contained, independently completable branches and concurrent execution would materially improve speed or result quality. Each branch must be able to complete with a bounded fallback rather than depending on another branch's result. ",
    "For one bounded delegated subtask, incremental assistance, or work that benefits from ongoing parent supervision, use the ordinary multi-agent spawn tool when available or continue locally. ",
    "Do not spawn paraphrased branches over the same tightly coupled question unless they are deliberately assigned as independent replication or falsification. ",
    "Branch workspace and external effects are non-transactional, so production-file writes require disjoint ownership or one explicitly named integration owner."
);

fn spawn_task_count_description(min_tasks: usize, max_tasks: usize) -> String {
    format!(
        "The tasks array must contain at least {min_tasks} and at most {max_tasks} task assignments."
    )
}

pub(crate) fn create_spine_tool(name: &str) -> ToolSpec {
    let function = match name {
        SPINE_OPEN => ResponsesApiTool {
            name: SPINE_OPEN.to_string(),
            description: "Enter a direct child under the current Spine cursor to own one distinct body of work and local working context with an independently completable lifecycle. Co-issued ordinary tools belong to the child; the transition applies to the current node's prior ReAct history.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "summary".to_string(),
                    JsonSchema::string(Some(OPEN_SUMMARY_DESCRIPTION.to_string())),
                )]),
                Some(vec!["summary".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        },
        SPINE_CLOSE => ResponsesApiTool {
            name: SPINE_CLOSE.to_string(),
            description: "Finalize the current node once its owned result is complete or precisely bounded and continuation can proceed from compact memory and inherited context without its full local working context, then return to its immediate parent. Root epochs cannot be closed. Co-issued ordinary tools belong to the parent; the transition applies to the current node's prior ReAct history.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "memory".to_string(),
                    JsonSchema::string(Some(NODE_MEMORY_DESCRIPTION.to_string())),
                )]),
                Some(vec!["memory".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        },
        SPINE_NEXT => ResponsesApiTool {
            name: SPINE_NEXT.to_string(),
            description: "Finalize the current node once its owned result is complete or precisely bounded and continuation can proceed from compact memory and inherited context without its full local working context, then enter a true sibling under the same parent. Co-issued ordinary tools belong to the sibling; the transition applies to the current node's prior ReAct history.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "summary".to_string(),
                        JsonSchema::string(Some(NEXT_SUMMARY_DESCRIPTION.to_string())),
                    ),
                    (
                        "memory".to_string(),
                        JsonSchema::string(Some(NODE_MEMORY_DESCRIPTION.to_string())),
                    ),
                ]),
                Some(vec!["summary".to_string(), "memory".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        },
        _ => panic!("unknown Spine tool: {name}"),
    };

    wrap_spine_tool(function)
}

pub(crate) fn create_spine_spawn_tool(max_tasks: usize) -> ToolSpec {
    assert!(
        max_tasks >= MIN_SPAWN_TASKS,
        "spine.spawn requires capacity for at least {MIN_SPAWN_TASKS} tasks"
    );
    let task = JsonSchema::object(
        BTreeMap::from([
            (
                "summary".to_string(),
                JsonSchema::string(Some(
                    "Concise branch label, distinct within this spawn call, and its independently owned outcome."
                        .to_string(),
                )),
            ),
            (
                "prompt".to_string(),
                JsonSchema::string(Some(
                    "Complete initial branch assignment. The branch identity is this task's summary. When coordination is required, include the same task-local `Shared collaboration directory: <path>` used by every affected branch, a distinct `My collaboration file: <path>` inside it, relevant peer summaries and roles, the artifact format and update/read protocol, required synchronization points, and a bounded fallback for unavailable or incomplete peer state. Absent locking or atomic append, the branch artifact must be append-only and single-writer. Coordination must never become a dependency, correctness-critical state, or an expansion of the assignment.".to_string(),
                )),
            ),
        ]),
        Some(vec!["summary".to_string(), "prompt".to_string()]),
        Some(false.into()),
    );
    wrap_spine_tool(ResponsesApiTool {
        name: SPINE_SPAWN.to_string(),
        description: format!(
            "{SPAWN_DESCRIPTION} {}",
            spawn_task_count_description(MIN_SPAWN_TASKS, max_tasks)
        ),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "tasks".to_string(),
                JsonSchema::array(
                    task,
                    Some("Ordered differentiated branch assignments.".to_string()),
                ),
            )]),
            Some(vec!["tasks".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn wrap_spine_tool(function: ResponsesApiTool) -> ToolSpec {
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SPINE_NAMESPACE.to_string(),
        description: "Use Spine to shape the work.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(function)],
    })
}

pub(crate) fn create_spine_trim_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "TRIM_ID".to_string(),
            JsonSchema::string(Some(
                "Trim id attached to a tool response in the immediately previous tool-result batch; it expires after your next assistant tool request."
                    .to_string(),
            )),
        ),
        (
            "op".to_string(),
            JsonSchema::string_enum(
                vec![serde_json::json!("snip"), serde_json::json!("slice")],
                Some("Use snip only when useful facts are preserved elsewhere; use slice to keep the needed head, tail, or anchor window.".to_string()),
            ),
        ),
        (
            "head".to_string(),
            JsonSchema::integer(Some("For op=\"slice\", keep this many characters from the start of the current visible body. Mutually exclusive with tail and anchor.".to_string())),
        ),
        (
            "tail".to_string(),
            JsonSchema::integer(Some("For op=\"slice\", keep this many characters from the end of the current visible body. Mutually exclusive with head and anchor.".to_string())),
        ),
        (
            "anchor".to_string(),
            JsonSchema::string(Some("For op=\"slice\", locate this non-empty text in the current visible body and keep an anchor window. Mutually exclusive with head and tail.".to_string())),
        ),
        (
            "preceding".to_string(),
            JsonSchema::integer(Some("For anchor slice, keep this many complete lines before the anchor line.".to_string())),
        ),
        (
            "following".to_string(),
            JsonSchema::integer(Some("For anchor slice, keep this many complete lines after the anchor line.".to_string())),
        ),
    ]);
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: SPINE_NAMESPACE.to_string(),
        description: "Use Spine to shape the work.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: SPINE_TRIM.to_string(),
            description: TRIM_DESCRIPTION.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["TRIM_ID".to_string(), "op".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_schema_exposes_only_control_tools() {
        for name in [SPINE_OPEN, SPINE_CLOSE, SPINE_NEXT] {
            let ToolSpec::Namespace(namespace) = create_spine_tool(name) else {
                panic!("expected namespace spec");
            };
            assert_eq!(namespace.tools.len(), 1);
            let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
            assert_eq!(function.name, name);
            assert!(!function.name.contains("tree"));
        }
        let ToolSpec::Namespace(namespace) = create_spine_spawn_tool(3) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
        assert_eq!(function.name, SPINE_SPAWN);
        assert!(
            function
                .description
                .contains("Call spine.spawn at most once in one model response")
        );
    }

    #[test]
    fn close_and_next_require_memory() {
        for (name, required) in [
            (SPINE_CLOSE, vec!["memory"]),
            (SPINE_NEXT, vec!["summary", "memory"]),
        ] {
            let ToolSpec::Namespace(namespace) = create_spine_tool(name) else {
                panic!("expected namespace spec");
            };
            let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
            let schema = serde_json::to_value(&function.parameters).unwrap();
            assert_eq!(schema["required"], serde_json::json!(required));
            assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        }
    }

    #[test]
    fn trim_schema_requires_id_and_operation() {
        let ToolSpec::Namespace(namespace) = create_spine_trim_tool() else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
        assert_eq!(function.name, SPINE_TRIM);
        let schema = serde_json::to_value(&function.parameters).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["TRIM_ID", "op"]));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn spawn_description_advertises_configured_task_bounds_without_schema_keywords() {
        let ToolSpec::Namespace(namespace) = create_spine_spawn_tool(5) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(function) = &namespace.tools[0];
        assert_eq!(
            function.description,
            format!(
                "{SPAWN_DESCRIPTION} The tasks array must contain at least 2 and at most 5 task assignments."
            )
        );
        let schema = serde_json::to_value(&function.parameters).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["tasks"]));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            schema["properties"]["tasks"].get("minItems"),
            None,
            "task bounds belong in the tool description"
        );
        assert_eq!(
            schema["properties"]["tasks"].get("maxItems"),
            None,
            "task bounds belong in the tool description"
        );
        assert_eq!(
            schema["properties"]["tasks"]["items"]["required"],
            serde_json::json!(["summary", "prompt"])
        );
        assert_eq!(
            schema["properties"]["tasks"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );
    }
}

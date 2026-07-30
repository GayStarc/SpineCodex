<h1 align="center">SpineCodex: Just-in-Time Context Tree Compilation for Cost-Efficient Long-Horizon Tasks</h1>

<p align="center"><strong>Trees are beautiful. Life begins with division and differentiation.</strong></p>
<p align="center">Maintained by <a href="https://ghabix.github.io">Jiahong Xiang</a> and Kunqiu Chen.</p>

## Quickstart

Install SpineCodex, then run it inside a project directory:

```bash
npm install -g @spinejit/spine-codex
spine-codex
```

The package installs its own `spine-codex` command and does not replace the
official `codex` command.

## Update SpineCodex

Update a global npm installation to the latest release:

```bash
npm install -g @spinejit/spine-codex@latest
spine-codex --version
```

## Experimental features

Run `/experimental` in the TUI to enable either feature. Both are disabled by
default; save the selection and start a new conversation for it to take effect.

- **Spine spawn (`spine_spawn`)** runs differentiated branches concurrently,
  then joins their terminal results into the current Spine tree. It requires
  SpineJIT and is available only for direct, non-Plan model calls.
- **Spinetree memory projection (`spinetree_memory_projection`)** projects
  closed-node memory as read-only Markdown under `.codex/spinetree/` for local
  inspection. It also maintains a derived `USER.md` from the active typed
  rollout history and requires SpineJIT.

## Why SpineCodex

Long Codex tasks tend to hit three practical limits:

- **The context window fills before the work is finished.** Important earlier
  requirements must compete with an ever-growing working history.
- **The agent loses focus and persistence.** It may repeat settled work, drift
  from the original goal, skip unfinished details, or produce a weaker final
  result.
- **Token usage and cost keep growing.** Every turn carries more intermediate
  analysis and tool output, even when much of it is no longer relevant to the
  current step.

These problems share one root cause: base Codex manages working context as a
line. New work is appended until the context approaches its limit, then a broad
compaction rewrites the history. Long sessions alternate between carrying too
much detail and reconstructing many different task scopes as one summary.

## Why a tree?

Long-horizon work is recursive: understand a problem, split it into smaller
problems, solve one, return its result, and continue. A linear context records
when work happened, but not which task owns it or where its result should
return. A task tree preserves those relationships.

```text
linear context:  all details --------------------------------> global compact
tree context:    stable prefix | mem(task A) | mem(task B) | live task details
```

This structure gives SpineCodex four practical advantages:

- **Local compilation with a stable prefix.** A completed branch can be
  replaced by memory without rewriting earlier context. Future requests carry
  fewer repeated details, and the unchanged prefix remains reusable by prompt
  caching.
- **A natural task-memory boundary.** Each child owns its local exploration.
  When it finishes, the decisions and result needed by the parent return as
  Node Memory, while the live task keeps the model's detailed attention.
- **A divide-and-conquer scaling boundary.** Routine work can stay shallow;
  difficult work can split recursively and receive more test-time reasoning
  where it is needed.
- **A natural Agent Spawn boundary.** A node already defines the child task,
  its inherited context, and the memory it returns. SpineCodex uses the same
  boundary for independent child agents through the experimental `spine_spawn`
  feature.

The tree does not make the model's context window infinite. It makes more of
that window useful by compiling completed work throughout the task instead of
waiting for one global rewrite.

## SpineJIT: an online LR(0) context compiler

Source code arrives as a linear token stream, but a compiler recovers its
nested syntax tree. Agent work has the same mismatch: messages and tool calls
arrive as a linear stream, while the task itself is hierarchical.

SpineJIT defines a small context grammar and runs an online LR(0) parser over
the agent stream:

```text
linear agent events
  -> context tokens
  -> LR(0) shift/reduce
  -> task tree
  -> model context

Nodes    -> Node | Nodes Node
Node     -> msg | toolcall | TaskTree
TaskTree -> open Nodes close
```

The task tree is not a branching UI layered on top of a transcript. It is the
parse tree of the context language and the state from which the next model
context is compiled.

Ordinary messages and completed tool calls are shifted into the live task.
`spine.open` shifts an `open` boundary and starts a child. When
`spine.close` completes the grammar rule `open Nodes close`, the parser reduces
the entire subtree to one `TaskTree`, compiles its detailed suffix into Node
Memory, and returns to the parent. `spine.next` is the same reduce followed by
the next sibling's `open`.

The parser uses structured control events rather than guessing task boundaries
from prose. Closed subtrees project as memory; the live path projects in full
detail; the context before the reduced suffix stays unchanged. That projection
is the compiler output.

This is **Just-in-Time Context Compilation**: parsing happens incrementally as
the trajectory grows, and compilation happens exactly when a task becomes
complete. Unlike threshold-driven global compaction, SpineJIT performs a local,
grammar-driven reduce at the semantic boundary where the information is still
well scoped.

## Using SpineCodex

Run SpineCodex like Codex. SpineJIT is enabled by default, and the agent manages
the task tree as it works; there is no tree to maintain by hand. Use
`/spine-tree` in the TUI to inspect the current tree and live task.

## Project

SpineCodex is an independently maintained fork based on and derived from
[OpenAI Codex](https://github.com/openai/codex). It is not the official OpenAI
Codex CLI or the official `@openai/codex` npm package.

- [Source](https://github.com/GhabiX/SpineCodex)
- [Releases](https://github.com/GhabiX/SpineCodex/releases)
- [Issues](https://github.com/GhabiX/SpineCodex/issues)
- [Contributing](./docs/contributing.md)
- [Installing and building from source](./docs/install.md)
- [Spine feedback and privacy](./docs/spine-feedback.md)
- [Upstream Codex documentation](https://developers.openai.com/codex)

SpineCodex is licensed under the [Apache-2.0 License](LICENSE). OpenAI Codex
and other derived components retain their attribution in [NOTICE](NOTICE).

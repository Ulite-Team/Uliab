# AGENTS.md — Ulite Team Build Tooling

This file governs how an AI coding agent (Claude Code, OpenCode, or any
other agent) works in any repository under the **Ulite Team** GitHub
organization related to the `ulb` build tool: the core language/evaluator,
`tree-sitter-ulb`, and `ulb-lsp`. These repos are the foundation of a
larger effort (an eventual Android IDE), so code quality here is not
negotiable — every line either survives into that IDE or gets replaced by
someone reading it and trusting it less. Follow this file exactly. If any
instruction here conflicts with a convenience shortcut, this file wins.

## 0. Prime directive

Never produce code, comments, or documentation whose only purpose is to
*look* finished. A reviewer (human or future agent) reading this codebase
cold should never be able to tell which parts were written by an AI. That
means:

- No stub functions that return a plausible-looking default instead of
  doing the work (`todo!()`, `unimplemented!()`, returning `Ok(())` /
  `vec![]` / `None` where real logic belongs).
- No `// TODO` comments as a substitute for either finishing the work or
  explicitly declaring it out of scope in the tracking doc (see §3).
- No comments that describe what the code obviously does
  (`// increment the counter` above `counter += 1`). Comments explain
  *why*, not *what*.
- No silent `unwrap()`/`expect()` on inputs that can realistically fail
  (parser input, file I/O, network) outside of tests. Use `Result` and
  propagate with context (`anyhow::Context` / `thiserror`).
- No catch-all `Err(_) => {}` / swallowed errors. Every error path either
  surfaces to the caller or is logged with enough context to debug it.
- If a task genuinely cannot be finished in one session, stop at a clean,
  compiling boundary and record exactly what's left in `PROGRESS.md`
  (§3) — never leave a half-finished function that compiles by accident.

## 1. Session & budget discipline

The human operating this agent has a **daily budget of ~980K tokens per
10-hour window** and a **200K context window**. Waste is expensive and
literally blocks further work that day. Behave accordingly:

- **One task at a time.** Pick exactly one numbered item from the active
  spec/prompt (or one item from `PROGRESS.md`'s "Next up" list). Finish
  it — compiling, tested, documented — before starting another. Do not
  fan out across unrelated files "while you're in there."
- **Read narrowly.** Don't re-read entire files or directories you've
  already seen this session unless you have reason to believe they
  changed. Use `grep`/targeted `view` with line ranges instead of full-file
  dumps when you only need one function.
- **Edit, don't regenerate.** Prefer `str_replace`-style targeted edits
  over rewriting a whole file when only part of it changes. A full
  rewrite is for new files or genuine structural rewrites only.
- **No speculative scope.** Don't implement features, abstractions, or
  "while I'm here" refactors that weren't asked for in the current task.
  If you notice something that should change elsewhere, note it in
  `PROGRESS.md` under "Observed issues" — don't go fix it unprompted.
- **Checkpoint before running out of room.** If you sense the context
  window is filling up mid-task, stop at the nearest clean boundary,
  commit, and update `PROGRESS.md` rather than pushing through with
  degraded attention to detail.

## 2. Repository map

Three repositories under the `Ulite-Team` GitHub org:

| Repo | Contents | Depends on |
|---|---|---|
| `Uliab` (core) | `ulb-lang` crate (lexer/parser/AST/evaluator), the `uliab` CLI build engine, `GRAMMAR.md`, `ARCHITECTURE.md` | — |
| `tree-sitter-ulb` | `grammar.js`, `highlights.scm`, `folds.scm`, `indents.scm` | `GRAMMAR.md` from core (read-only reference, not a build dependency) |
| `ulb-lsp` | LSP server binary | `ulb-lang` crate from `Uliab` (as a real dependency — via path during development, via a published/vendored version once versioning exists) |

Never let `tree-sitter-ulb` or `ulb-lsp` silently drift from the grammar
defined in `Uliab/GRAMMAR.md`. Any change to the grammar in one repo
requires a `PROGRESS.md` entry in the *other* repos noting they're now
out of sync, even if you can't fix them in the same session.

## 3. Cross-session task tracking

Every repo keeps a `PROGRESS.md` at its root. This is the source of truth
for "what's actually done" — not commit messages, not this file. Format:

```markdown
# Progress

## Done
- [x] Lexer: tokenizes all literals, keywords, comments (2026-08-12)
- [x] Parser: `android {}` block, `deps {}` block (2026-08-12)

## In progress
- [ ] Parser: `convention {}` block — statements done, `apply` resolution not started

## Next up (priority order)
1. Finish convention `apply` resolution
2. Evaluator skeleton for ModuleModel output

## Observed issues (not fixed, noted for later)
- `env()` builtin has no test for missing-var case

## Explicitly deferred (out of scope per AGENTS.md §0)
- KSP support — deferred to phase 2 per original spec
```

At the start of any session, an agent must read `PROGRESS.md` before
touching code. At the end of any task (or when stopping mid-task per §1),
update it. An empty or stale `PROGRESS.md` after a work session is itself
a bug.

## 4. Definition of Done (every task, no exceptions)

A task is not done until **all** of the following are true. Run them in
this order — don't claim completion before running them:

1. `cargo build --all-targets` succeeds with zero warnings (`#![warn(missing_docs)]` enabled on every public crate — see §5).
2. `cargo clippy --all-targets --all-features -- -D warnings` passes clean.
3. `cargo fmt --check` passes.
4. `cargo test` passes, including doc-tests (`cargo test --doc`).
5. Any new public function/struct/enum has a rustdoc comment (§5) and, for
   non-trivial ones, a compiling doc-test.
6. `PROGRESS.md` updated (§3).
7. If the change touches the grammar surface, `GRAMMAR.md` updated in the
   same commit — a code change and its spec must never land separately.
8. Touched files are clean of process/AI-tell language per §9 (grep for
   `Phase`, `phase`, `TODO`, `session`, and any AI tool/model names in
   comments and the commit message).

If any step fails, fix it before reporting the task as complete. Do not
report partial success as success.

## 5. Documentation standard (Rust doc comments that compile)

This project uses **doc-tested rustdoc**, not descriptive comments that
happen to sit near code:

- Every public item (`pub fn`, `pub struct`, `pub enum`, `pub trait`) gets
  a `///` doc comment stating what it does, its invariants, and its
  error conditions (`# Errors` section for anything returning `Result`).
- Non-trivial public functions get a `# Examples` section with a real,
  compiling example wrapped in ` ```rust ... ``` ` — these run under
  `cargo test --doc` and must pass. An example that doesn't compile is
  worse than no example: it lies.
- Modules get a `//!` doc comment at the top explaining their role in the
  crate (e.g. `parser.rs`: "Recursive-descent parser producing a
  span-annotated AST; see GRAMMAR.md for the language this implements").
- Every crate root (`lib.rs`) has `#![warn(missing_docs)]` — this makes
  missing documentation a build warning, which §4 step 1 then catches
  automatically. Do not remove or downgrade this lint to make a build
  pass faster.
- Example, the plugin/convention system specifically: when you implement
  the `convention {}` resolution logic, the deliverable includes rustdoc
  on the resolver type explaining *how* convention lookup and `apply`
  precedence work (not just "resolves conventions"), plus a doc-test
  showing one convention being applied and the resulting `ModuleModel`
  fields it sets. That doc-test is also effectively a regression test —
  treat it as one.

## 6. Verification the agent must actually run (not assume)

Never state that code "should work," "should compile," or "should pass
tests" without having run the command and read its output this session.
If a tool call to run `cargo build`/`cargo test`/`cargo clippy` fails or
is unavailable, say so explicitly rather than presenting unverified code
as finished.

For parser/grammar work specifically: add a snapshot or assertion test
for every grammar construct touched, in the same commit, not "left for
later." A parser change with no corresponding test is treated as
incomplete regardless of whether it compiles.

## 7. Commit discipline

- Every commit message must follow the Conventional Commits shape
  `type(scope): title` (e.g. `feat(jvm): wire KSP through kotlinc`,
  `fix(resolver): ...`, `docs(architecture): ...`, `test(task-engine):
  ...`). The public history is conventional and must stay that way — no
  untyped, free-form messages.
- Every new feature or new roadmap phase is developed on a dedicated
  feature branch (`<scope>/<topic>`, e.g. `feat/android-plugin`), never
  directly on `main`, and lands through a pull request that has passed
  §4's checklist. `main` only receives merged PRs.
- One task from `PROGRESS.md` → one commit (or a small tight series of
  commits if the task is naturally split into build → test → docs).
  Never bundle unrelated changes into one commit.
- Commit message states what changed and why, not "AI: update files" or
  similar. Write it as if a human teammate will read it in a year.
- Never commit code that fails §4's checklist, even as a "WIP" commit on
  a feature branch meant for later cleanup — a broken intermediate state
  is fine locally, but don't push it.

## 7b. Post-task workflow: push, PR, CI, review

A task is not truly complete until the following steps finish — not just
"committed locally":

1. **Push** the feature branch to origin.
2. **Create a pull request** targeting `main` via `gh pr create`. The PR
   title follows §7's conventional-commits style. The PR body lists what
   changed, the DSL syntax (when applicable), a checklist mirroring §4,
   and test counts.
3. **Watch CI** — do not proceed to review until the CI run on the PR
   branch is green. If CI fails, fix the failure, push, and re-watch
   before continuing.
4. **Open 5 parallel review sub-agents** (using the `task` tool with
   `subagent_type: general` or `explore` as appropriate), each reviewing
   the PR diff from a different angle:
   - **Agent 1 — Correctness**: logic bugs, edge cases, error handling,
     wrong assumptions.
   - **Agent 2 — Architecture**: does the change fit the existing
     module/plugin/boundary model, does it introduce coupling that
     shouldn't exist, does it match ARCHITECTURE.md.
   - **Agent 3 — Testing**: coverage gaps, missing negative tests,
     test names that don't describe what they assert, untested error
     paths.
   - **Agent 4 — Docs & grammar**: rustdoc completeness (§5),
     GRAMMAR.md/ARCHITECTURE.md consistency, comments that explain
     why not what, process-tell remnants (§9).
   - **Agent 5 — Security & hygiene**: secrets/keys in code, path
     traversal, unwrap on fallible inputs, swallowed errors, clippy
     lint regressions, dead code.
5. **Read all 5 review results** and execute every actionable
   finding — fix, push, re-watch CI. Dismiss findings only with a
   written rationale (e.g. "false positive because …"). A finding the
   agent cannot evaluate is escalated to the user, not silently
   dropped.
6. **Do NOT comment on the PR using the user's GitHub account.**
   PR comments must only be made from a dedicated CI/action account
   (e.g. `github-actions[bot]`). If no action account is available,
   report the review summary to the user in the terminal instead —
   never post as the user. Default to terminal-only reporting.
7. Only after CI is green and all review findings are resolved or
   dismissed does the agent report the task as complete to the user.

Skip any of these steps only when the user explicitly says so.

## 8. Escalation

If a task in the active spec is ambiguous, underspecified, or seems to
conflict with something already built, stop and ask — do not guess
silently and do not pick the interpretation that's fastest to implement.
Silent guessing is exactly the failure mode this file exists to prevent.

## 9. Public surface vs internal tracking — never mix them

`PROGRESS.md` and this file are **internal**: they exist so an agent
picking up the work in a later session (possibly a different agent,
possibly a different model) knows exactly where things stand. Process
language belongs there and only there — phase numbers, "next up" lists,
model/tool names, session notes, token-budget reasoning.

Everything a person outside this loop can see — **commit messages, PR
descriptions, code comments (`///`, `//!`, `//`), README/CHANGELOG
prose, GRAMMAR.md/ARCHITECTURE.md prose** — is public surface. It must
read like a competent engineer wrote it for its own sake, with zero
trace of the process that produced it. Concretely:

- **Never** reference "Phase N", "this session", "as requested", "the
  spec says", or similar meta-language in a commit message or code
  comment. If a doc comment needs to explain *why* something is built a
  certain way, ground it in the language/domain reasoning itself (e.g.
  "the parser is hand-written so error messages can be tailored"), never
  in "because Phase 3 of the plan said so."
- **Never** name the model, tool, or agent that produced the change
  ("Claude", "GPT", "an AI wrote this", "Sonnet", "M3", version strings
  of the assistant, etc.) anywhere in a commit, comment, or doc file.
  This includes indirectly — a comment like "works as of the model
  available when this was written" is still a tell; delete rather than
  hedge.
- **Never** leave a comment true only "for now" without saying so in
  domain terms. `ast.rs`'s old note "The parser (Phase 3) builds this
  AST from the token stream. Until then the types are exercised
  directly" is exactly the anti-pattern: once the parser existed, that
  sentence became simultaneously stale *and* a giveaway that it had been
  written against an internal plan. Prefer comments that stay true
  regardless of what's implemented yet ("The parser builds this AST from
  the token stream (see `parser.rs`)") over comments that narrate the
  build's own history.
- Before closing out any task, grep the files you touched for `Phase`,
  `phase`, `TODO`, `session`, and the names of any AI tools/models,
  the same way §4's checklist runs `cargo build`/`clippy`/`fmt` — this is
  part of Definition of Done, not optional polish.
- Commit messages describe the *change*, in the same voice a senior
  engineer would use reviewing their own diff before pushing: what
  changed, why, in domain terms. "Add recursive-descent parser with
  panic-mode error recovery" — not "Implement Phase 3 as specified."

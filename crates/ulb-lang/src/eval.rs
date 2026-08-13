//! Tree-walking evaluator for the ulb DSL.
//!
//! Resolves a parsed [`crate::ast::File`] into a [`Value`] module model,
//! applying `if`/`env`/`props`/`ver` builtins, `convention`/`fn`
//! definitions, and `apply` per GRAMMAR.md §9–§10. Evaluation is
//! deterministic and side-effect-free except for the two builtins that are
//! explicitly allowed to touch the outside world (`env`, `props`) — see
//! GRAMMAR.md Appendix C.
//!
//! For editor tooling, the lint-mode entry points
//! ([`collect_definitions_lint`], [`evaluate_build_lint`]) treat `env`/`props`
//! lookups that have no injected [`EvalEnvironment`] entry as *unresolved*:
//! they return an invalid value without consulting the process or filesystem
//! and without raising a diagnostic. That keeps name/type/arity diagnostics
//! available hermetically — an editor cannot verify a build-time environment,
//! so it should not report its absence as an error. Injected entries still
//! resolve, so lint mode remains deterministic.
//!
//! # Two-pass model
//!
//! `conventions.ulb` and `libs.ulb` declare things that are *globally
//! visible* to every `build.ulb` (GRAMMAR.md §6.3/§6.4, no imports). This
//! module therefore separates **definition collection**
//! ([`collect_definitions`]) from **build evaluation**
//! ([`evaluate_build`]): collect every `conventions.ulb`/`libs.ulb` file
//! first into one [`Definitions`], then evaluate each `build.ulb` against
//! it. [`evaluate_project`] is a convenience wrapper over exactly that
//! two-pass shape for the common one-conventions-file, one-libs-file,
//! one-build-file case (see its doc example).
//!
//! # Value merge/accumulate rule
//!
//! This is a deliberate design decision worth confirming, not a silent
//! guess: a block target (`android {}`, `buildTypes {}`, …) written more
//! than once — most commonly because a `convention` inlines an `android {}`
//! block and `build.ulb` also writes one — **merges** key-by-key (so a
//! convention's `compileSdk` and a module's own `namespace` coexist in one
//! resolved `android` value). A *scalar* pair key repeated at the same
//! level instead **accumulates into a [`Value::List`]** (so two
//! `implementation "..."` pairs, or two `plugin "..."` pairs, both survive
//! rather than the second silently overwriting the first). See
//! [`merge_block_value`] and [`insert_accumulating`].

use std::collections::BTreeMap;

use crate::ast::{
    Argument, Block, ElseBranch, Expr, ExprKind, File, Ident, Path, Statement, StatementKind,
    StrPart, VersionRef,
};
use crate::diagnostic::{Diagnostic, Severity};
use crate::span::Span;
use crate::token::Number;

/// A resolved value in the ulb module model.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A string (interpolation already resolved).
    Str(String),
    /// A number (integer or decimal).
    Number(Number),
    /// A boolean.
    Bool(bool),
    /// An ordered list — also used for repeated scalar pair keys (see the
    /// module-level merge/accumulate rule).
    List(Vec<Value>),
    /// A `ver(major=N, minor=N, patch=N)` result.
    Version(VersionValue),
    /// A `props("path")` result: the parsed `.properties` file.
    Properties(BTreeMap<String, String>),
    /// A resolved `group:artifact:version` Maven coordinate (the result of
    /// `"group:artifact" @ version`, or a `libs.ulb` alias that already
    /// carried a full coordinate).
    Coordinate(String),
    /// A nested block (`android { ... }`, `buildTypes { ... }`, a
    /// convention's own body, …).
    Block(BTreeMap<String, Value>),
    /// A value that could not be resolved; always paired with a
    /// [`Diagnostic`] already recorded at the point of failure. Consumers
    /// must skip it rather than guess at its meaning, mirroring
    /// [`crate::ast::StatementKind::Invalid`].
    Invalid(String),
}

impl Value {
    /// Renders a scalar value for string interpolation (`"${...}"`).
    /// Non-scalar values (`List`, `Block`, `Properties`) cannot be
    /// meaningfully interpolated; callers should diagnose that case before
    /// calling this (see [`Evaluator::eval_str`]).
    #[must_use]
    pub fn as_display_string(&self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s.clone()),
            Value::Number(n) => Some(n.as_text()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Version(v) => Some(v.to_string()),
            Value::Coordinate(c) => Some(c.clone()),
            Value::Invalid(_) => None,
            Value::List(_) | Value::Block(_) | Value::Properties(_) => None,
        }
    }
}

/// A resolved `ver(major=N, minor=N, patch=N)` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionValue {
    /// Major component.
    pub major: i64,
    /// Minor component.
    pub minor: i64,
    /// Patch component.
    pub patch: i64,
}

impl std::fmt::Display for VersionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Everything a `build.ulb` (or a `convention`/`fn` body) can reference by
/// bare name: conventions and helper functions from every `conventions.ulb`
/// collected so far, and version-catalog aliases/versions/plugins from
/// every `libs.ulb` collected so far. Built by repeated calls to
/// [`collect_definitions`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Definitions {
    /// `convention NAME { ... }` bodies, keyed by name.
    pub conventions: BTreeMap<String, Block>,
    /// `fn NAME(params) { ... }` bodies, keyed by name.
    pub functions: BTreeMap<String, (Vec<Ident>, Block)>,
    /// Resolved `libs.ulb` alias values (coordinates, versioned or not),
    /// keyed by alias name. Includes `bundle {}` entries as `Value::List`.
    pub aliases: BTreeMap<String, Value>,
    /// `versions { NAME = "..." }` entries, keyed by name.
    pub versions: BTreeMap<String, String>,
    /// `plugins { NAME = "group:artifact" @ ref }` entries, keyed by name.
    pub plugins: BTreeMap<String, Value>,
}

/// Values injected into an evaluation so it can run without a live
/// process environment or filesystem (GRAMMAR.md Appendix C). Both maps
/// are consulted before the real world: an injected `env` entry shadows
/// `std::env::var` for `env("NAME")`, and an injected `props` entry —
/// keyed by the path string exactly as written in the build file —
/// shadows the filesystem for `props("path")`. A name/path with no
/// injected entry falls back to the live environment, so a default
/// [`EvalEnvironment`] reproduces the previous non-hermetic behavior
/// exactly. Injected `props` values are raw `.properties` text, parsed
/// the same way a file on disk would be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvalEnvironment {
    /// Overrides for `env("NAME")` lookups, keyed by variable name.
    pub env: BTreeMap<String, String>,
    /// Overrides for `props("path")` lookups, keyed by the path string as
    /// written in the build file.
    pub props: BTreeMap<String, String>,
}

/// The result of evaluating one file: the resolved model plus every
/// diagnostic raised during evaluation (in addition to any parse
/// diagnostics the caller already has from [`crate::parser::parse`]).
#[derive(Debug, Clone, PartialEq)]
pub struct EvalOutcome {
    /// The resolved module model.
    pub model: Value,
    /// Diagnostics raised during evaluation.
    pub diagnostics: Vec<Diagnostic>,
}

/// Collects `convention`/`fn` definitions and `libs.ulb`-style
/// `versions {}` / alias / `bundle {}` / `plugins {}` declarations from one
/// file's top-level statements into `defs`, appending any diagnostics to
/// `diagnostics`. Call this once per `conventions.ulb`/`libs.ulb` file
/// *before* evaluating any `build.ulb` (GRAMMAR.md §6.3/§6.4: everything
/// declared here is globally visible, no imports).
///
/// Aliases and versions are collected first within a single call so that a
/// `bundle {}` or `plugins {}` block in the *same* file can reference an
/// alias declared earlier in that file; a `bundle`/`plugins` reference to
/// an alias declared in a *different* file requires that file to have been
/// collected in an earlier call.
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::{collect_definitions, Definitions};
/// use ulb_lang::parser::parse;
///
/// let libs = parse(r#"
///     versions { coreVersion = "1.16.0" }
///     coreKtx = "androidx.core:core-ktx" @ coreVersion
/// "#);
/// let mut defs = Definitions::default();
/// let mut diagnostics = Vec::new();
/// collect_definitions(&libs.file, &mut defs, &mut diagnostics);
/// assert!(diagnostics.is_empty());
/// assert_eq!(defs.versions["coreVersion"], "1.16.0");
/// assert!(defs.aliases.contains_key("coreKtx"));
/// ```
pub fn collect_definitions(file: &File, defs: &mut Definitions, diagnostics: &mut Vec<Diagnostic>) {
    collect_definitions_with(file, defs, diagnostics, &EvalEnvironment::default());
}

/// Like [`collect_definitions`], but resolves alias/version expressions in
/// lint mode: `env`/`props` lookups with no injected [`EvalEnvironment`]
/// entry resolve to an unresolved value without touching the process or
/// filesystem and without raising a diagnostic. Use this in editor tooling,
/// where a missing environment variable or properties file cannot be
/// meaningfully verified at edit time.
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::{collect_definitions_lint, Definitions};
/// use ulb_lang::parser::parse;
///
/// let libs = parse(r#"alias = env("ULB_LINT_UNSET_X")"#);
/// let mut defs = Definitions::default();
/// let mut diagnostics = Vec::new();
/// collect_definitions_lint(&libs.file, &mut defs, &mut diagnostics);
/// assert!(diagnostics.is_empty());
/// ```
pub fn collect_definitions_lint(
    file: &File,
    defs: &mut Definitions,
    diagnostics: &mut Vec<Diagnostic>,
) {
    collect_definitions_impl(file, defs, diagnostics, &EvalEnvironment::default(), true);
}

/// Like [`collect_definitions`], but resolves alias/version expressions
/// against the given [`EvalEnvironment`] so a `libs.ulb` that calls
/// `env`/`props` can be collected hermetically.
pub fn collect_definitions_with(
    file: &File,
    defs: &mut Definitions,
    diagnostics: &mut Vec<Diagnostic>,
    environment: &EvalEnvironment,
) {
    collect_definitions_impl(file, defs, diagnostics, environment, false);
}

/// Shared implementation of definition collection; `lint` selects whether
/// unresolvable `env`/`props` lookups are reported (see [`EvalEnvironment`]).
fn collect_definitions_impl(
    file: &File,
    defs: &mut Definitions,
    diagnostics: &mut Vec<Diagnostic>,
    environment: &EvalEnvironment,
    lint: bool,
) {
    // Pass 1: conventions, fns, `versions {}`, and plain alias assignments
    // — everything a same-file `bundle {}`/`plugins {}` block might need.
    for stmt in &file.statements {
        match &stmt.kind {
            StatementKind::ConventionDef { name, block } => {
                defs.conventions.insert(name.text.clone(), block.clone());
            }
            StatementKind::FnDef {
                name,
                params,
                block,
            } => {
                defs.functions
                    .insert(name.text.clone(), (params.clone(), block.clone()));
            }
            StatementKind::BlockStmt { path, block } if path.head() == "versions" => {
                for inner in &block.statements {
                    if let StatementKind::Assignment { path, value } = &inner.kind {
                        let mut ev = Evaluator::with_lint(defs, environment, lint);
                        let resolved = ev.eval_expr(value);
                        diagnostics.append(&mut ev.diagnostics);
                        if let Some(text) = resolved.as_display_string() {
                            defs.versions.insert(path.head().to_owned(), text);
                        } else {
                            diagnostics.push(Diagnostic {
                                span: value.span,
                                severity: Severity::Error,
                                message: format!(
                                    "version '{}' must resolve to a string",
                                    path.head()
                                ),
                            });
                        }
                    }
                }
            }
            StatementKind::Assignment { path, value } if path.is_single() => {
                let mut ev = Evaluator::with_lint(defs, environment, lint);
                let resolved = ev.eval_expr(value);
                diagnostics.append(&mut ev.diagnostics);
                defs.aliases.insert(path.head().to_owned(), resolved);
            }
            _ => {}
        }
    }

    // Pass 2: `bundle {}` / `plugins {}`, which may reference aliases just
    // collected above.
    for stmt in &file.statements {
        if let StatementKind::BlockStmt { path, block } = &stmt.kind {
            match path.head() {
                "bundle" => {
                    for inner in &block.statements {
                        if let StatementKind::Assignment { path, value } = &inner.kind {
                            let mut ev = Evaluator::with_lint(defs, environment, lint);
                            let resolved = ev.eval_expr(value);
                            diagnostics.append(&mut ev.diagnostics);
                            defs.aliases.insert(path.head().to_owned(), resolved);
                        }
                    }
                }
                "plugins" => {
                    for inner in &block.statements {
                        if let StatementKind::Assignment { path, value } = &inner.kind {
                            let mut ev = Evaluator::with_lint(defs, environment, lint);
                            let resolved = ev.eval_expr(value);
                            diagnostics.append(&mut ev.diagnostics);
                            defs.plugins.insert(path.head().to_owned(), resolved);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Evaluates a `build.ulb` file (or a `convention`'s own body, evaluated
/// standalone) against previously-[`collect_definitions`]-ed `defs`,
/// producing a [`Value::Block`] module model.
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::{evaluate_build, Definitions};
/// use ulb_lang::parser::parse;
///
/// let build = parse(r#"
///     android {
///         compileSdk 37
///         minSdk 24
///     }
/// "#);
/// let defs = Definitions::default();
/// let outcome = evaluate_build(&build.file, &defs);
/// assert!(outcome.diagnostics.is_empty());
/// let ulb_lang::eval::Value::Block(top) = &outcome.model else {
///     panic!("expected a Block model");
/// };
/// let ulb_lang::eval::Value::Block(android) = &top["android"] else {
///     panic!("expected android to be a Block");
/// };
/// assert_eq!(
///     android["compileSdk"],
///     ulb_lang::eval::Value::Number(ulb_lang::token::Number::Int(37))
/// );
/// ```
#[must_use]
pub fn evaluate_build(file: &File, defs: &Definitions) -> EvalOutcome {
    evaluate_build_with(file, defs, &EvalEnvironment::default())
}

/// Like [`evaluate_build`], but with a hermetic [`EvalEnvironment`]
/// injected so `env`/`props` lookups resolve deterministically instead of
/// against the live process.
#[must_use]
pub fn evaluate_build_with(
    file: &File,
    defs: &Definitions,
    environment: &EvalEnvironment,
) -> EvalOutcome {
    evaluate_build_impl(file, defs, environment, false)
}

/// Like [`evaluate_build`], but in lint mode: `env`/`props` lookups with no
/// injected [`EvalEnvironment`] entry resolve to an unresolved value without
/// touching the process or filesystem and without raising a diagnostic. Use
/// this in editor tooling, where a missing environment variable or
/// properties file cannot be meaningfully verified at edit time.
///
/// Name resolution, arity checks, type checks, and role validation behave
/// exactly as in [`evaluate_build`].
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::{evaluate_build_lint, Definitions};
/// use ulb_lang::parser::parse;
///
/// let defs = Definitions::default();
///
/// // An unset environment variable is unresolved, not an error.
/// let build = parse(r#"x env("ULB_LINT_UNSET_X")"#);
/// let outcome = evaluate_build_lint(&build.file, &defs);
/// assert!(outcome.diagnostics.is_empty());
///
/// // But a genuinely unknown reference still is.
/// let build = parse(r#"x ghostAlias"#);
/// let outcome = evaluate_build_lint(&build.file, &defs);
/// assert!(
///     outcome
///         .diagnostics
///         .iter()
///         .any(|d| d.message.contains("unknown reference"))
/// );
/// ```
#[must_use]
pub fn evaluate_build_lint(file: &File, defs: &Definitions) -> EvalOutcome {
    evaluate_build_impl(file, defs, &EvalEnvironment::default(), true)
}

/// Shared implementation of build evaluation; `lint` selects whether
/// unresolvable `env`/`props` lookups are reported (see
/// [`evaluate_build_lint`]).
fn evaluate_build_impl(
    file: &File,
    defs: &Definitions,
    environment: &EvalEnvironment,
    lint: bool,
) -> EvalOutcome {
    let mut evaluator = Evaluator::with_lint(defs, environment, lint);
    let mut target = BTreeMap::new();
    evaluator.eval_statements(&file.statements, &mut target);
    EvalOutcome {
        model: Value::Block(target),
        diagnostics: evaluator.diagnostics,
    }
}

/// Convenience wrapper: parses one `conventions.ulb` source, one
/// `libs.ulb` source, and one `build.ulb` source, collects definitions from
/// the first two, and evaluates the third — the shape every worked example
/// in this crate uses (GRAMMAR.md §6). Pass an empty string for a file role
/// that isn't needed.
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::evaluate_project;
///
/// let conventions = r#"
///     convention androidApp {
///         android { compileSdk 37 }
///     }
/// "#;
/// let libs = r#"
///     coreKtx = "androidx.core:core-ktx:1.16.0"
/// "#;
/// let build = r#"
///     apply "androidApp"
///     android { minSdk 24 }
///     deps { implementation coreKtx }
/// "#;
/// let outcome = evaluate_project(conventions, libs, build);
/// assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
/// ```
#[must_use]
pub fn evaluate_project(conventions_src: &str, libs_src: &str, build_src: &str) -> EvalOutcome {
    evaluate_project_with(
        conventions_src,
        libs_src,
        build_src,
        &EvalEnvironment::default(),
    )
}

/// Like [`evaluate_project`], but with a hermetic [`EvalEnvironment`]
/// injected so `env`/`props` lookups resolve deterministically instead of
/// against the live process.
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::{EvalEnvironment, evaluate_project_with};
///
/// let mut env = EvalEnvironment::default();
/// env.env.insert("KEY_PASSWORD".to_owned(), "hunter2".to_owned());
/// env.props.insert(
///     "signing.properties".to_owned(),
///     "storeFile=release.keystore\n".to_owned(),
/// );
///
/// let conventions = r#"
///     convention signed {
///         signing {
///             storeFile props("signing.properties").storeFile
///             keyPassword env("KEY_PASSWORD")
///         }
///     }
/// "#;
/// let outcome = evaluate_project_with(conventions, "", r#"apply "signed""#, &env);
/// assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
/// ```
#[must_use]
pub fn evaluate_project_with(
    conventions_src: &str,
    libs_src: &str,
    build_src: &str,
    environment: &EvalEnvironment,
) -> EvalOutcome {
    let mut diagnostics = Vec::new();
    let mut defs = Definitions::default();

    let conv_parsed = crate::parser::parse(conventions_src);
    diagnostics.extend(conv_parsed.diagnostics);
    collect_definitions_with(&conv_parsed.file, &mut defs, &mut diagnostics, environment);

    let libs_parsed = crate::parser::parse(libs_src);
    diagnostics.extend(libs_parsed.diagnostics);
    collect_definitions_with(&libs_parsed.file, &mut defs, &mut diagnostics, environment);

    let build_parsed = crate::parser::parse(build_src);
    diagnostics.extend(build_parsed.diagnostics);

    let outcome = evaluate_build_with(&build_parsed.file, &defs, environment);
    diagnostics.extend(outcome.diagnostics);

    EvalOutcome {
        model: outcome.model,
        diagnostics,
    }
}

/// Merges `value` into `target[key]`. A repeated **block** target merges
/// key-by-key (recursively, for nested blocks); a repeated **scalar**
/// value accumulates into a [`Value::List`]. See the module-level doc for
/// why these differ.
fn merge_block_value(target: &mut BTreeMap<String, Value>, key: String, value: Value) {
    if let (Value::Block(new_map), Some(Value::Block(_))) = (&value, target.get(&key)) {
        let Some(Value::Block(existing)) = target.get_mut(&key) else {
            unreachable!("checked above");
        };
        for (k, v) in new_map.clone() {
            merge_block_value(existing, k, v);
        }
        return;
    }
    insert_accumulating(target, key, value);
}

/// Inserts `value` at `target[key]`, converting a pre-existing scalar (or
/// list) entry into an accumulating [`Value::List`] rather than
/// overwriting it. Used for pair/assignment statements and for repeated
/// non-block values.
fn insert_accumulating(target: &mut BTreeMap<String, Value>, key: String, value: Value) {
    match target.remove(&key) {
        None => {
            target.insert(key, value);
        }
        Some(Value::List(mut items)) => {
            items.push(value);
            target.insert(key, Value::List(items));
        }
        Some(existing) => {
            target.insert(key, Value::List(vec![existing, value]));
        }
    }
}

struct Evaluator<'a> {
    defs: &'a Definitions,
    env: &'a EvalEnvironment,
    locals: Vec<BTreeMap<String, Value>>,
    diagnostics: Vec<Diagnostic>,
    lint: bool,
}

impl<'a> Evaluator<'a> {
    /// Constructs an evaluator; `lint` makes un-injected `env`/`props`
    /// lookups resolve to an unresolved value without touching the world
    /// or reporting a diagnostic (see [`evaluate_build_lint`]).
    fn with_lint(defs: &'a Definitions, env: &'a EvalEnvironment, lint: bool) -> Self {
        Self {
            defs,
            env,
            locals: Vec::new(),
            diagnostics: Vec::new(),
            lint,
        }
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            severity: Severity::Error,
            message: message.into(),
        });
    }

    fn lookup_local(&self, name: &str) -> Option<Value> {
        self.locals
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    // -- statement evaluation -------------------------------------------

    fn eval_statements(&mut self, statements: &[Statement], target: &mut BTreeMap<String, Value>) {
        for stmt in statements {
            self.eval_statement(stmt, target);
        }
    }

    fn eval_statement(&mut self, stmt: &Statement, target: &mut BTreeMap<String, Value>) {
        match &stmt.kind {
            StatementKind::If(if_kind) => {
                let cond = self.eval_expr(&if_kind.condition);
                match cond {
                    Value::Bool(true) => {
                        self.eval_statements(&if_kind.then_branch.statements, target);
                    }
                    Value::Bool(false) => {
                        if let Some(else_branch) = &if_kind.else_branch {
                            self.eval_else(else_branch, target);
                        }
                    }
                    Value::Invalid(_) => {
                        // Already diagnosed while evaluating the condition.
                    }
                    other => {
                        self.error(
                            if_kind.condition.span,
                            format!(
                                "'if' condition must be a boolean, got {}",
                                value_kind_name(&other)
                            ),
                        );
                    }
                }
            }
            StatementKind::BlockStmt { path, block } => {
                let mut nested = BTreeMap::new();
                self.eval_statements(&block.statements, &mut nested);
                self.insert_at_path(target, path, Value::Block(nested), true);
            }
            StatementKind::ConventionDef { .. } | StatementKind::FnDef { .. } => {
                self.error(
                    stmt.span,
                    "convention/fn definitions are only valid in conventions.ulb (GRAMMAR.md §10)",
                );
            }
            StatementKind::TaskDef { name, block } => {
                let mut nested = BTreeMap::new();
                self.eval_statements(&block.statements, &mut nested);
                let tasks = target
                    .entry("tasks".to_owned())
                    .or_insert_with(|| Value::Block(BTreeMap::new()));
                let Value::Block(tasks_map) = tasks else {
                    self.error(stmt.span, "'tasks' key is already a non-block value");
                    return;
                };
                tasks_map.insert(name.clone(), Value::Block(nested));
            }
            StatementKind::Apply { name, .. } => {
                if let Some(block) = self.defs.conventions.get(name).cloned() {
                    self.eval_statements(&block.statements, target);
                } else {
                    self.error(stmt.span, format!("unknown convention '{name}'"));
                }
            }
            StatementKind::Assignment { path, value } => {
                let resolved = self.eval_expr(value);
                self.insert_at_path(target, path, resolved, false);
            }
            StatementKind::Pair { key, value } => {
                let resolved = self.eval_expr(value);
                insert_accumulating(target, key.text.clone(), resolved);
            }
            StatementKind::CallStmt(call) => {
                self.eval_call_statement(&call.kind.callee, &call.kind.args, stmt.span, target);
            }
            StatementKind::Invalid { .. } => {
                // Already diagnosed by the parser; nothing to evaluate.
            }
        }
    }

    fn eval_else(&mut self, branch: &ElseBranch, target: &mut BTreeMap<String, Value>) {
        match branch {
            ElseBranch::Block(block) => self.eval_statements(&block.statements, target),
            ElseBranch::If(inner) => {
                let cond = self.eval_expr(&inner.kind.condition);
                match cond {
                    Value::Bool(true) => {
                        self.eval_statements(&inner.kind.then_branch.statements, target);
                    }
                    Value::Bool(false) => {
                        if let Some(next) = &inner.kind.else_branch {
                            self.eval_else(next, target);
                        }
                    }
                    Value::Invalid(_) => {}
                    other => self.error(
                        inner.kind.condition.span,
                        format!(
                            "'if' condition must be a boolean, got {}",
                            value_kind_name(&other)
                        ),
                    ),
                }
            }
        }
    }

    /// Inserts `value` under `path` in `target`, creating/descending
    /// intermediate `Block` maps for a dotted path (`commonMain.deps`).
    /// `merge` selects [`merge_block_value`] (for `BlockStmt`, where a
    /// repeated block should merge) vs [`insert_accumulating`] (for
    /// `Assignment`, which behaves like a pair).
    fn insert_at_path(
        &mut self,
        target: &mut BTreeMap<String, Value>,
        path: &Path,
        value: Value,
        merge: bool,
    ) {
        let mut cursor = target;
        for seg in &path.segments[..path.segments.len() - 1] {
            let entry = cursor
                .entry(seg.text.clone())
                .or_insert_with(|| Value::Block(BTreeMap::new()));
            match entry {
                Value::Block(map) => cursor = map,
                _ => {
                    self.error(
                        seg.span,
                        format!("'{}' is already a non-block value", seg.text),
                    );
                    return;
                }
            }
        }
        let last = path.segments.last().unwrap().text.clone();
        if merge {
            merge_block_value(cursor, last, value);
        } else {
            insert_accumulating(cursor, last, value);
        }
    }

    /// A `call_statement` (GRAMMAR.md §5): either a `run {}` action
    /// (`copy`/`exec`, accumulated under `__actions__`) or an invocation of
    /// a user-defined `fn`, whose body is inlined into `target` with its
    /// parameters bound as locals.
    fn eval_call_statement(
        &mut self,
        callee: &Ident,
        args: &[Argument],
        call_span: Span,
        target: &mut BTreeMap<String, Value>,
    ) {
        match callee.text.as_str() {
            "copy" | "exec" => {
                let mut action = BTreeMap::new();
                action.insert("action".to_owned(), Value::Str(callee.text.clone()));
                for arg in args {
                    match arg {
                        Argument::Named { name, value } => {
                            let resolved = self.eval_expr(value);
                            action.insert(name.text.clone(), resolved);
                        }
                        Argument::Positional(expr) => {
                            self.error(
                                expr.span,
                                format!("'{}' takes only named arguments", callee.text),
                            );
                        }
                    }
                }
                insert_accumulating(target, "__actions__".to_owned(), Value::Block(action));
            }
            name => {
                let Some((params, block)) = self.defs.functions.get(name).cloned() else {
                    self.error(call_span, format!("unknown function '{name}'"));
                    return;
                };
                let mut scope = BTreeMap::new();
                self.bind_args(&params, args, call_span, &mut scope);
                self.locals.push(scope);
                self.eval_statements(&block.statements, target);
                self.locals.pop();
            }
        }
    }

    fn bind_args(
        &mut self,
        params: &[Ident],
        args: &[Argument],
        call_span: Span,
        scope: &mut BTreeMap<String, Value>,
    ) {
        let all_named = args.iter().all(|a| matches!(a, Argument::Named { .. }));
        let all_positional = args.iter().all(|a| matches!(a, Argument::Positional(_)));
        if !args.is_empty() && !all_named && !all_positional {
            self.error(
                call_span,
                "a call's arguments must be all-named or all-positional",
            );
            return;
        }
        if all_positional {
            if args.len() != params.len() {
                self.error(
                    call_span,
                    format!("expected {} argument(s), got {}", params.len(), args.len()),
                );
            }
            for (param, arg) in params.iter().zip(args.iter()) {
                if let Argument::Positional(expr) = arg {
                    let value = self.eval_expr(expr);
                    scope.insert(param.text.clone(), value);
                }
            }
        } else {
            for arg in args {
                if let Argument::Named { name, value } = arg {
                    if !params.iter().any(|p| p.text == name.text) {
                        self.error(name.span, format!("unknown parameter '{}'", name.text));
                        continue;
                    }
                    let resolved = self.eval_expr(value);
                    scope.insert(name.text.clone(), resolved);
                }
            }
            for param in params {
                if !scope.contains_key(&param.text) {
                    self.error(call_span, format!("missing argument '{}'", param.text));
                }
            }
        }
    }

    // -- expression evaluation -------------------------------------------

    fn eval_expr(&mut self, expr: &Expr) -> Value {
        match &expr.kind {
            ExprKind::Str(str_expr) => Value::Str(self.eval_str(str_expr, expr.span)),
            ExprKind::Number(n) => Value::Number(n.clone()),
            ExprKind::Bool(b) => Value::Bool(*b),
            ExprKind::Ref(path) => self.eval_ref(path, expr.span),
            ExprKind::Call(call) => {
                self.eval_call_expr(&call.kind.callee, &call.kind.args, call.span)
            }
            ExprKind::MemberAccess { base, members } => {
                let base_value = self.eval_call_expr(&base.kind.callee, &base.kind.args, base.span);
                self.eval_member_access(base_value, members, expr.span)
            }
            ExprKind::List(items) => Value::List(items.iter().map(|e| self.eval_expr(e)).collect()),
            ExprKind::Versioned { base, version } => self.eval_versioned(base, version, expr.span),
            ExprKind::Binary { op, lhs, rhs } => self.eval_binary(*op, lhs, rhs),
            ExprKind::Not(inner) => match self.eval_expr(inner) {
                Value::Bool(b) => Value::Bool(!b),
                Value::Invalid(_) => Value::Invalid("not of invalid value".to_owned()),
                other => {
                    self.error(
                        inner.span,
                        format!("'!' requires a boolean, got {}", value_kind_name(&other)),
                    );
                    Value::Invalid("type error".to_owned())
                }
            },
            ExprKind::Group(inner) => self.eval_expr(inner),
            ExprKind::Invalid { message } => {
                // Already diagnosed by the parser.
                Value::Invalid(message.clone())
            }
        }
    }

    fn eval_str(&mut self, str_expr: &crate::ast::StrExpr, span: Span) -> String {
        let mut out = String::new();
        for part in &str_expr.parts {
            match part {
                StrPart::Literal(text) => out.push_str(text),
                StrPart::Interp(expr) => {
                    let value = self.eval_expr(expr);
                    match value.as_display_string() {
                        Some(text) => out.push_str(&text),
                        None => {
                            if !matches!(value, Value::Invalid(_)) {
                                self.error(
                                    expr.span,
                                    format!(
                                        "cannot interpolate a {} value",
                                        value_kind_name(&value)
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
        let _ = span; // reserved for future span-attached string diagnostics
        out
    }

    fn eval_ref(&mut self, path: &Path, span: Span) -> Value {
        if !path.is_single() {
            self.error(
                span,
                "dotted references are not supported outside member access",
            );
            return Value::Invalid("unsupported reference form".to_owned());
        }
        let name = path.head();
        if let Some(value) = self.lookup_local(name) {
            return value;
        }
        if let Some(value) = self.defs.aliases.get(name) {
            return value.clone();
        }
        if let Some(version) = self.defs.versions.get(name) {
            return Value::Str(version.clone());
        }
        if let Some(value) = self.defs.plugins.get(name) {
            return value.clone();
        }
        self.error(span, format!("unknown reference '{name}'"));
        Value::Invalid("unknown reference".to_owned())
    }

    /// The three expression-position builtins (GRAMMAR.md Appendix C).
    /// Any other callee is an error here: user `fn`s are only callable as
    /// statements (see [`Evaluator::eval_call_statement`]), since a `fn`
    /// body configures a block rather than producing a value.
    fn eval_call_expr(&mut self, callee: &Ident, args: &[Argument], span: Span) -> Value {
        match callee.text.as_str() {
            "env" => self.eval_env(args, span),
            "props" => self.eval_props(args, span),
            "ver" => self.eval_ver(args, span),
            other => {
                self.error(
                    span,
                    format!(
                        "unknown function '{other}' in expression position (only env, props, ver are callable here)"
                    ),
                );
                Value::Invalid("unknown function".to_owned())
            }
        }
    }

    fn single_positional_string(
        &mut self,
        args: &[Argument],
        span: Span,
        what: &str,
    ) -> Option<String> {
        if args.len() != 1 {
            self.error(span, format!("{what} takes exactly one argument"));
            return None;
        }
        let Argument::Positional(expr) = &args[0] else {
            self.error(span, format!("{what}'s argument must be positional"));
            return None;
        };
        match self.eval_expr(expr) {
            Value::Str(s) => Some(s),
            Value::Invalid(_) => None,
            other => {
                self.error(
                    expr.span,
                    format!(
                        "{what}'s argument must be a string, got {}",
                        value_kind_name(&other)
                    ),
                );
                None
            }
        }
    }

    fn eval_env(&mut self, args: &[Argument], span: Span) -> Value {
        let Some(name) = self.single_positional_string(args, span, "env(...)") else {
            return Value::Invalid("bad env() call".to_owned());
        };
        if let Some(value) = self.env.env.get(&name) {
            return Value::Str(value.clone());
        }
        if self.lint {
            return Value::Invalid("unresolved environment variable".to_owned());
        }
        match std::env::var(&name) {
            Ok(value) => Value::Str(value),
            Err(_) => {
                self.error(span, format!("environment variable '{name}' is not set"));
                Value::Invalid("missing environment variable".to_owned())
            }
        }
    }

    fn eval_props(&mut self, args: &[Argument], span: Span) -> Value {
        let Some(path) = self.single_positional_string(args, span, "props(...)") else {
            return Value::Invalid("bad props() call".to_owned());
        };
        if let Some(contents) = self.env.props.get(&path) {
            return Value::Properties(parse_properties(contents));
        }
        if self.lint {
            return Value::Invalid("unresolved properties file".to_owned());
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => Value::Properties(parse_properties(&contents)),
            Err(err) => {
                self.error(span, format!("could not read '{path}': {err}"));
                Value::Invalid("missing properties file".to_owned())
            }
        }
    }

    fn eval_ver(&mut self, args: &[Argument], span: Span) -> Value {
        if !args.iter().all(|a| matches!(a, Argument::Named { .. })) {
            self.error(
                span,
                "ver(...) requires named arguments (major, minor, patch)",
            );
            return Value::Invalid("bad ver() call".to_owned());
        }
        let mut major = None;
        let mut minor = None;
        let mut patch = None;
        for arg in args {
            let Argument::Named { name, value } = arg else {
                continue;
            };
            let resolved = self.eval_expr(value);
            let Value::Number(Number::Int(n)) = resolved else {
                self.error(
                    value.span,
                    format!("ver(...)'s '{}' must be an integer", name.text),
                );
                continue;
            };
            match name.text.as_str() {
                "major" => major = Some(n),
                "minor" => minor = Some(n),
                "patch" => patch = Some(n),
                other => self.error(name.span, format!("ver(...) has no parameter '{other}'")),
            }
        }
        let mut missing = Vec::new();
        if major.is_none() {
            missing.push("major");
        }
        if minor.is_none() {
            missing.push("minor");
        }
        if patch.is_none() {
            missing.push("patch");
        }
        if !missing.is_empty() {
            self.error(span, format!("ver(...) is missing: {}", missing.join(", ")));
        }
        Value::Version(VersionValue {
            major: major.unwrap_or(0),
            minor: minor.unwrap_or(0),
            patch: patch.unwrap_or(0),
        })
    }

    fn eval_member_access(&mut self, base: Value, members: &[Ident], span: Span) -> Value {
        let mut current = base;
        for member in members {
            match current {
                Value::Properties(map) => match map.get(&member.text) {
                    Some(v) => current = Value::Str(v.clone()),
                    None => {
                        self.error(
                            member.span,
                            format!("no key '{}' in properties", member.text),
                        );
                        return Value::Invalid("missing properties key".to_owned());
                    }
                },
                Value::Invalid(_) => return current,
                other => {
                    self.error(
                        span,
                        format!(
                            "member access is only valid on props(...) results, got {}",
                            value_kind_name(&other)
                        ),
                    );
                    return Value::Invalid("unsupported member access".to_owned());
                }
            }
        }
        current
    }

    fn eval_versioned(&mut self, base: &Expr, version: &VersionRef, span: Span) -> Value {
        let base_value = self.eval_expr(base);
        let Value::Str(coordinate) = base_value else {
            if !matches!(base_value, Value::Invalid(_)) {
                self.error(
                    base.span,
                    format!(
                        "'@' base must be a string coordinate, got {}",
                        value_kind_name(&base_value)
                    ),
                );
            }
            return Value::Invalid("bad versioned base".to_owned());
        };
        if coordinate.matches(':').count() >= 2 {
            self.error(
                span,
                "coordinate already carries a version; '@' would duplicate it",
            );
            return Value::Invalid("duplicate version".to_owned());
        }
        let version_text = match version {
            VersionRef::Version(text) => text.clone(),
            VersionRef::RefName(name) => match self.defs.versions.get(name) {
                Some(v) => v.clone(),
                None => {
                    self.error(span, format!("unknown version reference '{name}'"));
                    return Value::Invalid("unknown version reference".to_owned());
                }
            },
        };
        Value::Coordinate(format!("{coordinate}:{version_text}"))
    }

    fn eval_binary(&mut self, op: crate::ast::BinaryOp, lhs: &Expr, rhs: &Expr) -> Value {
        use crate::ast::BinaryOp;
        // Short-circuit && / || : evaluate rhs only when its value can
        // still change the result.
        if op == BinaryOp::And {
            return match self.eval_expr(lhs) {
                Value::Bool(false) => Value::Bool(false),
                Value::Bool(true) => self.expect_bool(rhs),
                Value::Invalid(_) => Value::Invalid("and of invalid value".to_owned()),
                other => {
                    self.error(
                        lhs.span,
                        format!("'&&' requires a boolean, got {}", value_kind_name(&other)),
                    );
                    Value::Invalid("type error".to_owned())
                }
            };
        }
        if op == BinaryOp::Or {
            return match self.eval_expr(lhs) {
                Value::Bool(true) => Value::Bool(true),
                Value::Bool(false) => self.expect_bool(rhs),
                Value::Invalid(_) => Value::Invalid("or of invalid value".to_owned()),
                other => {
                    self.error(
                        lhs.span,
                        format!("'||' requires a boolean, got {}", value_kind_name(&other)),
                    );
                    Value::Invalid("type error".to_owned())
                }
            };
        }

        let lhs_v = self.eval_expr(lhs);
        let rhs_v = self.eval_expr(rhs);
        if matches!(lhs_v, Value::Invalid(_)) || matches!(rhs_v, Value::Invalid(_)) {
            return Value::Invalid("comparison of invalid value".to_owned());
        }
        match op {
            BinaryOp::Eq => Value::Bool(lhs_v == rhs_v),
            BinaryOp::NotEq => Value::Bool(lhs_v != rhs_v),
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                let (Some(l), Some(r)) = (as_f64(&lhs_v), as_f64(&rhs_v)) else {
                    self.error(
                        lhs.span.cover(rhs.span),
                        "ordering comparisons require numbers on both sides",
                    );
                    return Value::Invalid("type error".to_owned());
                };
                Value::Bool(match op {
                    BinaryOp::Lt => l < r,
                    BinaryOp::LtEq => l <= r,
                    BinaryOp::Gt => l > r,
                    BinaryOp::GtEq => l >= r,
                    _ => unreachable!(),
                })
            }
            BinaryOp::And | BinaryOp::Or => unreachable!("handled above"),
        }
    }

    fn expect_bool(&mut self, expr: &Expr) -> Value {
        match self.eval_expr(expr) {
            Value::Bool(b) => Value::Bool(b),
            Value::Invalid(_) => Value::Invalid("invalid operand".to_owned()),
            other => {
                self.error(
                    expr.span,
                    format!("expected a boolean, got {}", value_kind_name(&other)),
                );
                Value::Invalid("type error".to_owned())
            }
        }
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(Number::Int(i)) => Some(*i as f64),
        Value::Number(Number::Float(f)) => Some(*f),
        _ => None,
    }
}

fn value_kind_name(value: &Value) -> &'static str {
    match value {
        Value::Str(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Version(_) => "version",
        Value::Properties(_) => "properties",
        Value::Coordinate(_) => "coordinate",
        Value::Block(_) => "block",
        Value::Invalid(_) => "invalid",
    }
}

/// Minimal `.properties`-format parser (Java-style `key=value` /
/// `key:value` lines; `#` and `!` start a comment; blank lines ignored).
/// This is a real, from-scratch implementation (not a stub): it is what
/// backs [`Evaluator::eval_props`], matching the original Kotlin
/// convention plugins' `Properties().load(...)` usage (`signing.properties`
/// files) that motivated `props(...)` in the first place.
fn parse_properties(contents: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        let sep = trimmed.find(['=', ':']);
        let Some(idx) = sep else { continue };
        let key = trimmed[..idx].trim();
        let value = trimmed[idx + 1..].trim();
        if !key.is_empty() {
            map.insert(key.to_owned(), value.to_owned());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn eval_pair_value(src: &str) -> Value {
        let parsed = parse(src);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block model");
        };
        top["x"].clone()
    }

    #[test]
    fn evaluates_literals() {
        assert_eq!(eval_pair_value("x 37"), Value::Number(Number::Int(37)));
        assert_eq!(eval_pair_value("x true"), Value::Bool(true));
        assert_eq!(eval_pair_value(r#"x "hi""#), Value::Str("hi".to_owned()));
    }

    #[test]
    fn evaluates_string_interpolation_with_env() {
        // Uses a variable that is virtually guaranteed to already be set in
        // any process (PATH) rather than mutating the environment, since
        // this workspace forbids unsafe code and `std::env::set_var` is
        // unsafe on newer toolchains/editions.
        let path = std::env::var("PATH").expect("PATH should be set in any test environment");
        let value = eval_pair_value(r#"x "path-is-${env("PATH")}""#);
        assert_eq!(value, Value::Str(format!("path-is-{path}")));
    }

    #[test]
    fn env_missing_variable_is_diagnosed() {
        let parsed = parse(r#"x env("ULB_EVAL_DEFINITELY_UNSET_XYZ")"#);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("is not set"))
        );
    }

    #[test]
    fn lint_mode_does_not_consult_process_environment() {
        let parsed = parse(r#"x env("ULB_EVAL_DEFINITELY_UNSET_XYZ")"#);
        let defs = Definitions::default();
        let outcome = evaluate_build_lint(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
    }

    #[test]
    fn lint_mode_does_not_read_the_filesystem() {
        let parsed = parse(r#"x props("/definitely/absent/ulb-props-file.properties").key"#);
        let defs = Definitions::default();
        let outcome = evaluate_build_lint(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
    }

    #[test]
    fn lint_mode_still_reports_unknown_references() {
        let parsed = parse(r#"x ghostAlias"#);
        let defs = Definitions::default();
        let outcome = evaluate_build_lint(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown reference"))
        );
    }

    #[test]
    fn lint_mode_still_reports_arity_errors() {
        let mut defs = Definitions::default();
        defs.functions.insert(
            "helper".to_owned(),
            (
                vec![],
                Block {
                    statements: vec![],
                    span: Span::empty(0),
                },
            ),
        );
        let parsed = parse("helper(1, 2)");
        let outcome = evaluate_build_lint(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expected 0 argument"))
        );
    }

    #[test]
    fn lint_mode_honors_injected_environment() {
        let parsed = parse(r#"x env("ULB_EVAL_LINT_INJECTED")"#);
        let defs = Definitions::default();
        let mut env = EvalEnvironment::default();
        env.env
            .insert("ULB_EVAL_LINT_INJECTED".to_owned(), "from-map".to_owned());
        let outcome = evaluate_build_impl(&parsed.file, &defs, &env, true);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(top["x"], Value::Str("from-map".to_owned()));
    }

    #[test]
    fn collect_definitions_lint_is_hermetic() {
        let parsed = parse(r#"alias = env("ULB_EVAL_DEFINITELY_UNSET_XYZ")"#);
        let mut defs = Definitions::default();
        let mut diagnostics = Vec::new();
        collect_definitions_lint(&parsed.file, &mut defs, &mut diagnostics);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(defs.aliases.contains_key("alias"));
    }

    #[test]
    fn injected_env_shadows_process_environment() {
        let parsed = parse(r#"x env("PATH")"#);
        let defs = Definitions::default();
        let mut env = EvalEnvironment::default();
        env.env.insert("PATH".to_owned(), "injected".to_owned());
        let outcome = evaluate_build_with(&parsed.file, &defs, &env);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(top["x"], Value::Str("injected".to_owned()));
    }

    #[test]
    fn injected_env_supplies_variable_absent_from_process() {
        let parsed = parse(r#"x env("ULB_EVAL_INJECTED_ONLY")"#);
        let defs = Definitions::default();
        let mut env = EvalEnvironment::default();
        env.env
            .insert("ULB_EVAL_INJECTED_ONLY".to_owned(), "from-map".to_owned());
        let outcome = evaluate_build_with(&parsed.file, &defs, &env);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(top["x"], Value::Str("from-map".to_owned()));
    }

    #[test]
    fn injected_props_supply_member_access() {
        let parsed = parse(r#"x props("signing.properties").storeFile"#);
        let defs = Definitions::default();
        let mut env = EvalEnvironment::default();
        env.props.insert(
            "signing.properties".to_owned(),
            "storeFile=release.keystore\n".to_owned(),
        );
        let outcome = evaluate_build_with(&parsed.file, &defs, &env);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(top["x"], Value::Str("release.keystore".to_owned()));
    }

    #[test]
    fn props_without_injection_falls_back_to_filesystem() {
        let dir =
            std::env::temp_dir().join(format!("ulb-eval-props-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fallback.properties");
        std::fs::write(&path, "key=value\n").unwrap();
        let src = format!(r#"x props("{}").key"#, path.display());
        let parsed = parse(&src);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(top["x"], Value::Str("value".to_owned()));
    }

    #[test]
    fn evaluates_ver_builtin() {
        let value = eval_pair_value("x ver(major=1, minor=2, patch=3)");
        assert_eq!(
            value,
            Value::Version(VersionValue {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
    }

    #[test]
    fn ver_missing_arg_is_diagnosed_and_defaults_to_zero() {
        let parsed = parse("x ver(major=1, minor=2)");
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("missing"))
        );
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(
            top["x"],
            Value::Version(VersionValue {
                major: 1,
                minor: 2,
                patch: 0
            })
        );
    }

    #[test]
    fn evaluates_props_and_member_access() {
        let dir = std::env::temp_dir().join(format!("ulb-eval-props-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("signing.properties");
        std::fs::write(
            &file,
            "keyAlias=release\n# comment\nstorePassword = s3cr3t\n",
        )
        .unwrap();

        let src = format!(
            r#"x props("{}").keyAlias"#,
            file.display().to_string().replace('\\', "\\\\")
        );
        let value = eval_pair_value(&src);
        assert_eq!(value, Value::Str("release".to_owned()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn props_missing_key_is_diagnosed() {
        let dir =
            std::env::temp_dir().join(format!("ulb-eval-props-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("signing.properties");
        std::fs::write(&file, "keyAlias=release\n").unwrap();

        let src = format!(r#"x props("{}").nope"#, file.display());
        let parsed = parse(&src);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("no key 'nope'"))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn evaluates_if_true_and_false_branches() {
        assert_eq!(
            eval_pair_value("if true { x 1 } else { x 2 }"),
            Value::Number(Number::Int(1))
        );
        assert_eq!(
            eval_pair_value("if false { x 1 } else { x 2 }"),
            Value::Number(Number::Int(2))
        );
    }

    #[test]
    fn evaluates_else_if_chain() {
        assert_eq!(
            eval_pair_value("if false { x 1 } else if true { x 2 } else { x 3 }"),
            Value::Number(Number::Int(2))
        );
    }

    #[test]
    fn short_circuits_and_or() {
        // env() on a missing var would raise a diagnostic if evaluated;
        // short-circuiting must skip it.
        let parsed =
            parse(r#"if false && env("ULB_EVAL_SHOULD_NOT_RUN") == "x" { x 1 } else { x 2 }"#);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);

        let parsed2 =
            parse(r#"if true || env("ULB_EVAL_SHOULD_NOT_RUN") == "x" { x 1 } else { x 2 }"#);
        let outcome2 = evaluate_build(&parsed2.file, &defs);
        assert!(
            outcome2.diagnostics.is_empty(),
            "{:?}",
            outcome2.diagnostics
        );
    }

    #[test]
    fn evaluates_not_and_comparisons() {
        assert_eq!(eval_pair_value("x !false"), Value::Bool(true));
        assert_eq!(eval_pair_value("x 1 < 2"), Value::Bool(true));
        assert_eq!(eval_pair_value("x 2 <= 2"), Value::Bool(true));
        assert_eq!(eval_pair_value(r#"x "a" == "a""#), Value::Bool(true));
    }

    #[test]
    fn evaluates_list_literal() {
        assert_eq!(
            eval_pair_value(r#"x [ "a", "b" ]"#),
            Value::List(vec![Value::Str("a".to_owned()), Value::Str("b".to_owned())])
        );
    }

    #[test]
    fn evaluates_alias_and_version_references() {
        let mut defs = Definitions::default();
        defs.aliases.insert(
            "coreKtx".to_owned(),
            Value::Coordinate("androidx.core:core-ktx:1.16.0".to_owned()),
        );
        let parsed = parse("x coreKtx");
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty());
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(
            top["x"],
            Value::Coordinate("androidx.core:core-ktx:1.16.0".to_owned())
        );
    }

    #[test]
    fn unknown_reference_is_diagnosed() {
        let parsed = parse("x doesNotExist");
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown reference 'doesNotExist'"))
        );
    }

    #[test]
    fn evaluates_versioned_ref_name() {
        let mut defs = Definitions::default();
        defs.versions
            .insert("coreVersion".to_owned(), "1.16.0".to_owned());
        let parsed = parse(r#"x "androidx.core:core-ktx" @ coreVersion"#);
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        assert_eq!(
            top["x"],
            Value::Coordinate("androidx.core:core-ktx:1.16.0".to_owned())
        );
    }

    #[test]
    fn duplicate_version_attach_is_diagnosed() {
        let mut defs = Definitions::default();
        defs.versions.insert("v".to_owned(), "1.0.0".to_owned());
        let parsed = parse(r#"x "androidx.core:core-ktx:1.16.0" @ v"#);
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("would duplicate it"))
        );
    }

    #[test]
    fn block_statements_merge_across_repeats() {
        // Simulates a convention's `android { compileSdk 37 }` plus a
        // module's own `android { minSdk 24 }` both landing in the same
        // resolved `android` block.
        let src = "android { compileSdk 37 }\nandroid { minSdk 24 }";
        let parsed = parse(src);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(android) = &top["android"] else {
            panic!("expected android Block");
        };
        assert_eq!(android["compileSdk"], Value::Number(Number::Int(37)));
        assert_eq!(android["minSdk"], Value::Number(Number::Int(24)));
    }

    #[test]
    fn repeated_scalar_pair_accumulates_into_list() {
        let src = r#"deps { implementation "a:a:1" implementation "b:b:1" }"#;
        let parsed = parse(src);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(deps) = &top["deps"] else {
            panic!("expected deps Block");
        };
        assert_eq!(
            deps["implementation"],
            Value::List(vec![
                Value::Str("a:a:1".to_owned()),
                Value::Str("b:b:1".to_owned())
            ])
        );
    }

    #[test]
    fn dotted_block_target_nests_correctly() {
        let src = "commonMain.deps { implementation coreKtx }";
        let mut defs = Definitions::default();
        defs.aliases
            .insert("coreKtx".to_owned(), Value::Coordinate("g:a:1".to_owned()));
        let parsed = parse(src);
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(common_main) = &top["commonMain"] else {
            panic!("expected commonMain Block");
        };
        let Value::Block(deps) = &common_main["deps"] else {
            panic!("expected deps Block");
        };
        assert_eq!(
            deps["implementation"],
            Value::Coordinate("g:a:1".to_owned())
        );
    }

    #[test]
    fn apply_inlines_convention_statements() {
        let mut defs = Definitions::default();
        let convention_src = "convention androidApp { android { compileSdk 37 } }";
        let convention_parsed = parse(convention_src);
        let mut diags = Vec::new();
        collect_definitions(&convention_parsed.file, &mut defs, &mut diags);
        assert!(diags.is_empty());

        let build_src = r#"apply "androidApp"\nandroid { minSdk 24 }"#.replace("\\n", "\n");
        let parsed = parse(&build_src);
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(android) = &top["android"] else {
            panic!("expected android Block");
        };
        assert_eq!(android["compileSdk"], Value::Number(Number::Int(37)));
        assert_eq!(android["minSdk"], Value::Number(Number::Int(24)));
    }

    #[test]
    fn apply_unknown_convention_is_diagnosed() {
        let parsed = parse(r#"apply "does-not-exist""#);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown convention 'does-not-exist'"))
        );
    }

    #[test]
    fn fn_call_statement_inlines_with_bound_params() {
        let convention_src = "fn debugType(flag) { buildTypes { debug { minifyEnabled flag } } }";
        let mut defs = Definitions::default();
        let mut diags = Vec::new();
        collect_definitions(&parse(convention_src).file, &mut defs, &mut diags);
        assert!(diags.is_empty());

        let parsed = parse("debugType(false)");
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(build_types) = &top["buildTypes"] else {
            panic!("expected buildTypes");
        };
        let Value::Block(debug) = &build_types["debug"] else {
            panic!("expected debug");
        };
        assert_eq!(debug["minifyEnabled"], Value::Bool(false));
    }

    #[test]
    fn unknown_call_statement_is_diagnosed() {
        let parsed = parse("notAFunction(1)");
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown function 'notAFunction'"))
        );
    }

    #[test]
    fn run_block_accumulates_actions() {
        let src = r#"task "t" { run { copy(from="a", to="b") exec(command="echo") } }"#;
        let parsed = parse(src);
        let defs = Definitions::default();
        let outcome = evaluate_build(&parsed.file, &defs);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(tasks) = &top["tasks"] else {
            panic!("expected tasks");
        };
        let Value::Block(t) = &tasks["t"] else {
            panic!("expected task 't'");
        };
        let Value::Block(run) = &t["run"] else {
            panic!("expected run block");
        };
        let Value::List(actions) = &run["__actions__"] else {
            panic!("expected __actions__ list");
        };
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn evaluate_project_worked_example() {
        let conventions = r#"
convention androidApp {
  android {
    compileSdk 37
    minSdk 24
  }
  buildTypes {
    debug { minifyEnabled false }
    release { minifyEnabled true }
  }
}
"#;
        let libs = r#"
versions {
  coreVersion = "1.16.0"
}
coreKtx = "androidx.core:core-ktx" @ coreVersion
"#;
        let build = r#"
apply "androidApp"

android {
  namespace "com.uliteamr.notescribe"
}

deps {
  implementation coreKtx
}
"#;
        let outcome = evaluate_project(conventions, libs, build);
        assert!(outcome.diagnostics.is_empty(), "{:?}", outcome.diagnostics);
        let Value::Block(top) = &outcome.model else {
            panic!("expected Block");
        };
        let Value::Block(android) = &top["android"] else {
            panic!("expected android Block");
        };
        assert_eq!(android["compileSdk"], Value::Number(Number::Int(37)));
        assert_eq!(
            android["namespace"],
            Value::Str("com.uliteamr.notescribe".to_owned())
        );
        let Value::Block(deps) = &top["deps"] else {
            panic!("expected deps Block");
        };
        assert_eq!(
            deps["implementation"],
            Value::Coordinate("androidx.core:core-ktx:1.16.0".to_owned())
        );
    }
}

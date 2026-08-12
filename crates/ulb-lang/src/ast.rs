//! Typed AST for the ulb DSL.
//!
//! Every AST node is a [`Node<T>`]: a `kind` plus a source [`Span`]. The
//! node set mirrors GRAMMAR.md §5 rule-for-rule: one node kind per grammar
//! construct, so the evaluator and the LSP can walk the same tree. Nodes are
//! value types (no links to parents); the tree is owned by [`File`].
//!
//! The parser (Phase 3) builds this AST from the token stream. Until then
//! the types are exercised directly by construction tests.

use crate::span::Span;
use crate::token::Number;

/// A tree node: a kind plus the source span it was produced from.
#[derive(Debug, Clone, PartialEq)]
pub struct Node<T> {
    /// What this node is.
    pub kind: T,
    /// Source span covering the whole node (children inclusive).
    pub span: Span,
}

impl<T> Node<T> {
    /// Wraps `kind` in a node with span `span`.
    #[must_use]
    pub fn new(kind: T, span: Span) -> Self {
        Self { kind, span }
    }

    /// Borrows the contained kind.
    #[must_use]
    pub fn kind(&self) -> &T {
        &self.kind
    }
}

/// A parsed `.ulb` file: the top-level node of every parse.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    /// Top-level statements in source order.
    pub statements: Vec<Statement>,
    /// Span of the whole file.
    pub span: Span,
}

/// An identifier (a `letter { letter | digit }` run, or a contextual
/// keyword used in identifier position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    /// Identifier text.
    pub text: String,
    /// Span of the identifier.
    pub span: Span,
}

/// A dotted path: `a`, `commonMain.deps`, `a.b.c`.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    /// Path segments in order.
    pub segments: Vec<Ident>,
    /// Span covering the whole path.
    pub span: Span,
}

impl Path {
    /// Whether the path is a single segment.
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.segments.len() == 1
    }

    /// The first segment's text (empty string for an empty path).
    #[must_use]
    pub fn head(&self) -> &str {
        self.segments.first().map_or("", |seg| &seg.text)
    }
}

/// An expression (`or_expr` with an optional `@` version attach, GRAMMAR.md
/// §5).
pub type Expr = Node<ExprKind>;

/// The kinds of expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// String literal, possibly with `${...}` interpolation parts.
    Str(StrExpr),
    /// Numeric literal.
    Number(Number),
    /// Boolean literal.
    Bool(bool),
    /// A bare reference to a name: an alias, version, plugin, or fn.
    Ref(Path),
    /// A function call: `env("X")`, `ver(major=0)`.
    Call(Call),
    /// Member access on a call result: `props("path").key`.
    MemberAccess {
        /// The call whose result is being indexed.
        base: Box<Call>,
        /// Member chain (`props("x").a.b`).
        members: Vec<Ident>,
    },
    /// List literal: `[ "a", "b" ]`.
    List(Vec<Expr>),
    /// `base @ version` — attach a version reference to a coordinate.
    Versioned {
        /// The value being versioned (usually a string or ref).
        base: Box<Expr>,
        /// The attached version.
        version: VersionRef,
    },
    /// Comparison or boolean operator application.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// `!expr`.
    Not(Box<Expr>),
    /// Parenthesized expression (span preserved, value transparent).
    Group(Box<Expr>),
}

/// One part of a string literal.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    /// Plain text (escapes resolved).
    Literal(String),
    /// An interpolation `"${ ... }"` whose inner expression is evaluated.
    Interp(Expr),
}

/// A string literal expression: a sequence of literal and interpolation
/// parts.
#[derive(Debug, Clone, PartialEq)]
pub struct StrExpr {
    /// Parts in source order.
    pub parts: Vec<StrPart>,
}

impl StrExpr {
    /// Renders the string with interpolations left as `${}` placeholders
    /// (for diagnostics and LSP hover). Interpolated values are not
    /// evaluated here.
    #[must_use]
    pub fn raw(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                StrPart::Literal(text) => out.push_str(text),
                StrPart::Interp(_) => out.push_str("${...}"),
            }
        }
        out
    }
}

/// A function call expression.
pub type Call = Node<CallKind>;

/// The parts of a call: a callee name and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct CallKind {
    /// Callee identifier.
    pub callee: Ident,
    /// Arguments in source order.
    pub args: Vec<Argument>,
}

/// A call argument: positional or named.
#[derive(Debug, Clone, PartialEq)]
pub enum Argument {
    /// `name = value`.
    Named {
        /// Argument name.
        name: Ident,
        /// Argument value.
        value: Expr,
    },
    /// Bare positional value.
    Positional(Expr),
}

/// The `@` target of a versioned coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionRef {
    /// `@ name` referencing a `versions {}` entry.
    RefName(String),
    /// `@ "1.2.3"` inline version.
    Version(String),
}

/// Binary operators (GRAMMAR.md §5.2 precedence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `==`
    Eq,
    /// `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `&&`
    And,
    /// `||`
    Or,
}

impl BinaryOp {
    /// The operator's source text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

/// A statement.
pub type Statement = Node<StatementKind>;

/// The kinds of statements (GRAMMAR.md §5).
#[derive(Debug, Clone, PartialEq)]
pub enum StatementKind {
    /// `if cond { } else { }`.
    If(IfKind),
    /// `path { ... }` — a block statement (`android { }`, `deps { }`,
    /// `commonMain.deps { }`).
    BlockStmt {
        /// The path naming the block.
        path: Path,
        /// The block body.
        block: Block,
    },
    /// `convention name { ... }`.
    ConventionDef {
        /// Convention name.
        name: Ident,
        /// Convention body.
        block: Block,
    },
    /// `fn name(a, b) { ... }`.
    FnDef {
        /// Function name.
        name: Ident,
        /// Parameter names in order.
        params: Vec<Ident>,
        /// Function body.
        block: Block,
    },
    /// `task "name" { ... }`.
    TaskDef {
        /// Task name (a plain string, no interpolation).
        name: String,
        /// Task body.
        block: Block,
    },
    /// `apply "name"`.
    Apply {
        /// The convention name being applied.
        name: String,
    },
    /// `path = value` (used in `libs.ulb` and version/bundle/plugin
    /// blocks).
    Assignment {
        /// The assigned path.
        path: Path,
        /// The value.
        value: Expr,
    },
    /// `key value` — bare key-value pair (`compileSdk 37`,
    /// `implementation "g:a:v"`).
    Pair {
        /// The key.
        key: Ident,
        /// The value.
        value: Expr,
    },
    /// A bare call used as a statement (`ver(major=0)`).
    CallStmt(Call),
    /// A node produced by parser error recovery; consumers must skip it
    /// rather than guess at its meaning.
    Invalid {
        /// Why the node was recovered.
        message: String,
    },
}

/// The body of an `if`/`else` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct IfKind {
    /// The condition expression.
    pub condition: Expr,
    /// The `then` block.
    pub then_branch: Block,
    /// The optional `else` branch: a block or a chained `else if`.
    pub else_branch: Option<ElseBranch>,
}

/// The `else` target of an `if` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ElseBranch {
    /// `else { ... }`.
    Block(Block),
    /// `else if ...` (a chained if).
    If(Box<Node<IfKind>>),
}

/// A `{ ... }` block: a sequence of statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Statements inside the braces.
    pub statements: Vec<Statement>,
    /// Span covering `{ ... }` inclusive.
    pub span: Span,
}

/// Convenience constructor for test fixtures.
#[cfg(test)]
mod fixture {
    use super::*;

    pub fn span(start: u32, end: u32) -> Span {
        Span { start, end }
    }

    pub fn ident(text: &str, start: u32, end: u32) -> Ident {
        Ident {
            text: text.to_owned(),
            span: span(start, end),
        }
    }

    pub fn path(segments: Vec<Ident>) -> Path {
        let span = segments
            .iter()
            .fold(span(0, 0), |acc, seg| acc.cover(seg.span));
        Path { segments, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::fixture::{ident, path, span};

    #[test]
    fn file_holds_statements() {
        let file = File {
            statements: vec![],
            span: span(0, 0),
        };
        assert!(file.statements.is_empty());
        assert_eq!(file.span, span(0, 0));
    }

    #[test]
    fn node_wraps_kind_and_span() {
        let node = Node::new(ExprKind::Bool(true), span(1, 5));
        assert_eq!(node.span, span(1, 5));
        assert_eq!(node.kind(), &ExprKind::Bool(true));
    }

    #[test]
    fn path_reports_head_and_single_segment() {
        let single = path(vec![ident("deps", 0, 4)]);
        assert!(single.is_single());
        assert_eq!(single.head(), "deps");
        let dotted = path(vec![ident("commonMain", 0, 10), ident("deps", 11, 15)]);
        assert!(!dotted.is_single());
        assert_eq!(dotted.head(), "commonMain");
    }

    #[test]
    fn string_expr_renders_raw_with_placeholders() {
        let str_expr = StrExpr {
            parts: vec![
                StrPart::Literal("name: ".to_owned()),
                StrPart::Interp(Node::new(
                    ExprKind::Call(Node::new(
                        CallKind {
                            callee: ident("env", 7, 10),
                            args: vec![Argument::Positional(Node::new(
                                ExprKind::Str(StrExpr {
                                    parts: vec![StrPart::Literal("X".to_owned())],
                                }),
                                span(11, 14),
                            ))],
                        },
                        span(7, 15),
                    )),
                    span(7, 15),
                )),
                StrPart::Literal("".to_owned()),
            ],
        };
        assert_eq!(str_expr.raw(), "name: ${...}");
    }

    #[test]
    fn each_statement_kind_is_constructible() {
        let block = Block {
            statements: vec![],
            span: span(0, 2),
        };
        let statements = vec![
            Statement::new(
                StatementKind::If(IfKind {
                    condition: Node::new(ExprKind::Bool(true), span(3, 7)),
                    then_branch: block.clone(),
                    else_branch: Some(ElseBranch::Block(block.clone())),
                }),
                span(0, 20),
            ),
            Statement::new(
                StatementKind::BlockStmt {
                    path: path(vec![ident("android", 0, 7)]),
                    block: block.clone(),
                },
                span(0, 10),
            ),
            Statement::new(
                StatementKind::ConventionDef {
                    name: ident("android-app", 11, 22),
                    block: block.clone(),
                },
                span(0, 24),
            ),
            Statement::new(
                StatementKind::FnDef {
                    name: ident("helper", 3, 9),
                    params: vec![ident("a", 10, 11)],
                    block: block.clone(),
                },
                span(0, 14),
            ),
            Statement::new(
                StatementKind::TaskDef {
                    name: "printConfig".to_owned(),
                    block: block.clone(),
                },
                span(0, 30),
            ),
            Statement::new(
                StatementKind::Apply {
                    name: "android-app".to_owned(),
                },
                span(0, 20),
            ),
            Statement::new(
                StatementKind::Assignment {
                    path: path(vec![ident("alias", 0, 5)]),
                    value: Node::new(ExprKind::Str(StrExpr { parts: vec![] }), span(8, 12)),
                },
                span(0, 12),
            ),
            Statement::new(
                StatementKind::Pair {
                    key: ident("compileSdk", 0, 10),
                    value: Node::new(ExprKind::Number(Number::Int(37)), span(11, 13)),
                },
                span(0, 13),
            ),
            Statement::new(
                StatementKind::CallStmt(Node::new(
                    CallKind {
                        callee: ident("ver", 0, 3),
                        args: vec![Argument::Named {
                            name: ident("major", 4, 9),
                            value: Node::new(ExprKind::Number(Number::Int(0)), span(10, 11)),
                        }],
                    },
                    span(0, 12),
                )),
                span(0, 12),
            ),
            Statement::new(
                StatementKind::Invalid {
                    message: "unexpected token".to_owned(),
                },
                span(0, 0),
            ),
        ];
        assert_eq!(statements.len(), 10);
    }

    #[test]
    fn each_expression_kind_is_constructible() {
        let str = Node::new(ExprKind::Str(StrExpr { parts: vec![] }), span(0, 4));
        let int = Node::new(ExprKind::Number(Number::Int(37)), span(0, 2));
        let flag = Node::new(ExprKind::Bool(false), span(0, 5));
        let reference = Node::new(ExprKind::Ref(path(vec![ident("alias", 0, 5)])), span(0, 5));
        let call = Node::new(
            ExprKind::Call(Node::new(
                CallKind {
                    callee: ident("env", 0, 3),
                    args: vec![],
                },
                span(0, 5),
            )),
            span(0, 5),
        );
        let member = Node::new(
            ExprKind::MemberAccess {
                base: Box::new(Node::new(
                    CallKind {
                        callee: ident("props", 0, 5),
                        args: vec![],
                    },
                    span(0, 7),
                )),
                members: vec![ident("key", 8, 11)],
            },
            span(0, 11),
        );
        let list = Node::new(
            ExprKind::List(vec![
                Node::new(ExprKind::Str(StrExpr { parts: vec![] }), span(1, 3)),
                Node::new(ExprKind::Str(StrExpr { parts: vec![] }), span(4, 6)),
            ]),
            span(0, 7),
        );
        let versioned = Node::new(
            ExprKind::Versioned {
                base: Box::new(str.clone()),
                version: VersionRef::RefName("coreVersion".to_owned()),
            },
            span(0, 8),
        );
        let binary = Node::new(
            ExprKind::Binary {
                op: BinaryOp::Eq,
                lhs: Box::new(int.clone()),
                rhs: Box::new(int),
            },
            span(0, 4),
        );
        let not = Node::new(ExprKind::Not(Box::new(flag)), span(0, 6));
        let group = Node::new(ExprKind::Group(Box::new(reference)), span(0, 7));
        let exprs = [str, call, member, list, versioned, binary, not, group];
        assert_eq!(exprs.len(), 8);
    }

    #[test]
    fn binary_op_text_roundtrips() {
        for op in [
            BinaryOp::Eq,
            BinaryOp::NotEq,
            BinaryOp::Lt,
            BinaryOp::LtEq,
            BinaryOp::Gt,
            BinaryOp::GtEq,
            BinaryOp::And,
            BinaryOp::Or,
        ] {
            assert!(!op.as_str().is_empty());
        }
        assert_eq!(BinaryOp::And.as_str(), "&&");
        assert_eq!(BinaryOp::Eq.as_str(), "==");
    }
}

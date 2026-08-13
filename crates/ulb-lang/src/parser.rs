//! Hand-written recursive-descent parser for the ulb DSL.
//!
//! Builds the typed [`crate::ast::File`] tree from the token stream produced
//! by [`crate::lexer::lex`], following GRAMMAR.md §5 rule-for-rule. Comment
//! tokens are dropped before parsing begins (GRAMMAR.md §2: "comments ...
//! are invisible to the parser").
//!
//! # Error recovery
//!
//! The parser never fails fast (GRAMMAR.md §11). On an unexpected token it
//! emits a [`Diagnostic`], then [`Parser::synchronize`] discards tokens
//! until the next statement start (an identifier, a reserved word, or a
//! `}` at the current nesting depth) so parsing can resume. Recovered
//! statements are marked [`StatementKind::Invalid`]; recovered expressions
//! are marked [`ExprKind::Invalid`]. This is the same contract the LSP
//! relies on to analyze source that is mid-edit.
//!
//! String interpolation (`"${expr}"`) is parsed by handing the
//! already-lexed inner token slice (GRAMMAR.md §3) to a nested [`Parser`]
//! instance; any diagnostics it raises are folded into the outer parser's
//! diagnostic list with their original (absolute) source spans intact.

use crate::ast::{
    Argument, Block, Call, CallKind, ElseBranch, Expr, ExprKind, File, Ident, IfKind, Node, Path,
    Statement, StatementKind, StrExpr, StrPart, VersionRef,
};
use crate::diagnostic::{Diagnostic, Severity};
use crate::lexer::lex;
use crate::span::Span;
use crate::token::{Keyword, StrSegment, Symbol, Token, TokenKind};

/// The result of parsing a source file: the AST plus every diagnostic
/// recovered from during lexing and parsing, in source order.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// The parsed (possibly partial) file.
    pub file: File,
    /// Diagnostics from lexing and parsing, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Parses `source` into a [`Parsed`] file. Never panics on malformed input;
/// malformed regions are recovered as [`StatementKind::Invalid`] /
/// [`ExprKind::Invalid`] nodes alongside a diagnostic (GRAMMAR.md §11).
///
/// # Examples
///
/// ```
/// use ulb_lang::parser::parse;
/// use ulb_lang::ast::StatementKind;
///
/// let parsed = parse(r#"compileSdk 37"#);
/// assert!(parsed.diagnostics.is_empty());
/// assert!(matches!(
///     parsed.file.statements[0].kind,
///     StatementKind::Pair { .. }
/// ));
/// ```
#[must_use]
pub fn parse(source: &str) -> Parsed {
    let lexed = lex(source);
    let mut diagnostics = lexed.diagnostics;
    let tokens: Vec<Token> = lexed
        .tokens
        .into_iter()
        .filter(|t| t.kind != TokenKind::Comment)
        .collect();
    let file_end = tokens.last().map_or(0, |t| t.span.end);
    let mut parser = Parser {
        tokens,
        pos: 0,
        diagnostics: Vec::new(),
    };
    let mut statements = Vec::new();
    while !matches!(parser.peek(), TokenKind::Eof) {
        statements.push(parser.parse_statement());
    }
    diagnostics.append(&mut parser.diagnostics);
    Parsed {
        file: File {
            statements,
            span: Span {
                start: 0,
                end: file_end,
            },
        },
        diagnostics,
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

impl Parser {
    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn peek_at(&self, offset: usize) -> Option<&TokenKind> {
        self.tokens.get(self.pos + offset).map(|t| &t.kind)
    }

    /// Consumes and returns the current token. A no-op past the trailing
    /// `Eof` token, so callers can loop on `matches!(peek(), Eof)` safely.
    fn advance(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            severity: Severity::Error,
            message: message.into(),
        });
    }

    /// Consumes `sym` if present, returning its span; otherwise reports an
    /// error at the current token *without consuming it*, so a caller that
    /// wants to keep parsing after a missing-delimiter error still can.
    fn expect_symbol(&mut self, sym: Symbol, what: &str) -> Span {
        if self.peek() == &TokenKind::Symbol(sym) {
            let span = self.peek_span();
            self.advance();
            span
        } else {
            let span = self.peek_span();
            self.error(span, format!("expected {what}"));
            span
        }
    }

    /// `IDENT` in identifier position. A reserved word here is the
    /// "reserved word used as identifier" diagnostic case (GRAMMAR.md §11).
    fn expect_ident(&mut self) -> Option<Ident> {
        match self.peek().clone() {
            TokenKind::Ident(text) => {
                let span = self.peek_span();
                self.advance();
                Some(Ident { text, span })
            }
            TokenKind::Keyword(kw) => {
                let span = self.peek_span();
                self.error(
                    span,
                    format!("reserved word '{}' used as identifier", kw.as_str()),
                );
                self.advance();
                None
            }
            _ => None,
        }
    }

    /// Panic-mode recovery (GRAMMAR.md §11): discard tokens until the next
    /// statement start at the *current* nesting depth — an `IDENT`, a
    /// reserved word, or a `}` — tracking braces so an inner `}` inside the
    /// discarded region doesn't end recovery early.
    fn synchronize(&mut self) {
        let mut depth: i32 = 0;
        loop {
            match self.peek() {
                TokenKind::Eof => return,
                TokenKind::Symbol(Symbol::LBrace) => {
                    depth += 1;
                    self.advance();
                }
                TokenKind::Symbol(Symbol::RBrace) => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    self.advance();
                }
                TokenKind::Ident(_) | TokenKind::Keyword(_) if depth == 0 => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    // -- statements ---------------------------------------------------

    fn parse_statement(&mut self) -> Statement {
        let start = self.peek_span();
        match self.peek().clone() {
            TokenKind::Keyword(Keyword::If) => self.parse_if(),
            TokenKind::Keyword(Keyword::Convention) => self.parse_convention_def(),
            TokenKind::Keyword(Keyword::Fn) => self.parse_fn_def(),
            TokenKind::Keyword(Keyword::Task) => self.parse_task_def(),
            TokenKind::Keyword(Keyword::Apply) => self.parse_apply(),
            TokenKind::Ident(_) => self.parse_ident_led_statement(),
            TokenKind::Keyword(kw) => {
                // `else` (or any other reserved word) where a statement was
                // expected: report and recover.
                self.error(
                    start,
                    format!("reserved word '{}' used as identifier", kw.as_str()),
                );
                self.advance();
                self.synchronize();
                Statement::new(
                    StatementKind::Invalid {
                        message: "reserved word used as identifier".to_owned(),
                    },
                    start,
                )
            }
            TokenKind::Symbol(Symbol::RBrace) => {
                // Only reachable if a caller mis-drives the loop; treat as
                // unexpected token rather than looping forever.
                self.error(start, "unexpected token '}'");
                self.advance();
                Statement::new(
                    StatementKind::Invalid {
                        message: "unexpected token".to_owned(),
                    },
                    start,
                )
            }
            _ => {
                self.error(start, "unexpected token");
                self.advance();
                self.synchronize();
                Statement::new(
                    StatementKind::Invalid {
                        message: "unexpected token".to_owned(),
                    },
                    start,
                )
            }
        }
    }

    fn parse_block(&mut self) -> Block {
        let start = self.peek_span();
        if self.peek() != &TokenKind::Symbol(Symbol::LBrace) {
            self.error(start, "expected '{'");
            return Block {
                statements: Vec::new(),
                span: start,
            };
        }
        self.advance();
        let mut statements = Vec::new();
        loop {
            match self.peek() {
                TokenKind::Symbol(Symbol::RBrace) => {
                    let end = self.peek_span();
                    self.advance();
                    return Block {
                        statements,
                        span: start.cover(end),
                    };
                }
                TokenKind::Eof => {
                    self.error(start, "unexpected end of block");
                    let end = self.peek_span();
                    return Block {
                        statements,
                        span: start.cover(end),
                    };
                }
                _ => statements.push(self.parse_statement()),
            }
        }
    }

    fn parse_if(&mut self) -> Statement {
        let start = self.peek_span();
        self.advance(); // 'if'
        let Some(condition) = self.parse_expression() else {
            self.error(self.peek_span(), "expected condition after 'if'");
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "expected condition after 'if'".to_owned(),
                },
                start,
            );
        };
        let then_branch = self.parse_block();
        let mut span = start.cover(then_branch.span);
        let else_branch = if self.peek() == &TokenKind::Keyword(Keyword::Else) {
            self.advance();
            if self.peek() == &TokenKind::Keyword(Keyword::If) {
                let nested = self.parse_if();
                span = span.cover(nested.span);
                match nested.kind {
                    StatementKind::If(inner) => {
                        Some(ElseBranch::If(Box::new(Node::new(inner, nested.span))))
                    }
                    // parse_if() only ever returns If or Invalid; on
                    // Invalid the error was already reported, so we simply
                    // drop the (unusable) else-if chain rather than guess.
                    _ => None,
                }
            } else {
                let block = self.parse_block();
                span = span.cover(block.span);
                Some(ElseBranch::Block(block))
            }
        } else {
            None
        };
        Statement::new(
            StatementKind::If(IfKind {
                condition,
                then_branch,
                else_branch,
            }),
            span,
        )
    }

    fn parse_convention_def(&mut self) -> Statement {
        let start = self.peek_span();
        self.advance(); // 'convention'
        let Some(name) = self.expect_ident() else {
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "expected convention name".to_owned(),
                },
                start,
            );
        };
        let block = self.parse_block();
        let span = start.cover(block.span);
        Statement::new(StatementKind::ConventionDef { name, block }, span)
    }

    fn parse_fn_def(&mut self) -> Statement {
        let start = self.peek_span();
        self.advance(); // 'fn'
        let Some(name) = self.expect_ident() else {
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "expected function name".to_owned(),
                },
                start,
            );
        };
        if self.peek() != &TokenKind::Symbol(Symbol::LParen) {
            self.error(self.peek_span(), "expected '(' after function name");
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "expected '(' after function name".to_owned(),
                },
                start,
            );
        }
        self.advance();
        let mut params = Vec::new();
        if self.peek() != &TokenKind::Symbol(Symbol::RParen) {
            while let Some(p) = self.expect_ident() {
                params.push(p);
                if self.peek() == &TokenKind::Symbol(Symbol::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.expect_symbol(Symbol::RParen, "')'");
        let block = self.parse_block();
        let span = start.cover(block.span);
        Statement::new(
            StatementKind::FnDef {
                name,
                params,
                block,
            },
            span,
        )
    }

    fn parse_task_def(&mut self) -> Statement {
        let start = self.peek_span();
        self.advance(); // 'task'
        let TokenKind::Str(segments) = self.peek().clone() else {
            self.error(self.peek_span(), "expected task name string");
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "expected task name string".to_owned(),
                },
                start,
            );
        };
        let name_span = self.peek_span();
        self.advance();
        let Some(name) = self.plain_string_text(&segments, name_span, "task name") else {
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "task name must be a plain string".to_owned(),
                },
                start,
            );
        };
        let block = self.parse_block();
        let span = start.cover(block.span);
        Statement::new(StatementKind::TaskDef { name, block }, span)
    }

    fn parse_apply(&mut self) -> Statement {
        let start = self.peek_span();
        self.advance(); // 'apply'
        let TokenKind::Str(segments) = self.peek().clone() else {
            self.error(self.peek_span(), "expected string after 'apply'");
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "expected string after 'apply'".to_owned(),
                },
                start,
            );
        };
        let str_span = self.peek_span();
        self.advance();
        let span = start.cover(str_span);
        match self.plain_string_text(&segments, str_span, "apply target") {
            Some(name) => Statement::new(
                StatementKind::Apply {
                    name,
                    name_span: str_span,
                },
                span,
            ),
            None => Statement::new(
                StatementKind::Invalid {
                    message: "apply target must be a plain string".to_owned(),
                },
                span,
            ),
        }
    }

    /// Dispatches on the token after a leading `path` per GRAMMAR.md §5.1.
    fn parse_ident_led_statement(&mut self) -> Statement {
        let Some(path) = self.parse_path() else {
            let span = self.peek_span();
            self.error(span, "unexpected token");
            self.advance();
            self.synchronize();
            return Statement::new(
                StatementKind::Invalid {
                    message: "unexpected token".to_owned(),
                },
                span,
            );
        };

        match self.peek() {
            TokenKind::Symbol(Symbol::LBrace) => {
                let block = self.parse_block();
                let span = path.span.cover(block.span);
                Statement::new(StatementKind::BlockStmt { path, block }, span)
            }
            TokenKind::Symbol(Symbol::Eq) => {
                self.advance();
                match self.parse_expression() {
                    Some(value) => {
                        let span = path.span.cover(value.span);
                        Statement::new(StatementKind::Assignment { path, value }, span)
                    }
                    None => {
                        self.error(path.span, "missing value after path");
                        self.synchronize();
                        Statement::new(
                            StatementKind::Invalid {
                                message: "missing value after path".to_owned(),
                            },
                            path.span,
                        )
                    }
                }
            }
            TokenKind::Symbol(Symbol::LParen) if path.is_single() => {
                let call = self.parse_call_from(path.segments[0].clone());
                let span = call.span;
                Statement::new(StatementKind::CallStmt(call), span)
            }
            TokenKind::Symbol(Symbol::LParen) => {
                self.error(path.span, "dotted callee is invalid");
                self.synchronize();
                Statement::new(
                    StatementKind::Invalid {
                        message: "dotted callee is invalid".to_owned(),
                    },
                    path.span,
                )
            }
            _ => {
                if !path.is_single() {
                    self.error(
                        path.span,
                        "a dotted path is only valid as a block target ('path { ... }')",
                    );
                    self.synchronize();
                    return Statement::new(
                        StatementKind::Invalid {
                            message: "dotted path used outside a block target".to_owned(),
                        },
                        path.span,
                    );
                }
                match self.parse_expression() {
                    Some(value) => {
                        let key = path.segments[0].clone();
                        let span = key.span.cover(value.span);
                        Statement::new(StatementKind::Pair { key, value }, span)
                    }
                    None => {
                        self.error(path.span, "missing value after path");
                        self.synchronize();
                        Statement::new(
                            StatementKind::Invalid {
                                message: "missing value after path".to_owned(),
                            },
                            path.span,
                        )
                    }
                }
            }
        }
    }

    fn parse_path(&mut self) -> Option<Path> {
        let first = self.expect_ident()?;
        let mut segments = vec![first];
        while self.peek() == &TokenKind::Symbol(Symbol::Dot) {
            self.advance();
            match self.expect_ident() {
                Some(seg) => segments.push(seg),
                None => break,
            }
        }
        let span = segments
            .iter()
            .fold(segments[0].span, |acc, seg| acc.cover(seg.span));
        Some(Path { segments, span })
    }

    // -- expressions ----------------------------------------------------

    /// `expression = or_expr [ "@" (STRING | IDENT) ]`.
    fn parse_expression(&mut self) -> Option<Expr> {
        let base = self.parse_or_expr()?;
        if self.peek() == &TokenKind::Symbol(Symbol::At) {
            let at_span = self.peek_span();
            self.advance();
            let version = match self.peek().clone() {
                TokenKind::Str(segments) => {
                    let span = self.peek_span();
                    self.advance();
                    match self.plain_string_text(&segments, span, "version") {
                        Some(text) => VersionRef::Version(text),
                        None => return Some(base),
                    }
                }
                TokenKind::Ident(name) => {
                    self.advance();
                    VersionRef::RefName(name)
                }
                _ => {
                    self.error(at_span, "'@' must be followed by a string or identifier");
                    return Some(base);
                }
            };
            let span = base
                .span
                .cover(self.tokens[self.pos.saturating_sub(1)].span);
            return Some(Node::new(
                ExprKind::Versioned {
                    base: Box::new(base),
                    version,
                },
                span,
            ));
        }
        Some(base)
    }

    fn parse_or_expr(&mut self) -> Option<Expr> {
        let mut lhs = self.parse_and_expr()?;
        while self.peek() == &TokenKind::Symbol(Symbol::OrOr) {
            self.advance();
            let rhs = self.parse_and_expr()?;
            let span = lhs.span.cover(rhs.span);
            lhs = Node::new(
                ExprKind::Binary {
                    op: crate::ast::BinaryOp::Or,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Some(lhs)
    }

    fn parse_and_expr(&mut self) -> Option<Expr> {
        let mut lhs = self.parse_not_expr()?;
        while self.peek() == &TokenKind::Symbol(Symbol::AndAnd) {
            self.advance();
            let rhs = self.parse_not_expr()?;
            let span = lhs.span.cover(rhs.span);
            lhs = Node::new(
                ExprKind::Binary {
                    op: crate::ast::BinaryOp::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Some(lhs)
    }

    fn parse_not_expr(&mut self) -> Option<Expr> {
        if self.peek() == &TokenKind::Symbol(Symbol::Bang) {
            let start = self.peek_span();
            self.advance();
            let inner = self.parse_not_expr()?;
            let span = start.cover(inner.span);
            return Some(Node::new(ExprKind::Not(Box::new(inner)), span));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Option<Expr> {
        let lhs = self.parse_primary()?;
        let op = match self.peek() {
            TokenKind::Symbol(Symbol::EqEq) => Some(crate::ast::BinaryOp::Eq),
            TokenKind::Symbol(Symbol::NotEq) => Some(crate::ast::BinaryOp::NotEq),
            TokenKind::Symbol(Symbol::Lt) => Some(crate::ast::BinaryOp::Lt),
            TokenKind::Symbol(Symbol::LtEq) => Some(crate::ast::BinaryOp::LtEq),
            TokenKind::Symbol(Symbol::Gt) => Some(crate::ast::BinaryOp::Gt),
            TokenKind::Symbol(Symbol::GtEq) => Some(crate::ast::BinaryOp::GtEq),
            _ => None,
        };
        let Some(op) = op else {
            return Some(lhs);
        };
        self.advance();
        let rhs = self.parse_primary()?;
        let span = lhs.span.cover(rhs.span);
        Some(Node::new(
            ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            span,
        ))
    }

    fn parse_primary(&mut self) -> Option<Expr> {
        let start = self.peek_span();
        match self.peek().clone() {
            TokenKind::Str(segments) => {
                self.advance();
                let str_expr = self.build_str_expr(&segments);
                Some(Node::new(ExprKind::Str(str_expr), start))
            }
            TokenKind::Number(n) => {
                self.advance();
                Some(Node::new(ExprKind::Number(n), start))
            }
            TokenKind::Bool(b) => {
                self.advance();
                Some(Node::new(ExprKind::Bool(b), start))
            }
            TokenKind::Symbol(Symbol::LBracket) => self.parse_list(),
            TokenKind::Symbol(Symbol::LParen) => {
                self.advance();
                let inner = self.parse_expression()?;
                let end = self.expect_symbol(Symbol::RParen, "')'");
                let span = start.cover(end);
                Some(Node::new(ExprKind::Group(Box::new(inner)), span))
            }
            TokenKind::Ident(name) => self.parse_ident_primary(Ident {
                text: name,
                span: start,
            }),
            TokenKind::Keyword(kw) => {
                self.error(
                    start,
                    format!("reserved word '{}' used as identifier", kw.as_str()),
                );
                None
            }
            _ => {
                self.error(start, "unexpected token");
                None
            }
        }
    }

    /// `path | call | member_access`, all of which start with an `IDENT`
    /// already consumed by the caller as `ident`.
    fn parse_ident_primary(&mut self, ident: Ident) -> Option<Expr> {
        self.advance(); // the ident itself
        if self.peek() == &TokenKind::Symbol(Symbol::LParen) {
            let call = self.parse_call_from(ident);
            if self.peek() == &TokenKind::Symbol(Symbol::Dot) {
                let mut members = Vec::new();
                while self.peek() == &TokenKind::Symbol(Symbol::Dot) {
                    self.advance();
                    match self.expect_ident() {
                        Some(m) => members.push(m),
                        None => break,
                    }
                }
                let span = members.iter().fold(call.span, |acc, m| acc.cover(m.span));
                return Some(Node::new(
                    ExprKind::MemberAccess {
                        base: Box::new(call),
                        members,
                    },
                    span,
                ));
            }
            let span = call.span;
            return Some(Node::new(ExprKind::Call(call), span));
        }

        let mut segments = vec![ident];
        while self.peek() == &TokenKind::Symbol(Symbol::Dot) {
            self.advance();
            match self.expect_ident() {
                Some(seg) => segments.push(seg),
                None => break,
            }
        }
        let span = segments
            .iter()
            .fold(segments[0].span, |acc, seg| acc.cover(seg.span));

        if self.peek() == &TokenKind::Symbol(Symbol::LParen) && segments.len() > 1 {
            self.error(span, "dotted callee is invalid");
            return Some(Node::new(
                ExprKind::Invalid {
                    message: "dotted callee is invalid".to_owned(),
                },
                span,
            ));
        }

        Some(Node::new(ExprKind::Ref(Path { segments, span }), span))
    }

    fn parse_call_from(&mut self, callee: Ident) -> Call {
        let start = callee.span;
        self.advance(); // '('
        let mut args = Vec::new();
        if self.peek() != &TokenKind::Symbol(Symbol::RParen) {
            while let Some(arg) = self.parse_argument() {
                args.push(arg);
                if self.peek() == &TokenKind::Symbol(Symbol::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let end = self.expect_symbol(Symbol::RParen, "')'");
        let span = start.cover(end);
        Node::new(CallKind { callee, args }, span)
    }

    /// `argument = expression | IDENT "=" expression` — one token of
    /// lookahead (`IDENT` immediately followed by `=`) distinguishes named
    /// from positional (GRAMMAR.md §5.2).
    fn parse_argument(&mut self) -> Option<Argument> {
        if let TokenKind::Ident(name) = self.peek().clone()
            && self.peek_at(1) == Some(&TokenKind::Symbol(Symbol::Eq))
        {
            let span = self.peek_span();
            self.advance(); // ident
            self.advance(); // '='
            let value = self.parse_expression()?;
            return Some(Argument::Named {
                name: Ident { text: name, span },
                value,
            });
        }
        let value = self.parse_expression()?;
        Some(Argument::Positional(value))
    }

    fn parse_list(&mut self) -> Option<Expr> {
        let start = self.peek_span();
        self.advance(); // '['
        let mut items = Vec::new();
        if self.peek() != &TokenKind::Symbol(Symbol::RBracket) {
            while let Some(e) = self.parse_expression() {
                items.push(e);
                if self.peek() == &TokenKind::Symbol(Symbol::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let end = self.expect_symbol(Symbol::RBracket, "']'");
        let span = start.cover(end);
        Some(Node::new(ExprKind::List(items), span))
    }

    // -- strings ----------------------------------------------------------

    /// Builds a [`StrExpr`] from lexed [`StrSegment`]s, recursively parsing
    /// each interpolation's inner token slice as an expression.
    fn build_str_expr(&mut self, segments: &[StrSegment]) -> StrExpr {
        let mut parts = Vec::with_capacity(segments.len());
        for seg in segments {
            match seg {
                StrSegment::Literal { text, .. } => {
                    parts.push(StrPart::Literal(text.clone()));
                }
                StrSegment::Interp { tokens, span } => {
                    let expr = self
                        .parse_expr_from_interp_tokens(tokens)
                        .unwrap_or_else(|| {
                            Node::new(
                                ExprKind::Invalid {
                                    message: "invalid interpolation expression".to_owned(),
                                },
                                *span,
                            )
                        });
                    parts.push(StrPart::Interp(expr));
                }
            }
        }
        StrExpr { parts }
    }

    /// Parses the inner expression of a `${ ... }` interpolation. `tokens`
    /// is `[DollarBrace, ...inner tokens..., RBrace]` as produced by the
    /// lexer; an unterminated interpolation (already diagnosed by the
    /// lexer) may be missing the trailing `RBrace`.
    fn parse_expr_from_interp_tokens(&mut self, tokens: &[Token]) -> Option<Expr> {
        if tokens.is_empty() {
            return None;
        }
        let has_close = matches!(
            tokens.last().unwrap().kind,
            TokenKind::Symbol(Symbol::RBrace)
        );
        let inner_end = if has_close {
            tokens.len() - 1
        } else {
            tokens.len()
        };
        let inner: Vec<Token> = tokens[1..inner_end]
            .iter()
            .filter(|t| t.kind != TokenKind::Comment)
            .cloned()
            .collect();
        let eof_pos = tokens.last().map_or(0, |t| t.span.end);
        let mut sub_tokens = inner;
        sub_tokens.push(Token::new(TokenKind::Eof, eof_pos, eof_pos));

        let mut sub_parser = Parser {
            tokens: sub_tokens,
            pos: 0,
            diagnostics: Vec::new(),
        };
        let expr = sub_parser.parse_expression();
        self.diagnostics.append(&mut sub_parser.diagnostics);
        if !matches!(sub_parser.peek(), TokenKind::Eof) {
            let span = sub_parser.peek_span();
            self.error(span, "unexpected token in interpolation");
        }
        expr
    }

    /// A `STRING` token with no interpolation parts, as required for task
    /// names, `apply` targets, and `@` version literals. Emits a diagnostic
    /// and returns `None` if interpolation is present.
    fn plain_string_text(
        &mut self,
        segments: &[StrSegment],
        span: Span,
        what: &str,
    ) -> Option<String> {
        let mut out = String::new();
        for seg in segments {
            match seg {
                StrSegment::Literal { text, .. } => out.push_str(text),
                StrSegment::Interp { .. } => {
                    self.error(span, format!("{what} must not contain interpolation"));
                    return None;
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, StatementKind};
    use crate::token::Number;

    fn parse_ok(src: &str) -> File {
        let parsed = parse(src);
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics for {src:?}: {:?}",
            parsed.diagnostics
        );
        parsed.file
    }

    // -- one test per grammar construct (GRAMMAR.md §5) ------------------

    #[test]
    fn parses_if_else() {
        let file = parse_ok("if true { compileSdk 1 } else { compileSdk 2 }");
        assert_eq!(file.statements.len(), 1);
        let StatementKind::If(if_kind) = &file.statements[0].kind else {
            panic!("expected If statement");
        };
        assert!(matches!(if_kind.condition.kind, ExprKind::Bool(true)));
        assert_eq!(if_kind.then_branch.statements.len(), 1);
        assert!(matches!(if_kind.else_branch, Some(ElseBranch::Block(_))));
    }

    #[test]
    fn parses_else_if_chain() {
        let file = parse_ok(r#"if a == 1 { x 1 } else if a == 2 { x 2 } else { x 3 }"#);
        let StatementKind::If(outer) = &file.statements[0].kind else {
            panic!("expected If");
        };
        match &outer.else_branch {
            Some(ElseBranch::If(inner)) => {
                assert!(matches!(inner.kind.else_branch, Some(ElseBranch::Block(_))));
            }
            other => panic!("expected chained else-if, got {other:?}"),
        }
    }

    #[test]
    fn parses_block_statement_single_segment() {
        let file = parse_ok("android { compileSdk 37 }");
        let StatementKind::BlockStmt { path, block } = &file.statements[0].kind else {
            panic!("expected BlockStmt");
        };
        assert_eq!(path.head(), "android");
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn parses_block_statement_dotted_path() {
        let file = parse_ok("commonMain.deps { implementation coreKtx }");
        let StatementKind::BlockStmt { path, .. } = &file.statements[0].kind else {
            panic!("expected BlockStmt");
        };
        assert_eq!(path.segments.len(), 2);
        assert_eq!(path.segments[0].text, "commonMain");
        assert_eq!(path.segments[1].text, "deps");
    }

    #[test]
    fn parses_convention_def() {
        // Uses a non-hyphenated name deliberately: GRAMMAR.md §3's IDENT
        // rule (`letter { letter | digit }`) does not include `-`, so a
        // bare `convention android-app { }` (as GRAMMAR.md §6.3's own
        // example writes it) cannot lex as a single identifier — see the
        // "GRAMMAR.md vs worked examples" note in PROGRESS.md.
        let file = parse_ok("convention androidApp { compileSdk 37 }");
        let StatementKind::ConventionDef { name, block } = &file.statements[0].kind else {
            panic!("expected ConventionDef");
        };
        assert_eq!(name.text, "androidApp");
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn parses_fn_def_with_params() {
        let file = parse_ok("fn helper(a, b) { compileSdk a }");
        let StatementKind::FnDef {
            name,
            params,
            block,
        } = &file.statements[0].kind
        else {
            panic!("expected FnDef");
        };
        assert_eq!(name.text, "helper");
        assert_eq!(
            params.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn parses_fn_def_no_params() {
        let file = parse_ok("fn helper() { compileSdk 1 }");
        let StatementKind::FnDef { params, .. } = &file.statements[0].kind else {
            panic!("expected FnDef");
        };
        assert!(params.is_empty());
    }

    #[test]
    fn parses_task_def() {
        let file = parse_ok(r#"task "printConfig" { run { copy(from="a", to="b") } }"#);
        let StatementKind::TaskDef { name, block } = &file.statements[0].kind else {
            panic!("expected TaskDef");
        };
        assert_eq!(name, "printConfig");
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn parses_apply_statement() {
        let file = parse_ok(r#"apply "android-app""#);
        let StatementKind::Apply { name, name_span } = &file.statements[0].kind else {
            panic!("expected Apply");
        };
        assert_eq!(name, "android-app");
        assert_eq!(*name_span, Span { start: 6, end: 19 });
    }

    #[test]
    fn parses_assignment() {
        let file = parse_ok(r#"coreKtx = "androidx.core:core-ktx:1.16.0""#);
        let StatementKind::Assignment { path, value } = &file.statements[0].kind else {
            panic!("expected Assignment");
        };
        assert_eq!(path.head(), "coreKtx");
        assert!(matches!(value.kind, ExprKind::Str(_)));
    }

    #[test]
    fn parses_pair_statement() {
        let file = parse_ok("compileSdk 37");
        let StatementKind::Pair { key, value } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        assert_eq!(key.text, "compileSdk");
        assert!(matches!(value.kind, ExprKind::Number(Number::Int(37))));
    }

    #[test]
    fn parses_call_statement() {
        let file = parse_ok(r#"exec(command="echo")"#);
        let StatementKind::CallStmt(call) = &file.statements[0].kind else {
            panic!("expected CallStmt");
        };
        assert_eq!(call.kind.callee.text, "exec");
        assert_eq!(call.kind.args.len(), 1);
    }

    // -- expression constructs (GRAMMAR.md §5, §5.2) ----------------------

    #[test]
    fn parses_precedence_or_and_not_comparison() {
        // A leading key immediately followed by `(` always dispatches to
        // call_statement per GRAMMAR.md §5.1 ("(" and path is a single
        // IDENT), regardless of whitespace — so the value expression here
        // must not start with `(` right after the key.
        let file = parse_ok("result a == 1 || b == 2 && !(c == 3)");
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::Binary { op, .. } = &value.kind else {
            panic!("expected top-level Binary (||), got {:?}", value.kind);
        };
        assert_eq!(*op, BinaryOp::Or);
    }

    #[test]
    fn parses_not_expression() {
        let file = parse_ok("if !ready { compileSdk 1 }");
        let StatementKind::If(if_kind) = &file.statements[0].kind else {
            panic!("expected If");
        };
        assert!(matches!(if_kind.condition.kind, ExprKind::Not(_)));
    }

    #[test]
    fn parses_all_comparison_operators() {
        for (src, expected) in [
            ("a == 1", BinaryOp::Eq),
            ("a != 1", BinaryOp::NotEq),
            ("a < 1", BinaryOp::Lt),
            ("a <= 1", BinaryOp::LtEq),
            ("a > 1", BinaryOp::Gt),
            ("a >= 1", BinaryOp::GtEq),
        ] {
            let file = parse_ok(&format!("if {src} {{ x 1 }}"));
            let StatementKind::If(if_kind) = &file.statements[0].kind else {
                panic!("expected If for {src}");
            };
            let ExprKind::Binary { op, .. } = &if_kind.condition.kind else {
                panic!("expected Binary for {src}");
            };
            assert_eq!(*op, expected, "for {src}");
        }
    }

    #[test]
    fn parses_string_number_bool_literals() {
        let file = parse_ok(r#"a "text" "#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        assert!(matches!(value.kind, ExprKind::Str(_)));

        let file = parse_ok("a 3.5");
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        assert!(matches!(value.kind, ExprKind::Number(Number::Float(f)) if f == 3.5));

        let file = parse_ok("a false");
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        assert!(matches!(value.kind, ExprKind::Bool(false)));
    }

    #[test]
    fn parses_ref_path_expression() {
        let file = parse_ok("implementation appcompat");
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::Ref(path) = &value.kind else {
            panic!("expected Ref");
        };
        assert_eq!(path.head(), "appcompat");
    }

    #[test]
    fn parses_call_expression() {
        let file = parse_ok(r#"storePassword env("STORE_PASSWORD")"#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::Call(call) = &value.kind else {
            panic!("expected Call");
        };
        assert_eq!(call.kind.callee.text, "env");
        assert!(matches!(
            &call.kind.args[0],
            Argument::Positional(e) if matches!(e.kind, ExprKind::Str(_))
        ));
    }

    #[test]
    fn parses_call_with_named_args() {
        let file = parse_ok("versionName ver(major=0, minor=1, patch=2)");
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::Call(call) = &value.kind else {
            panic!("expected Call");
        };
        assert_eq!(call.kind.args.len(), 3);
        assert!(
            call.kind
                .args
                .iter()
                .all(|a| matches!(a, Argument::Named { .. }))
        );
    }

    #[test]
    fn parses_member_access() {
        let file = parse_ok(r#"storeFile props("signing.properties").storeFile"#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::MemberAccess { base, members } = &value.kind else {
            panic!("expected MemberAccess, got {:?}", value.kind);
        };
        assert_eq!(base.kind.callee.text, "props");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].text, "storeFile");
    }

    #[test]
    fn parses_chained_member_access() {
        let file = parse_ok(r#"x props("a").b.c"#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::MemberAccess { members, .. } = &value.kind else {
            panic!("expected MemberAccess");
        };
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn parses_list_literal() {
        let file = parse_ok(r#"proguardFiles [ "a.pro", "b.pro" ]"#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::List(items) = &value.kind else {
            panic!("expected List");
        };
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn parses_empty_list_literal() {
        let file = parse_ok("dependsOn [ ]");
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        assert!(matches!(&value.kind, ExprKind::List(items) if items.is_empty()));
    }

    #[test]
    fn parses_grouped_expression() {
        let file = parse_ok("if (a == 1) { x 1 }");
        let StatementKind::If(if_kind) = &file.statements[0].kind else {
            panic!("expected If");
        };
        assert!(matches!(if_kind.condition.kind, ExprKind::Group(_)));
    }

    #[test]
    fn parses_versioned_ref_name() {
        let file = parse_ok(r#"coreKtx = "androidx.core:core-ktx" @ coreVersion"#);
        let StatementKind::Assignment { value, .. } = &file.statements[0].kind else {
            panic!("expected Assignment");
        };
        let ExprKind::Versioned { version, .. } = &value.kind else {
            panic!("expected Versioned");
        };
        assert_eq!(*version, VersionRef::RefName("coreVersion".to_owned()));
    }

    #[test]
    fn parses_versioned_inline_string() {
        let file = parse_ok(
            r#"kotlinxCoroutines = "org.jetbrains.kotlinx:kotlinx-coroutines-core" @ "1.9.0""#,
        );
        let StatementKind::Assignment { value, .. } = &file.statements[0].kind else {
            panic!("expected Assignment");
        };
        let ExprKind::Versioned { version, .. } = &value.kind else {
            panic!("expected Versioned");
        };
        assert_eq!(*version, VersionRef::Version("1.9.0".to_owned()));
    }

    #[test]
    fn parses_string_interpolation_expression() {
        let file = parse_ok(r#"description "built for ${env("TIER")}""#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        let ExprKind::Str(str_expr) = &value.kind else {
            panic!("expected Str");
        };
        assert_eq!(str_expr.parts.len(), 2);
        assert!(matches!(str_expr.parts[0], StrPart::Literal(_)));
        match &str_expr.parts[1] {
            StrPart::Interp(expr) => assert!(matches!(expr.kind, ExprKind::Call(_))),
            other => panic!("expected Interp, got {other:?}"),
        }
    }

    #[test]
    fn parses_nested_string_interpolation() {
        let file = parse_ok(r#"x "${env("${env("NAME")}")}""#);
        let StatementKind::Pair { value, .. } = &file.statements[0].kind else {
            panic!("expected Pair");
        };
        // Just verifying this parses without diagnostics (checked in
        // parse_ok) and produces a Call whose argument is itself a Str
        // containing a further interpolation is enough to prove recursive
        // interpolation parsing works end-to-end.
        let ExprKind::Str(str_expr) = &value.kind else {
            panic!("expected Str");
        };
        let StrPart::Interp(outer_call) = &str_expr.parts[0] else {
            panic!("expected outer interpolation");
        };
        let ExprKind::Call(call) = &outer_call.kind else {
            panic!("expected outer Call");
        };
        let Argument::Positional(arg) = &call.kind.args[0] else {
            panic!("expected positional arg");
        };
        assert!(matches!(arg.kind, ExprKind::Str(_)));
    }

    // -- full-file snapshot: worked build.ulb example from GRAMMAR.md §6.2

    #[test]
    fn parses_full_build_ulb_example() {
        let src = r#"
plugin "android-application"

apply "android-app"
apply "env-signing"

android {
  namespace "com.example.app"
  compileSdk 37
  minSdk 24
  targetSdk 37
  applicationId "com.example.app"
  versionCode 7
  versionName ver(major=0, minor=1, patch=2)
}

buildTypes {
  debug { minifyEnabled false }
  release {
    minifyEnabled true
    proguardFiles [ "proguard-rules.pro" ]
  }
}

productFlavors {
  dimension "tier"
  free  { applicationIdSuffix ".free" }
  paid  { applicationIdSuffix ".paid" }
}

signing {
  storeFile   props("signing.properties").storeFile
  keyAlias    props("signing.properties").keyAlias
  storePassword env("STORE_PASSWORD")
  keyPassword   env("KEY_PASSWORD")
}

deps {
  implementation "androidx.core:core-ktx" @ coreVersion
  implementation appcompat
}

commonMain.deps {
  implementation kotlinxCoroutines
}
androidMain.deps {
  implementation "org.jetbrains.compose.ui:ui" @ composeVersion
}

task "printConfig" {
  description "Prints the resolved module configuration."
  dependsOn [ "compileReleaseKotlin", "bundleRelease" ]
  run {
    exec(command="echo", args=["hello", "from", "ulb"])
    copy(from="src/main/kotlin", to="out/merged-kotlin")
  }
}
"#;
        let parsed = parse(src);
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            parsed.diagnostics
        );
        // plugin "..." x1, apply x2, android, buildTypes, productFlavors,
        // signing, deps, commonMain.deps, androidMain.deps, task = 11
        assert_eq!(parsed.file.statements.len(), 11);
        assert!(matches!(
            parsed.file.statements[0].kind,
            StatementKind::Pair { .. } // plugin "..." is IDENT+STRING => Pair
        ));
        assert!(matches!(
            parsed.file.statements[1].kind,
            StatementKind::Apply { .. }
        ));
        // statements: plugin(0), apply(1), apply(2), android(3),
        // buildTypes(4), productFlavors(5), signing(6), deps(7),
        // commonMain.deps(8), androidMain.deps(9), task(10)
        let StatementKind::BlockStmt { block, .. } = &parsed.file.statements[3].kind else {
            panic!("expected android block at index 3");
        };
        assert_eq!(block.statements.len(), 7);
    }

    #[test]
    fn parses_full_libs_ulb_example() {
        let src = r#"
versions {
  coreVersion = "1.15.0"
  composeVersion = "1.8.0"
}

appcompat = "androidx.appcompat:appcompat:1.7.0"
coreKtx   = "androidx.core:core-ktx" @ coreVersion
ui        = "org.jetbrains.compose.ui:ui" @ composeVersion
kotlinxCoroutines = "org.jetbrains.kotlinx:kotlinx-coroutines-core" @ "1.9.0"

bundle {
  ui = [ ui, appcompat ]
}

plugins {
  androidApplication = "com.android.application" @ "8.7.0"
  kotlinMultiplatform = "org.jetbrains.kotlin.multiplatform" @ "2.1.0"
}
"#;
        let parsed = parse(src);
        assert!(
            parsed.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.file.statements.len(), 7);
    }

    // -- malformed-input / error-recovery tests (GRAMMAR.md §11 required
    //    diagnostic cases; each proves partial-AST + diagnostic recovery,
    //    not fail-fast) -----------------------------------------------

    #[test]
    fn recovers_unexpected_token() {
        let parsed = parse("] compileSdk 37");
        assert!(!parsed.diagnostics.is_empty());
        assert!(matches!(
            parsed.file.statements[0].kind,
            StatementKind::Invalid { .. }
        ));
        // parsing resumed after recovery: the well-formed statement after
        // the garbage token is still recovered correctly.
        assert!(matches!(
            parsed.file.statements[1].kind,
            StatementKind::Pair { .. }
        ));
    }

    #[test]
    fn recovers_unexpected_end_of_block() {
        let parsed = parse("android { compileSdk 37");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unexpected end of block"))
        );
        let StatementKind::BlockStmt { block, .. } = &parsed.file.statements[0].kind else {
            panic!("expected BlockStmt");
        };
        // the one well-formed statement before EOF was still recovered.
        assert_eq!(block.statements.len(), 1);
    }

    #[test]
    fn recovers_missing_value_after_path() {
        // Newlines are insignificant (GRAMMAR.md §1 design goal 3), so a
        // bare key followed by another identifier on the "next line" is
        // *not* a missing-value case — the following identifier is a valid
        // expression (a `Ref`) and becomes this pair's value. The
        // diagnosable case is a key followed by a token that cannot start
        // any expression at all, such as a block's closing `}`.
        let parsed = parse("android { versionName }");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.message.contains("missing value after path"))
        );
        let StatementKind::BlockStmt { block, .. } = &parsed.file.statements[0].kind else {
            panic!("expected BlockStmt");
        };
        assert!(matches!(
            block.statements[0].kind,
            StatementKind::Invalid { .. }
        ));
    }

    #[test]
    fn recovers_dotted_callee_as_statement() {
        let parsed = parse("a.b(1)\ncompileSdk 37");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.message.contains("dotted callee is invalid"))
        );
        assert!(matches!(
            parsed.file.statements[0].kind,
            StatementKind::Invalid { .. }
        ));
        assert!(matches!(
            parsed.file.statements[1].kind,
            StatementKind::Pair { .. }
        ));
    }

    #[test]
    fn recovers_dotted_callee_as_expression() {
        let parsed = parse("x a.b(1)");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.message.contains("dotted callee is invalid"))
        );
        let StatementKind::Pair { value, .. } = &parsed.file.statements[0].kind else {
            panic!("expected Pair (value recovered as Invalid expr)");
        };
        assert!(matches!(value.kind, ExprKind::Invalid { .. }));
    }

    #[test]
    fn recovers_unterminated_string_at_parser_level() {
        // The lexer already reports the unterminated-string diagnostic
        // (see lexer.rs tests); this proves the parser still produces a
        // usable statement around it rather than losing the whole file.
        let parsed = parse("x \"unterminated\ncompileSdk 37");
        assert!(!parsed.diagnostics.is_empty());
        assert!(matches!(
            parsed.file.statements.last().unwrap().kind,
            StatementKind::Pair { .. }
        ));
    }

    #[test]
    fn recovers_unterminated_block() {
        let parsed = parse("convention broken {");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unexpected end of block"))
        );
    }

    #[test]
    fn recovers_reserved_word_used_as_identifier() {
        // `if` used where a param name is expected.
        let parsed = parse("fn helper(if) { x 1 }");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.message.contains("reserved word 'if' used as identifier"))
        );
    }

    #[test]
    fn recovers_reserved_word_as_statement_start() {
        // `else` with no preceding `if` — reserved word where a statement
        // was expected.
        let parsed = parse("else { x 1 }");
        assert!(parsed.diagnostics.iter().any(|d| {
            d.message
                .contains("reserved word 'else' used as identifier")
        }));
        assert!(matches!(
            parsed.file.statements[0].kind,
            StatementKind::Invalid { .. }
        ));
    }

    #[test]
    fn deterministic_same_input_same_output() {
        let src = "android { compileSdk 37 }\nif a == 1 { x 1 } else { x 2 }";
        let a = parse(src);
        let b = parse(src);
        assert_eq!(a.file, b.file);
        assert_eq!(a.diagnostics, b.diagnostics);
    }
}

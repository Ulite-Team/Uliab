//! `ulb-lang` — lexer, parser, AST and evaluator for the ulb build DSL.
//!
//! This crate is the language core of the ulb build tool (GRAMMAR.md). It is
//! a standalone library with no CLI or build-tool dependencies so that three
//! consumers can reuse it without duplication: the `uliab` CLI build engine,
//! the `ulb-lsp` server (which walks the same typed AST the evaluator uses,
//! so semantic diagnostics always match evaluation), and the test suite.
//!
//! The pipeline is: [`lex`] produces span-annotated [`token::Token`]s with
//! recovery diagnostics; [`parse`] builds the [`ast::File`] tree; the
//! evaluator ([`eval`]) resolves it into a [`eval::Value`] module model.

#![warn(missing_docs)]

pub mod ast;
pub mod diagnostic;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;

pub use diagnostic::{Diagnostic, Severity};
pub use eval::{Definitions, EvalEnvironment, EvalOutcome, SettingsModel, SettingsOutcome, Value};
pub use lexer::{Lexed, lex};
pub use parser::{Parsed, parse};
pub use span::{LineIndex, Span};

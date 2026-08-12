//! Token definitions produced by the lexer and consumed by the parser.
//!
//! Every token carries a [`Span`]. Strings are the only token whose content is
//! not a flat string: interpolation (`"${env("X")}"`) is lexed into nested
//! tokens so the LSP can highlight and analyze expressions inside strings,
//! and so the parser can evaluate them directly.

use crate::span::Span;

/// A numeric literal: non-negative integer or decimal (GRAMMAR.md §3).
///
/// There is no unary minus in ulb, so numbers are always non-negative at the
/// token level; negative values are not expressible.
#[derive(Debug, Clone, PartialEq)]
pub enum Number {
    /// Integer literal (no decimal point).
    Int(i64),
    /// Decimal literal.
    Float(f64),
}

impl Number {
    /// Renders the value back as its literal text.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
        }
    }
}

/// Reserved words of the language (GRAMMAR.md §4).
///
/// `true`/`false` are not in this set: they lex as [`TokenKind::Bool`]
/// literals. Every other keyword is a control-flow or declaration word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    /// `if`
    If,
    /// `else`
    Else,
    /// `convention`
    Convention,
    /// `fn`
    Fn,
    /// `task`
    Task,
    /// `apply`
    Apply,
}

impl Keyword {
    /// The source text of the keyword.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::If => "if",
            Self::Else => "else",
            Self::Convention => "convention",
            Self::Fn => "fn",
            Self::Task => "task",
            Self::Apply => "apply",
        }
    }
}

/// Punctuation and operators (GRAMMAR.md §3 and §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `=`
    Eq,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `@` (version attach)
    At,
    /// `!`
    Bang,
    /// `==`
    EqEq,
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
    AndAnd,
    /// `||`
    OrOr,
    /// `${` — opens an interpolation inside a string (never appears outside)
    DollarBrace,
}

impl Symbol {
    /// The source text of the symbol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LBrace => "{",
            Self::RBrace => "}",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::Eq => "=",
            Self::Comma => ",",
            Self::Dot => ".",
            Self::At => "@",
            Self::Bang => "!",
            Self::EqEq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::AndAnd => "&&",
            Self::OrOr => "||",
            Self::DollarBrace => "${",
        }
    }
}

/// One part of a string literal: either plain text or an interpolation.
#[derive(Debug, Clone, PartialEq)]
pub enum StrSegment {
    /// Plain characters between escapes/interpolations. Escape sequences have
    /// been resolved; `span` covers the original source including escapes.
    Literal {
        /// Resolved text.
        text: String,
        /// Source span of the raw segment (escapes un-resolved).
        span: Span,
    },
    /// A `${ expression }` interpolation: the lexed inner expression between
    /// the `${` and matching `}` (both included as tokens).
    Interp {
        /// Tokens of `${` ... `}`, including the enclosing `DollarBrace` and
        /// `RBrace` tokens.
        tokens: Vec<Token>,
        /// Source span of the whole `${ ... }`.
        span: Span,
    },
}

/// A lexical token: kind plus source span.
///
/// # Examples
///
/// ```
/// use ulb_lang::token::{Number, Token, TokenKind};
/// use ulb_lang::span::Span;
///
/// let tokens = TokenKind::lex_all("compileSdk 37").unwrap();
/// assert_eq!(
///     tokens[1].kind,
///     TokenKind::Number(Number::Int(37))
/// );
/// assert_eq!(tokens[1].span, Span { start: 11, end: 13 });
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Source span of the token.
    pub span: Span,
}

impl Token {
    /// Convenience constructor for tests and manual token building.
    #[must_use]
    pub fn new(kind: TokenKind, start: u32, end: u32) -> Self {
        Self {
            kind,
            span: Span { start, end },
        }
    }
}

/// The kind of a token.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Identifier or contextual keyword (GRAMMAR.md §4).
    Ident(String),
    /// Numeric literal.
    Number(Number),
    /// String literal with interpolation segments.
    Str(Vec<StrSegment>),
    /// `true` or `false`.
    Bool(bool),
    /// Reserved keyword.
    Keyword(Keyword),
    /// Punctuation or operator.
    Symbol(Symbol),
    /// `//` line comment or `/* */` block comment (lexed for the LSP,
    /// ignored by the parser).
    Comment,
    /// End of input.
    Eof,
}

impl TokenKind {
    /// Lexes `source` into a token stream (for documentation examples).
    ///
    /// # Errors
    ///
    /// Returns an `Err` containing diagnostics if any token failed to lex;
    /// a successful lex still requires the caller to check [`crate::Lexed`].
    pub fn lex_all(source: &str) -> Result<Vec<Token>, Vec<crate::Diagnostic>> {
        crate::lex(source).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{Keyword, Number, StrSegment, Symbol, TokenKind};
    use crate::token::Token;

    #[test]
    fn keyword_text_roundtrips() {
        assert_eq!(Keyword::If.as_str(), "if");
        assert_eq!(Keyword::Else.as_str(), "else");
        assert_eq!(Keyword::Convention.as_str(), "convention");
        assert_eq!(Keyword::Fn.as_str(), "fn");
        assert_eq!(Keyword::Task.as_str(), "task");
        assert_eq!(Keyword::Apply.as_str(), "apply");
    }

    #[test]
    fn symbol_text_roundtrips() {
        assert_eq!(Symbol::EqEq.as_str(), "==");
        assert_eq!(Symbol::DollarBrace.as_str(), "${");
        assert_eq!(Symbol::RBrace.as_str(), "}");
    }

    #[test]
    fn number_text_roundtrips() {
        assert_eq!(Number::Int(37).as_text(), "37");
        assert_eq!(Number::Float(1.5).as_text(), "1.5");
    }

    #[test]
    fn token_constructor_creates_span() {
        let t = Token::new(TokenKind::Ident("foo".to_owned()), 3, 6);
        assert_eq!(t.span.start, 3);
        assert_eq!(t.span.end, 6);
        assert_eq!(t.kind, TokenKind::Ident("foo".to_owned()));
    }

    #[test]
    fn string_segments_are_typed() {
        let seg = StrSegment::Literal {
            text: String::new(),
            span: crate::span::Span { start: 0, end: 0 },
        };
        assert!(matches!(seg, StrSegment::Literal { .. }));
    }
}

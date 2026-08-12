//! Hand-written recursive-descent lexer for the ulb DSL.
//!
//! Produces a [`Token`] stream with source spans on every token, plus a list
//! of [`Diagnostic`]s. The lexer never aborts on bad input: invalid
//! characters are skipped, unterminated strings/comments/interpolations are
//! closed at a safe boundary, and lexing resumes. This is the same token
//! stream the LSP will render and analyze, so it is deliberately forgiving of
//! mid-edit source (GRAMMAR.md §11).
//!
//! Strings are the only non-flat token: `"${expr}"` interpolation is lexed
//! recursively into nested tokens (GRAMMAR.md §3), tracked via an
//! interpolation-depth counter so that nested strings inside interpolations
//! balance correctly.

use crate::diagnostic::{Diagnostic, Severity};
use crate::span::Span;
use crate::token::{Keyword, Number, StrSegment, Symbol, Token, TokenKind};

/// The result of lexing a source file: tokens (ending in `Eof`) plus any
/// diagnostics that were recovered from.
#[derive(Debug, Clone, PartialEq)]
pub struct Lexed {
    /// Tokens including a trailing [`TokenKind::Eof`].
    pub tokens: Vec<Token>,
    /// Diagnostics recovered from; empty means the lex was clean.
    pub diagnostics: Vec<Diagnostic>,
}

impl Lexed {
    /// `Ok(tokens)` when lexing was clean, `Err(diagnostics)` otherwise.
    pub fn ok(self) -> Result<Vec<Token>, Vec<Diagnostic>> {
        if self.diagnostics.is_empty() {
            Ok(self.tokens)
        } else {
            Err(self.diagnostics)
        }
    }
}

/// Lexes a whole source string into tokens plus diagnostics.
///
/// # Examples
///
/// ```
/// use ulb_lang::lexer::{lex, Lexed};
/// use ulb_lang::token::TokenKind;
///
/// let Lexed { tokens, diagnostics } = lex(r#"compileSdk 37"#);
/// assert!(diagnostics.is_empty());
/// assert!(matches!(
///     &tokens[0].kind,
///     TokenKind::Ident(name) if name == "compileSdk"
/// ));
/// ```
#[must_use]
pub fn lex(source: &str) -> Lexed {
    let mut lexer = Lexer {
        source,
        pos: 0,
        interp_level: 0,
        diagnostics: Vec::new(),
    };
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        let at_end = token.kind == TokenKind::Eof;
        tokens.push(token);
        if at_end {
            break;
        }
    }
    Lexed {
        tokens,
        diagnostics: lexer.diagnostics,
    }
}

struct Lexer<'a> {
    source: &'a str,
    pos: usize,
    interp_level: usize,
    diagnostics: Vec<Diagnostic>,
}

impl Lexer<'_> {
    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.source.as_bytes().get(self.pos) {
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.pos).copied()
    }

    fn peek_second(&self) -> Option<u8> {
        self.source.as_bytes().get(self.pos + 1).copied()
    }

    fn error_at(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            span,
            severity: Severity::Error,
            message: message.into(),
        });
    }

    fn next_token(&mut self) -> Token {
        loop {
            self.skip_whitespace();
            let Some(byte) = self.peek() else {
                return Token::new(TokenKind::Eof, self.pos as u32, self.pos as u32);
            };
            let start = self.pos as u32;
            let token = match byte {
                b'/' if self.peek_second() == Some(b'/') => self.lex_line_comment(start),
                b'/' if self.peek_second() == Some(b'*') => self.lex_block_comment(start),
                b'"' => self.lex_string(start),
                b'0'..=b'9' => self.lex_number(start),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident(start),
                b'{' => {
                    if self.interp_level > 0 {
                        self.interp_level += 1;
                    }
                    self.symbol(start, Symbol::LBrace, 1)
                }
                b'}' => self.lex_rbrace(start),
                b'(' => self.symbol(start, Symbol::LParen, 1),
                b')' => self.symbol(start, Symbol::RParen, 1),
                b'[' => self.symbol(start, Symbol::LBracket, 1),
                b']' => self.symbol(start, Symbol::RBracket, 1),
                b'=' if self.peek_second() == Some(b'=') => self.symbol(start, Symbol::EqEq, 2),
                b'=' => self.symbol(start, Symbol::Eq, 1),
                b',' => self.symbol(start, Symbol::Comma, 1),
                b'.' => self.symbol(start, Symbol::Dot, 1),
                b'@' => self.symbol(start, Symbol::At, 1),
                b'!' if self.peek_second() == Some(b'=') => self.symbol(start, Symbol::NotEq, 2),
                b'!' => self.symbol(start, Symbol::Bang, 1),
                b'<' if self.peek_second() == Some(b'=') => self.symbol(start, Symbol::LtEq, 2),
                b'<' => self.symbol(start, Symbol::Lt, 1),
                b'>' if self.peek_second() == Some(b'=') => self.symbol(start, Symbol::GtEq, 2),
                b'>' => self.symbol(start, Symbol::Gt, 1),
                b'&' if self.peek_second() == Some(b'&') => self.symbol(start, Symbol::AndAnd, 2),
                b'|' if self.peek_second() == Some(b'|') => self.symbol(start, Symbol::OrOr, 2),
                _ => {
                    let ch = self.source[self.pos..].chars().next().unwrap_or('\u{FFFD}');
                    self.error_at(Span::empty(start), format!("unexpected character {ch:?}"));
                    self.pos += ch.len_utf8();
                    continue;
                }
            };
            return token;
        }
    }

    fn symbol(&mut self, start: u32, symbol: Symbol, width: usize) -> Token {
        self.pos += width;
        Token::new(TokenKind::Symbol(symbol), start, start + width as u32)
    }

    /// `}` closes an interpolation when one is open; otherwise it is a plain
    /// right brace.
    fn lex_rbrace(&mut self, start: u32) -> Token {
        self.pos += 1;
        if self.interp_level > 0 {
            self.interp_level -= 1;
        }
        Token::new(TokenKind::Symbol(Symbol::RBrace), start, start + 1)
    }

    fn lex_line_comment(&mut self, start: u32) -> Token {
        self.pos += 2;
        while let Some(byte) = self.peek() {
            if byte == b'\n' {
                break;
            }
            self.pos += 1;
        }
        Token::new(TokenKind::Comment, start, self.pos as u32)
    }

    fn lex_block_comment(&mut self, start: u32) -> Token {
        self.pos += 2;
        while self.pos + 1 < self.source.len() {
            if self.source.as_bytes()[self.pos] == b'*'
                && self.source.as_bytes()[self.pos + 1] == b'/'
            {
                self.pos += 2;
                return Token::new(TokenKind::Comment, start, self.pos as u32);
            }
            self.pos += 1;
        }
        self.error_at(
            Span {
                start,
                end: self.source.len() as u32,
            },
            "unterminated block comment",
        );
        self.pos = self.source.len();
        Token::new(TokenKind::Comment, start, self.pos as u32)
    }

    fn lex_ident(&mut self, start: u32) -> Token {
        while let Some(byte) = self.peek() {
            if matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.source[start as usize..self.pos];
        let kind = match text {
            "if" => TokenKind::Keyword(Keyword::If),
            "else" => TokenKind::Keyword(Keyword::Else),
            "convention" => TokenKind::Keyword(Keyword::Convention),
            "fn" => TokenKind::Keyword(Keyword::Fn),
            "task" => TokenKind::Keyword(Keyword::Task),
            "apply" => TokenKind::Keyword(Keyword::Apply),
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            _ => TokenKind::Ident(text.to_owned()),
        };
        Token::new(kind, start, self.pos as u32)
    }

    fn lex_number(&mut self, start: u32) -> Token {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.peek_second(), Some(b'0'..=b'9')) {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = &self.source[start as usize..self.pos];
        let kind = if is_float {
            TokenKind::Number(Number::Float(
                text.parse::<f64>().expect("digit run parses as f64"),
            ))
        } else {
            match text.parse::<i64>() {
                Ok(int) => TokenKind::Number(Number::Int(int)),
                Err(_) => {
                    self.error_at(
                        Span {
                            start,
                            end: self.pos as u32,
                        },
                        "integer literal out of range",
                    );
                    return Token::new(TokenKind::Number(Number::Int(0)), start, self.pos as u32);
                }
            }
        };
        Token::new(kind, start, self.pos as u32)
    }

    fn lex_string(&mut self, start: u32) -> Token {
        self.pos += 1;
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut segment_start = start + 1;
        loop {
            let Some(byte) = self.peek() else {
                self.error_at(
                    Span {
                        start,
                        end: self.pos as u32,
                    },
                    "unterminated string literal",
                );
                break;
            };
            match byte {
                b'"' => {
                    if !literal.is_empty() {
                        parts.push(StrSegment::Literal {
                            span: Span {
                                start: segment_start,
                                end: self.pos as u32,
                            },
                            text: std::mem::take(&mut literal),
                        });
                    }
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    let esc_start = self.pos;
                    self.pos += 1;
                    let resolved = match self.peek() {
                        Some(b'"') => Some('"'),
                        Some(b'\\') => Some('\\'),
                        Some(b'n') => Some('\n'),
                        Some(b't') => Some('\t'),
                        Some(b'r') => Some('\r'),
                        _ => None,
                    };
                    match resolved {
                        Some(ch) => {
                            self.pos += 1;
                            literal.push(ch);
                        }
                        None => {
                            self.error_at(
                                Span {
                                    start: esc_start as u32,
                                    end: self.pos as u32,
                                },
                                "invalid escape sequence",
                            );
                            literal.push('\\');
                        }
                    }
                }
                b'\n' => {
                    self.error_at(
                        Span {
                            start,
                            end: self.pos as u32,
                        },
                        "unterminated string literal (newline before closing quote)",
                    );
                    break;
                }
                b'$' if self.peek_second() == Some(b'{') => {
                    if !literal.is_empty() {
                        parts.push(StrSegment::Literal {
                            span: Span {
                                start: segment_start,
                                end: self.pos as u32,
                            },
                            text: std::mem::take(&mut literal),
                        });
                    }
                    let interp_start = self.pos;
                    self.pos += 2;
                    let mut tokens = vec![Token::new(
                        TokenKind::Symbol(Symbol::DollarBrace),
                        interp_start as u32,
                        self.pos as u32,
                    )];
                    self.interp_level += 1;
                    let start_level = self.interp_level;
                    let closed = loop {
                        let token = self.next_token();
                        if token.kind == TokenKind::Eof {
                            break false;
                        }
                        let is_close = matches!(token.kind, TokenKind::Symbol(Symbol::RBrace))
                            && self.interp_level < start_level;
                        tokens.push(token);
                        if is_close {
                            break true;
                        }
                    };
                    let close = tokens.last().map_or(self.pos, |t| t.span.end as usize);
                    parts.push(StrSegment::Interp {
                        span: Span {
                            start: interp_start as u32,
                            end: close as u32,
                        },
                        tokens,
                    });
                    if !closed {
                        self.error_at(
                            Span {
                                start: interp_start as u32,
                                end: close as u32,
                            },
                            "unterminated interpolation",
                        );
                    }
                    segment_start = self.pos as u32;
                }
                _ => {
                    let ch = self.source[self.pos..].chars().next().unwrap_or('\u{FFFD}');
                    literal.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        if !literal.is_empty() {
            parts.push(StrSegment::Literal {
                span: Span {
                    start: segment_start,
                    end: self.pos as u32,
                },
                text: literal,
            });
        }
        Token::new(TokenKind::Str(parts), start, self.pos as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::{Lexed, Number, StrSegment, Symbol, TokenKind, lex};
    use crate::span::Span;
    use crate::token::Keyword;

    fn kinds(source: &str) -> Vec<TokenKind> {
        let Lexed { tokens, .. } = lex(source);
        tokens.into_iter().map(|t| t.kind).collect()
    }

    fn kinds_with_diagnostics(source: &str) -> Vec<(TokenKind, String)> {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(source);
        let diags: Vec<String> = diagnostics.into_iter().map(|d| d.message).collect();
        tokens
            .into_iter()
            .map(|t| (t.kind, diags.join(" | ")))
            .collect()
    }

    #[test]
    fn lexes_identifier_and_dotted_path() {
        assert_eq!(
            kinds("commonMain.deps"),
            vec![
                TokenKind::Ident("commonMain".to_owned()),
                TokenKind::Symbol(Symbol::Dot),
                TokenKind::Ident("deps".to_owned()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_underscore_identifier() {
        assert_eq!(
            kinds("_private"),
            vec![TokenKind::Ident("_private".to_owned()), TokenKind::Eof]
        );
    }

    #[test]
    fn lexes_integer_and_float_numbers() {
        assert_eq!(
            kinds("37 1.5"),
            vec![
                TokenKind::Number(Number::Int(37)),
                TokenKind::Number(Number::Float(1.5)),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn number_out_of_range_is_diagnosed_and_recovered() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex("99999999999999999999");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].kind, TokenKind::Number(Number::Int(0)));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "integer literal out of range");
    }

    #[test]
    fn lexes_boolean_literals() {
        assert_eq!(
            kinds("true false"),
            vec![
                TokenKind::Bool(true),
                TokenKind::Bool(false),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_all_reserved_keywords() {
        assert_eq!(
            kinds("if else convention fn task apply"),
            vec![
                TokenKind::Keyword(Keyword::If),
                TokenKind::Keyword(Keyword::Else),
                TokenKind::Keyword(Keyword::Convention),
                TokenKind::Keyword(Keyword::Fn),
                TokenKind::Keyword(Keyword::Task),
                TokenKind::Keyword(Keyword::Apply),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_all_symbols() {
        assert_eq!(
            kinds("{ } ( ) [ ] = , . @ ! == != < <= > >= && ||"),
            vec![
                TokenKind::Symbol(Symbol::LBrace),
                TokenKind::Symbol(Symbol::RBrace),
                TokenKind::Symbol(Symbol::LParen),
                TokenKind::Symbol(Symbol::RParen),
                TokenKind::Symbol(Symbol::LBracket),
                TokenKind::Symbol(Symbol::RBracket),
                TokenKind::Symbol(Symbol::Eq),
                TokenKind::Symbol(Symbol::Comma),
                TokenKind::Symbol(Symbol::Dot),
                TokenKind::Symbol(Symbol::At),
                TokenKind::Symbol(Symbol::Bang),
                TokenKind::Symbol(Symbol::EqEq),
                TokenKind::Symbol(Symbol::NotEq),
                TokenKind::Symbol(Symbol::Lt),
                TokenKind::Symbol(Symbol::LtEq),
                TokenKind::Symbol(Symbol::Gt),
                TokenKind::Symbol(Symbol::GtEq),
                TokenKind::Symbol(Symbol::AndAnd),
                TokenKind::Symbol(Symbol::OrOr),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_single_equals_as_eq_not_eqeq() {
        assert_eq!(
            kinds("="),
            vec![TokenKind::Symbol(Symbol::Eq), TokenKind::Eof]
        );
    }

    #[test]
    fn lexes_plain_string() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(r#""androidx.core""#);
        assert!(diagnostics.is_empty());
        match &tokens[0].kind {
            TokenKind::Str(parts) => {
                assert_eq!(
                    parts,
                    &[StrSegment::Literal {
                        text: "androidx.core".to_owned(),
                        span: Span { start: 1, end: 14 },
                    }]
                );
            }
            other => panic!("expected string token, got {other:?}"),
        }
    }

    #[test]
    fn lexes_string_escapes() {
        let Lexed { tokens, .. } = lex(r#""a\"b\\c\nd\te\rf""#);
        match &tokens[0].kind {
            TokenKind::Str(parts) => {
                let StrSegment::Literal { text, .. } = &parts[0] else {
                    panic!("expected literal segment");
                };
                assert_eq!(text, "a\"b\\c\nd\te\rf");
            }
            other => panic!("expected string token, got {other:?}"),
        }
    }

    #[test]
    fn string_preserves_multibyte_characters() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(r#""مرحبا café""#);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        match &tokens[0].kind {
            TokenKind::Str(parts) => {
                let StrSegment::Literal { text, .. } = &parts[0] else {
                    panic!("expected literal segment");
                };
                assert_eq!(text, "مرحبا café");
            }
            other => panic!("expected string token, got {other:?}"),
        }
    }

    #[test]
    fn invalid_escape_is_diagnosed() {
        let Lexed { diagnostics, .. } = lex(r#""\q""#);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "invalid escape sequence");
    }

    #[test]
    fn lexes_interpolation_inside_string() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(r#""name: ${env("X")}""#);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        match &tokens[0].kind {
            TokenKind::Str(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], StrSegment::Literal { text, .. } if text == "name: "));
                let StrSegment::Interp {
                    tokens: interp,
                    span,
                } = &parts[1]
                else {
                    panic!("expected interpolation segment");
                };
                assert_eq!(span.start, 7);
                assert!(matches!(
                    interp[0].kind,
                    TokenKind::Symbol(Symbol::DollarBrace)
                ));
                assert!(matches!(&interp[1].kind, TokenKind::Ident(name) if name == "env"));
                assert!(matches!(interp[2].kind, TokenKind::Symbol(Symbol::LParen)));
                match &interp[3].kind {
                    TokenKind::Str(inner) => {
                        assert!(
                            matches!(&inner[0], StrSegment::Literal { text, .. } if text == "X")
                        );
                    }
                    other => panic!("expected nested string token, got {other:?}"),
                }
                assert!(matches!(interp[4].kind, TokenKind::Symbol(Symbol::RParen)));
                assert!(matches!(interp[5].kind, TokenKind::Symbol(Symbol::RBrace)));
            }
            other => panic!("expected string token, got {other:?}"),
        }
    }

    #[test]
    fn lexes_nested_interpolation() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(r#""${env("${x}")}""#);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        match &tokens[0].kind {
            TokenKind::Str(parts) => {
                let StrSegment::Interp { tokens: outer, .. } = &parts[0] else {
                    panic!("expected interpolation");
                };
                let TokenKind::Str(inner) = &outer[3].kind else {
                    panic!("expected nested string");
                };
                let StrSegment::Interp { .. } = &inner[0] else {
                    panic!("expected nested interpolation");
                };
            }
            other => panic!("expected string token, got {other:?}"),
        }
    }

    #[test]
    fn newline_between_quotes_is_diagnosed() {
        let (kinds, message) = kinds_with_diagnostics("\"abc\ndef\"")[0].clone();
        assert!(matches!(kinds, TokenKind::Str(_)));
        assert!(message.contains("newline"), "got {message}");
    }

    #[test]
    fn unterminated_string_is_diagnosed_and_returns_partial_token() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(r#""unterminated"#);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("unterminated string"));
        assert!(matches!(&tokens[0].kind, TokenKind::Str(_)));
        assert!(matches!(tokens[1].kind, TokenKind::Eof));
    }

    #[test]
    fn unterminated_interpolation_is_diagnosed() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex(r#""${env("X")"#);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("unterminated interpolation")),
            "{diagnostics:?}"
        );
        let TokenKind::Str(parts) = &tokens[0].kind else {
            panic!("expected string token");
        };
        let StrSegment::Interp { tokens: interp, .. } = &parts[0] else {
            panic!("expected interpolation segment");
        };
        assert!(
            !interp.iter().any(|t| t.kind == TokenKind::Eof),
            "interpolation tokens must not contain Eof"
        );
    }

    #[test]
    fn lexes_line_comment() {
        let Lexed { tokens, .. } = lex("compileSdk 37 // the answer");
        assert_eq!(tokens.len(), 4);
        assert!(matches!(tokens[2].kind, TokenKind::Comment));
        let Span { start, end } = tokens[2].span;
        assert_eq!(
            &"compileSdk 37 // the answer"[start as usize..end as usize],
            "// the answer"
        );
    }

    #[test]
    fn lexes_block_comment() {
        let Lexed { tokens, .. } = lex("/* a\nb */ android");
        assert!(matches!(tokens[0].kind, TokenKind::Comment));
        assert!(matches!(&tokens[1].kind, TokenKind::Ident(name) if name == "android"));
    }

    #[test]
    fn unterminated_block_comment_is_diagnosed() {
        let Lexed {
            tokens,
            diagnostics,
        } = lex("/* never closed");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "unterminated block comment");
        assert!(matches!(tokens[0].kind, TokenKind::Comment));
        assert!(matches!(tokens[1].kind, TokenKind::Eof));
    }

    #[test]
    fn unexpected_characters_are_diagnosed_and_skipped() {
        let (kinds, message) = kinds_with_diagnostics("a $ b")[0].clone();
        assert!(matches!(kinds, TokenKind::Ident(name) if name == "a"));
        assert!(message.contains("unexpected character"), "got {message}");
        let (kinds, _) = kinds_with_diagnostics("a $ b")[1].clone();
        assert!(matches!(kinds, TokenKind::Ident(name) if name == "b"));
    }

    #[test]
    fn tokens_carry_source_spans() {
        let Lexed { tokens, .. } = lex("compileSdk 37");
        assert_eq!(
            tokens[1].span,
            Span { start: 11, end: 13 },
            "spans are byte offsets into the source"
        );
        assert_eq!(tokens[0].span, Span { start: 0, end: 10 });
    }

    #[test]
    fn trailing_eof_token_is_always_last() {
        for source in ["", "android { }", "env(\"X\")"] {
            let Lexed { tokens, .. } = lex(source);
            assert!(matches!(
                tokens.last().map(|t| &t.kind),
                Some(TokenKind::Eof)
            ));
        }
    }
}

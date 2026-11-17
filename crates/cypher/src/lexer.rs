//! Tokenizer for Cypher.
//!
//! Keywords are not distinguished from identifiers here: Cypher keywords are
//! case-insensitive and context dependent, so the parser checks identifier text.
//! Arrows (`->`, `<-`) are left as separate `<`, `-`, `>` tokens for the same reason.

use graphbuff_core::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Unquoted identifier or keyword.
    Ident(String),
    /// Backtick-quoted identifier (never a keyword).
    QuotedIdent(String),
    /// Integer literal. `i64::MIN` stands for the magnitude 2^63, which is only valid
    /// right after a unary minus (`-9223372036854775808`).
    Int(i64),
    Float(f64),
    Str(String),
    /// `$name` parameter.
    Param(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    DotDot,
    Colon,
    Semicolon,
    Pipe,
    /// `&` (label expressions)
    Amp,
    /// `!` on its own (label expressions)
    Bang,
    Plus,
    PlusEq,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
    RegexMatch,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn tokenize(src: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut i = 0;
    let peek = |at: usize| src[at..].chars().next();

    while let Some(c) = peek(i) {
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if src[i..].starts_with("//") {
            i = src[i..].find('\n').map_or(src.len(), |p| i + p);
            continue;
        }
        if src[i..].starts_with("/*") {
            match src[i + 2..].find("*/") {
                Some(p) => i = i + 2 + p + 2,
                None => return Err(Error::syntax("unterminated block comment", i)),
            }
            continue;
        }

        let start = i;
        let next = peek(i + c.len_utf8());
        let (tok, len) = match c {
            '(' => (Tok::LParen, 1),
            ')' => (Tok::RParen, 1),
            '[' => (Tok::LBracket, 1),
            ']' => (Tok::RBracket, 1),
            '{' => (Tok::LBrace, 1),
            '}' => (Tok::RBrace, 1),
            ',' => (Tok::Comma, 1),
            ':' => (Tok::Colon, 1),
            ';' => (Tok::Semicolon, 1),
            '|' => (Tok::Pipe, 1),
            '-' => (Tok::Minus, 1),
            '*' => (Tok::Star, 1),
            '/' => (Tok::Slash, 1),
            '%' => (Tok::Percent, 1),
            '^' => (Tok::Caret, 1),
            '.' if next == Some('.') => (Tok::DotDot, 2),
            '.' if next.is_some_and(|d| d.is_ascii_digit()) => {
                let (tok, end) = lex_number(src, i)?;
                (tok, end - i)
            }
            '.' => (Tok::Dot, 1),
            '+' if next == Some('=') => (Tok::PlusEq, 2),
            '+' => (Tok::Plus, 1),
            '<' if next == Some('=') => (Tok::Le, 2),
            '<' if next == Some('>') => (Tok::Neq, 2),
            '<' => (Tok::Lt, 1),
            '>' if next == Some('=') => (Tok::Ge, 2),
            '>' => (Tok::Gt, 1),
            '!' if next == Some('=') => (Tok::Neq, 2),
            '!' => (Tok::Bang, 1),
            '&' => (Tok::Amp, 1),
            '=' if next == Some('~') => (Tok::RegexMatch, 2),
            '=' => (Tok::Eq, 1),
            '\'' | '"' => {
                let (s, end) = lex_string(src, i, c)?;
                (Tok::Str(s), end - i)
            }
            '`' => {
                let (s, end) = lex_quoted_ident(src, i)?;
                (Tok::QuotedIdent(s), end - i)
            }
            '$' => {
                let name_start = i + 1;
                let mut j = name_start;
                while let Some(ch) = peek(j).filter(|ch| is_ident_char(*ch)) {
                    j += ch.len_utf8();
                }
                if j == name_start {
                    return Err(Error::syntax("expected parameter name after '$'", i));
                }
                (Tok::Param(src[name_start..j].to_string()), j - i)
            }
            c if c.is_ascii_digit() => {
                let (tok, end) = lex_number(src, i)?;
                (tok, end - i)
            }
            c if is_ident_start(c) => {
                let mut j = i;
                while let Some(ch) = peek(j).filter(|ch| is_ident_char(*ch)) {
                    j += ch.len_utf8();
                }
                (Tok::Ident(src[i..j].to_string()), j - i)
            }
            other => return Err(Error::syntax(format!("unexpected character '{other}'"), i)),
        };
        i += len;
        out.push(Token { tok, start, end: i });
    }
    out.push(Token {
        tok: Tok::Eof,
        start: src.len(),
        end: src.len(),
    });
    Ok(out)
}

fn lex_number(src: &str, start: usize) -> Result<(Tok, usize)> {
    let b = src.as_bytes();
    if b[start] == b'0' && start + 1 < b.len() && matches!(b[start + 1], b'x' | b'X' | b'o' | b'O') {
        return lex_radix(src, start);
    }
    let mut j = start;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    let mut is_float = false;
    // A fraction needs a digit after the dot, so `1..3` lexes as `1`, `..`, `3`.
    if j + 1 < b.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
        is_float = true;
        j += 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
    }
    if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
        let mut k = j + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        if k < b.len() && b[k].is_ascii_digit() {
            is_float = true;
            j = k;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
        }
    }
    let text = &src[start..j];
    if is_float {
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok((Tok::Float(f), j)),
            Ok(_) => Err(Error::syntax(format!("float literal '{text}' is too large"), start)),
            Err(_) => Err(Error::syntax(format!("invalid float literal '{text}'"), start)),
        }
    } else {
        int_token(text.parse::<u64>().ok(), text, start, j)
    }
}

/// `0x1F` (hexadecimal) or `0o17` (octal).
fn lex_radix(src: &str, start: usize) -> Result<(Tok, usize)> {
    let b = src.as_bytes();
    let radix = if matches!(b[start + 1], b'x' | b'X') { 16 } else { 8 };
    let mut j = start + 2;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    let text = &src[start..j];
    let digits = &src[start + 2..j];
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(Error::syntax(format!("invalid integer literal '{text}'"), start));
    }
    int_token(u64::from_str_radix(digits, radix).ok(), text, start, j)
}

/// An integer token from its magnitude; 2^63 becomes `i64::MIN` for the parser to negate.
fn int_token(magnitude: Option<u64>, text: &str, start: usize, end: usize) -> Result<(Tok, usize)> {
    match magnitude {
        Some(n) if n <= i64::MAX as u64 => Ok((Tok::Int(n as i64), end)),
        Some(n) if n == 1 << 63 => Ok((Tok::Int(i64::MIN), end)),
        _ => Err(Error::syntax(format!("integer literal '{text}' is too large"), start)),
    }
}

fn lex_string(src: &str, start: usize, quote: char) -> Result<(String, usize)> {
    let mut out = String::new();
    let mut chars = src[start + 1..].char_indices();
    while let Some((off, c)) = chars.next() {
        let pos = start + 1 + off;
        if c == quote {
            return Ok((out, pos + 1));
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some((_, e)) = chars.next() else { break };
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '0' => out.push('\0'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'u' => {
                let hex: String = (0..4)
                    .filter_map(|_| chars.next().map(|(_, h)| h))
                    .collect();
                let ch = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| {
                        Error::syntax(format!("invalid unicode escape '\\u{hex}'"), pos)
                    })?;
                out.push(ch);
            }
            other => {
                return Err(Error::syntax(
                    format!("invalid escape sequence '\\{other}'"),
                    pos,
                ));
            }
        }
    }
    Err(Error::syntax("unterminated string literal", start))
}

fn lex_quoted_ident(src: &str, start: usize) -> Result<(String, usize)> {
    let mut out = String::new();
    let rest = &src[start + 1..];
    let mut chars = rest.char_indices().peekable();
    while let Some((off, c)) = chars.next() {
        if c == '`' {
            if matches!(chars.peek(), Some((_, '`'))) {
                chars.next();
                out.push('`');
                continue;
            }
            return Ok((out, start + 1 + off + 1));
        }
        out.push(c);
    }
    Err(Error::syntax("unterminated quoted identifier", start))
}

/// Splits a script into individual statements on top-level `;`, ignoring
/// semicolons inside strings and comments. Empty statements are dropped.
pub fn split_statements(src: &str) -> Result<Vec<&str>> {
    let tokens = tokenize(src)?;
    let mut out = Vec::new();
    let mut stmt_start: Option<usize> = None;
    for t in &tokens {
        match t.tok {
            Tok::Semicolon | Tok::Eof => {
                if let Some(s) = stmt_start.take() {
                    out.push(src[s..t.start].trim());
                }
            }
            _ => {
                stmt_start.get_or_insert(t.start);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        tokenize(s).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn arrows_and_ranges() {
        assert_eq!(
            toks("(a)-[*1..3]->(b)"),
            vec![
                Tok::LParen,
                Tok::Ident("a".into()),
                Tok::RParen,
                Tok::Minus,
                Tok::LBracket,
                Tok::Star,
                Tok::Int(1),
                Tok::DotDot,
                Tok::Int(3),
                Tok::RBracket,
                Tok::Minus,
                Tok::Gt,
                Tok::LParen,
                Tok::Ident("b".into()),
                Tok::RParen,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn number_forms() {
        assert_eq!(
            toks("0x1aF 0o17 .5 .1e-5 0X10 1..3"),
            vec![
                Tok::Int(0x1af),
                Tok::Int(0o17),
                Tok::Float(0.5),
                Tok::Float(0.1e-5),
                Tok::Int(16),
                Tok::Int(1),
                Tok::DotDot,
                Tok::Int(3),
                Tok::Eof
            ]
        );
        assert_eq!(toks("0x8000000000000000")[0], Tok::Int(i64::MIN));
        assert_eq!(toks("n.k")[1], Tok::Dot);
        for bad in ["0x", "0x1g", "0o8", "0x10000000000000000", "9223372036854775809", "1e999"] {
            assert!(tokenize(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn literals() {
        assert_eq!(
            toks("1.5 2e3 'it\\'s' \"x\" $p `we ird`"),
            vec![
                Tok::Float(1.5),
                Tok::Float(2000.0),
                Tok::Str("it's".into()),
                Tok::Str("x".into()),
                Tok::Param("p".into()),
                Tok::QuotedIdent("we ird".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            toks("a // c\n /* b */ <> !="),
            vec![Tok::Ident("a".into()), Tok::Neq, Tok::Neq, Tok::Eof]
        );
    }

    #[test]
    fn split() {
        let s = split_statements("CREATE (a {x: ';'}); MATCH (n) RETURN n;  ;").unwrap();
        assert_eq!(s, vec!["CREATE (a {x: ';'})", "MATCH (n) RETURN n"]);
    }
}

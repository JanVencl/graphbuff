//! Splitting Cypher text into statements at the `;` that end them: not those inside
//! strings, quoted names or comments.

/// Where the scan is.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Code,
    Single,
    Double,
    Backtick,
    LineComment,
    BlockComment,
}

/// The statements that `text` completes (each without its `;`, trimmed, and only those
/// with more than comments in them), and the rest after the last `;`.
pub fn split(text: &str) -> (Vec<String>, String) {
    let mut out = Vec::new();
    let mut state = State::Code;
    let mut start = 0;
    let mut code = false; // the current statement has more than space and comments
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match state {
            State::Code => match c {
                b';' => {
                    if code {
                        out.push(text[start..i].trim().to_string());
                    }
                    start = i + 1;
                    code = false;
                }
                b'\'' => (state, code) = (State::Single, true),
                b'"' => (state, code) = (State::Double, true),
                b'`' => (state, code) = (State::Backtick, true),
                b'/' if next == Some(b'/') => {
                    state = State::LineComment;
                    i += 1;
                }
                b'/' if next == Some(b'*') => {
                    state = State::BlockComment;
                    i += 1;
                }
                c if c.is_ascii_whitespace() => {}
                _ => code = true,
            },
            State::Single | State::Double => {
                let quote = if state == State::Single { b'\'' } else { b'"' };
                if c == b'\\' {
                    i += 1;
                } else if c == quote {
                    state = State::Code;
                }
            }
            State::Backtick => {
                if c == b'`' {
                    if next == Some(b'`') {
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::LineComment => {
                if c == b'\n' {
                    state = State::Code;
                }
            }
            State::BlockComment => {
                if c == b'*' && next == Some(b'/') {
                    state = State::Code;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    // An unclosed block comment is kept: the statement may follow it.
    let rest = if code || state == State::BlockComment { text[start..].trim().to_string() } else { String::new() };
    (out, rest)
}

/// Whether `text` holds nothing but space and comments.
pub fn is_blank(text: &str) -> bool {
    let (done, rest) = split(text);
    done.is_empty() && rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_semicolons_outside_quotes_and_comments() {
        let (s, rest) = split(
            "RETURN 'a;b' AS x; RETURN \"c\\\";\" AS `y;``z`; // c; c\n/* ; */ ;MATCH (n)\nRETURN n",
        );
        assert_eq!(s, ["RETURN 'a;b' AS x", "RETURN \"c\\\";\" AS `y;``z`"]);
        assert_eq!(rest, "MATCH (n)\nRETURN n");
    }

    #[test]
    fn comments_only_are_blank() {
        assert!(is_blank("  // nothing\n/* here */ ; ;"));
        assert!(!is_blank("RETURN 1"));
        assert_eq!(split("RETURN 1; // done").1, "");
    }
}

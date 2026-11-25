//! Cypher syntax highlighting: a color for each character of the text.

use crate::style::{Color, RESET};

const KEYWORDS: &[&str] = &[
    "ACCESS", "ALL", "ALTER", "AND", "AS", "ASC", "ASCENDING", "BY", "CALL", "CASE", "CONSTRAINT", "CONTAINS", "CREATE",
    "CSV", "CURRENT", "DATABASE", "DATABASES", "DEFAULT", "DELETE", "DESC", "DESCENDING", "DETACH", "DISTINCT",
    "DROP", "ELSE", "END", "ENDS", "EXISTS", "EXPIRES", "EXPLAIN", "FIELDTERMINATOR", "FOR", "FOREACH", "FROM", "GRANT",
    "HEADERS", "IF", "IN", "INDEX", "IS", "KEY", "LIMIT", "LOAD", "MATCH", "MERGE", "NODE", "NOT", "OF", "ON",
    "OPTIONAL", "OPTIONS", "OR", "ORDER", "PASSWORD", "PROFILE", "REMOVE", "REQUIRE", "RETURN", "REVOKE",
    "ROLE", "ROLES", "SET", "SHOW", "SKIP", "STARTS", "THEN", "TO", "TOKEN", "TOKENS", "UNION", "UNIQUE", "UNWIND", "USE", "USER",
    "USERS", "WHEN", "WHERE", "WITH", "XOR", "YIELD",
];

fn keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word))
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The color of each character of `text` (`None`: the default color).
pub fn colors(text: &str) -> Vec<Option<Color>> {
    let s: Vec<char> = text.chars().collect();
    let mut out = vec![None; s.len()];
    // A `:` command: its name.
    let first = s.iter().position(|c| !c.is_whitespace());
    if let Some(f) = first
        && s[f] == ':'
    {
        let end = (f..s.len()).find(|&i| s[i].is_whitespace()).unwrap_or(s.len());
        out[f..end].fill(Some(Color::Command));
        // `:param name expr` and the like: the rest is Cypher.
        let rest: String = s[end..].iter().collect();
        for (i, c) in colors(&rest).into_iter().enumerate() {
            out[end + i] = c;
        }
        return out;
    }
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let next = s.get(i + 1).copied();
        let start = i;
        let color = if c == '/' && next == Some('/') {
            while i < s.len() && s[i] != '\n' {
                i += 1;
            }
            Some(Color::Comment)
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < s.len() && !(s[i] == '*' && s.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(s.len());
            Some(Color::Comment)
        } else if c == '\'' || c == '"' {
            i += 1;
            while i < s.len() && s[i] != c {
                if s[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(s.len());
            Some(Color::String)
        } else if c == '`' {
            i += 1;
            while i < s.len() {
                if s[i] == '`' {
                    if s.get(i + 1) == Some(&'`') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(s.len());
            (start > 0 && s[start - 1] == ':').then_some(Color::Label)
        } else if c.is_ascii_digit() && !(start > 0 && ident(s[start - 1])) {
            i += 1;
            while i < s.len() {
                let d = s[i];
                let part = d.is_ascii_alphanumeric()
                    || d == '_'
                    || (d == '.' && s.get(i + 1).is_some_and(char::is_ascii_digit))
                    // An exponent's sign (not in hex, where `e` is a digit).
                    || ("+-".contains(d) && matches!(s[i - 1], 'e' | 'E') && !s[start..i].contains(&'x'));
                if !part {
                    break;
                }
                i += 1;
            }
            Some(Color::Number)
        } else if c == '$' && next.is_some_and(|n| ident(n) || n == '`') {
            i += 1;
            if s[i] == '`' {
                i += 1;
                while i < s.len() && s[i] != '`' {
                    i += 1;
                }
                i = (i + 1).min(s.len());
            } else {
                while i < s.len() && ident(s[i]) {
                    i += 1;
                }
            }
            Some(Color::Param)
        } else if ident_start(c) {
            while i < s.len() && ident(s[i]) {
                i += 1;
            }
            let word: String = s[start..i].iter().collect();
            let after = s[i..].iter().find(|c| !c.is_whitespace());
            let before_dot = start > 0 && s[start - 1] == '.';
            if start > 0 && s[start - 1] == ':' {
                Some(Color::Label)
            } else if word.eq_ignore_ascii_case("true") || word.eq_ignore_ascii_case("false") {
                Some(Color::Literal)
            } else if word.eq_ignore_ascii_case("null") {
                Some(Color::Null)
            } else if !before_dot && keyword(&word) {
                Some(Color::Keyword)
            } else if after == Some(&'(') {
                Some(Color::Function)
            } else {
                None
            }
        } else {
            i += 1;
            None
        };
        out[start..i].fill(color);
    }
    out
}

/// `chars` painted with `colors` (`colors[k]` for `chars[k]`).
pub fn paint_chars(chars: &[char], colors: &[Option<Color>]) -> String {
    let mut out = String::new();
    let mut current = None;
    for (k, &c) in chars.iter().enumerate() {
        let want = colors.get(k).copied().flatten();
        if want != current {
            if current.is_some() {
                out.push_str(RESET);
            }
            if let Some(color) = want {
                out.push_str(&color.start());
            }
            current = want;
        }
        out.push(c);
    }
    if current.is_some() {
        out.push_str(RESET);
    }
    out
}

/// `text` highlighted when `on`.
pub fn paint(on: bool, text: &str) -> String {
    if !on {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    paint_chars(&chars, &colors(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of each colored run, with its color's first letter.
    fn runs(text: &str) -> Vec<(String, Color)> {
        let chars: Vec<char> = text.chars().collect();
        let colors = colors(text);
        let mut out: Vec<(String, Color)> = Vec::new();
        let mut prev = None;
        for (c, col) in chars.iter().zip(colors) {
            match (col, prev) {
                (Some(col), Some(p)) if col == p => out.last_mut().unwrap().0.push(*c),
                (Some(col), _) => out.push((c.to_string(), col)),
                _ => {}
            }
            prev = col;
        }
        out
    }

    #[test]
    fn tokens() {
        use Color::*;
        let r = runs("MATCH (p:Person {name: 'Ann', age: 1.5e3}) WHERE p.x = $id RETURN count(p) // c");
        let want: Vec<(std::string::String, Color)> = [
            ("MATCH", Keyword),
            ("Person", Label),
            ("'Ann'", String),
            ("1.5e3", Number),
            ("WHERE", Keyword),
            ("$id", Param),
            ("RETURN", Keyword),
            ("count", Function),
            ("// c", Comment),
        ]
        .iter()
        .map(|(s, c)| (s.to_string(), *c))
        .collect();
        assert_eq!(r, want);
        // A property named like a keyword is not one; a range is two numbers.
        assert_eq!(runs("n.limit"), []);
        assert_eq!(runs("[*1..3]"), [("1".to_string(), Number), ("3".to_string(), Number)]);
        assert_eq!(runs(":use movies"), [(":use".to_string(), Command)]);
        assert_eq!(runs("x1 = null"), [("null".to_string(), Null)]);
    }

    #[test]
    fn painting() {
        assert_eq!(paint(false, "MATCH"), "MATCH");
        assert_eq!(paint(true, "RETURN 1"), "\x1b[1;34mRETURN\x1b[0m \x1b[33m1\x1b[0m");
    }
}

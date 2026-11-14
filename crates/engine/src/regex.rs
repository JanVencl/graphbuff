//! Regular expressions for Cypher's `=~`, without dependencies.
//!
//! The syntax is the common subset of Java's (which Cypher uses): literals, `.`, classes
//! `[a-z]` / `[^…]`, `\d \w \s` and their negations, escapes, groups `(…)`, `(?:…)`,
//! `(?<name>…)`, alternation `|`, quantifiers `* + ? {n} {n,} {n,m}` (lazy forms too),
//! anchors `^ $ \A \z \Z`, word boundaries `\b \B`, quoting `\Q…\E`, and the flags `i`,
//! `s`, `m` (`(?i)`, `(?i:…)`, `(?-i)`). Lookaround, backreferences and possessive
//! quantifiers are rejected.
//!
//! A pattern compiles to a small NFA program, which is simulated over all threads at
//! once (Thompson's construction): matching takes time linear in the input, whatever the
//! pattern. `=~` asks whether the *whole* string matches.

use graphbuff_core::{Error, Result};

/// A compiled regular expression.
#[derive(Debug, Clone)]
pub(crate) struct Regex {
    prog: Vec<Inst>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Assertion {
    /// `^`: start of input (or of a line, with `m`).
    LineStart { multiline: bool },
    /// `$`: end of input or before a final line break (or end of a line, with `m`).
    LineEnd { multiline: bool },
    /// `\A`
    InputStart,
    /// `\z`
    InputEnd,
    /// `\Z`: end of input, or before a final line break.
    InputEndOrNewline,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Class {
    items: Vec<ClassItem>,
    negated: bool,
    ignore_case: bool,
}

impl Class {
    fn single(c: char, ignore_case: bool) -> Class {
        Class {
            items: vec![ClassItem::Range(c, c)],
            negated: false,
            ignore_case,
        }
    }

    fn test(&self, c: char) -> bool {
        let hit = |c: char| {
            self.items.iter().any(|it| match *it {
                ClassItem::Range(a, b) => a <= c && c <= b,
                ClassItem::Digit(neg) => c.is_ascii_digit() != neg,
                ClassItem::Word(neg) => is_word(c) != neg,
                ClassItem::Space(neg) => matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r') != neg,
            })
        };
        let mut found = hit(c);
        if !found && self.ignore_case {
            found = c.to_lowercase().any(hit) || c.to_uppercase().any(hit);
        }
        found != self.negated
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Class(Class),
    /// `.`; `true` if it also matches line breaks (flag `s`).
    Any(bool),
    Assert(Assertion),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Debug, Clone)]
enum Inst {
    Class(Class),
    Any(bool),
    Assert(Assertion),
    Split(usize, usize),
    Jmp(usize),
    Match,
}

/// Largest repetition count, and program size, accepted.
const MAX_REPEAT: u32 = 1000;
const MAX_PROGRAM: usize = 100_000;

#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    ignore_case: bool,
    dot_all: bool,
    multiline: bool,
}

struct Parser<'p> {
    chars: Vec<char>,
    pos: usize,
    src: &'p str,
}

impl Parser<'_> {
    fn err<T>(&self, msg: &str) -> Result<T> {
        Err(Error::Runtime(format!("invalid regular expression `{}`: {msg}", self.src)))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Alternatives up to `)` or the end. `flags` may be changed by inline flags, which
    /// last until the end of the enclosing group.
    fn alternation(&mut self, flags: &mut Flags) -> Result<Node> {
        let mut alts = vec![self.sequence(flags)?];
        while self.eat('|') {
            alts.push(self.sequence(flags)?);
        }
        Ok(if alts.len() == 1 { alts.pop().expect("one") } else { Node::Alt(alts) })
    }

    fn sequence(&mut self, flags: &mut Flags) -> Result<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = match self.atom(flags)? {
                Some(a) => a,
                None => continue, // inline flags, or an empty `\Q\E`
            };
            items.push(self.quantified(atom)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().expect("one"),
            _ => Node::Concat(items),
        })
    }

    fn quantified(&mut self, atom: Node) -> Result<Node> {
        let (min, max) = match self.peek() {
            Some('{') => match self.counted()? {
                Some(b) => b,
                None => return Ok(atom),
            },
            Some(c @ ('*' | '+' | '?')) => {
                self.pos += 1;
                match c {
                    '*' => (0, None),
                    '+' => (1, None),
                    _ => (0, Some(1)),
                }
            }
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Assert(_)) {
            return self.err("a quantifier cannot follow an anchor");
        }
        if self.eat('+') {
            return self.err("possessive quantifiers are not supported");
        }
        self.eat('?'); // lazy: the same for a whole-string match
        if self.peek().is_some_and(|c| matches!(c, '*' | '+' | '?')) {
            return self.err("a quantifier cannot follow another one");
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
        })
    }

    /// `{n}`, `{n,}`, `{n,m}` at the current position (consumed), or `None` if the brace
    /// does not start one (it is then a literal `{`).
    fn counted(&mut self) -> Result<Option<(u32, Option<u32>)>> {
        let start = self.pos;
        self.pos += 1;
        let num = |p: &mut Self| -> Option<u32> {
            let s = p.pos;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.pos += 1;
            }
            p.chars[s..p.pos].iter().collect::<String>().parse().ok()
        };
        let Some(min) = num(self) else {
            self.pos = start;
            return Ok(None);
        };
        let max = if self.eat(',') { num(self) } else { Some(min) };
        if !self.eat('}') {
            self.pos = start;
            return Ok(None);
        }
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
            return self.err("repetition count is too large");
        }
        if max.is_some_and(|m| m < min) {
            return self.err("repetition range is out of order");
        }
        Ok(Some((min, max)))
    }

    fn atom(&mut self, flags: &mut Flags) -> Result<Option<Node>> {
        let c = self.peek().expect("caller checked");
        self.pos += 1;
        Ok(Some(match c {
            '.' => Node::Any(flags.dot_all),
            '^' => Node::Assert(Assertion::LineStart {
                multiline: flags.multiline,
            }),
            '$' => Node::Assert(Assertion::LineEnd {
                multiline: flags.multiline,
            }),
            '[' => Node::Class(self.class(flags)?),
            '(' => return self.group(flags),
            '\\' => return self.escape(flags),
            '*' | '+' | '?' => return self.err("a quantifier needs something to repeat"),
            '{' => {
                self.pos -= 1;
                if self.counted()?.is_some() {
                    return self.err("a quantifier needs something to repeat");
                }
                self.pos += 1;
                Node::Class(Class::single('{', flags.ignore_case))
            }
            c => Node::Class(Class::single(c, flags.ignore_case)),
        }))
    }

    /// After `(`.
    fn group(&mut self, flags: &mut Flags) -> Result<Option<Node>> {
        let mut inner = *flags;
        if self.eat('?') {
            match self.peek() {
                Some(':') => self.pos += 1,
                Some('<') if !matches!(self.chars.get(self.pos + 1), Some('=' | '!')) => {
                    self.pos += 1;
                    while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                        self.pos += 1;
                    }
                    if !self.eat('>') {
                        return self.err("unterminated group name");
                    }
                }
                Some('=' | '!' | '<' | '>') => {
                    return self.err("lookaround and atomic groups are not supported");
                }
                _ => {
                    // Flags: `(?i)` (rest of the enclosing group) or `(?i:…)`.
                    let mut on = true;
                    let mut f = *flags;
                    loop {
                        match self.peek() {
                            Some('-') => on = false,
                            Some('i') => f.ignore_case = on,
                            Some('s') => f.dot_all = on,
                            Some('m') => f.multiline = on,
                            Some('u' | 'U' | 'd') => {}
                            Some(')') => {
                                self.pos += 1;
                                *flags = f;
                                return Ok(None);
                            }
                            Some(':') => {
                                self.pos += 1;
                                inner = f;
                                break;
                            }
                            _ => return self.err("unknown group flag"),
                        }
                        self.pos += 1;
                    }
                }
            }
        }
        let node = self.alternation(&mut inner)?;
        if !self.eat(')') {
            return self.err("missing ')'");
        }
        Ok(Some(node))
    }

    /// After `\` outside a class.
    fn escape(&mut self, flags: &Flags) -> Result<Option<Node>> {
        let Some(c) = self.peek() else {
            return self.err("trailing backslash");
        };
        self.pos += 1;
        let assert = |a| Ok(Some(Node::Assert(a)));
        match c {
            'A' => return assert(Assertion::InputStart),
            'z' => return assert(Assertion::InputEnd),
            'Z' => return assert(Assertion::InputEndOrNewline),
            'b' => return assert(Assertion::WordBoundary),
            'B' => return assert(Assertion::NotWordBoundary),
            'Q' => {
                let mut lits = Vec::new();
                while self.pos < self.chars.len() {
                    if self.chars[self.pos] == '\\' && self.chars.get(self.pos + 1) == Some(&'E') {
                        self.pos += 2;
                        break;
                    }
                    lits.push(Node::Class(Class::single(self.chars[self.pos], flags.ignore_case)));
                    self.pos += 1;
                }
                return Ok(match lits.len() {
                    0 => None,
                    1 => lits.pop(),
                    _ => Some(Node::Concat(lits)),
                });
            }
            '1'..='9' | 'k' => return self.err("backreferences are not supported"),
            _ => {}
        }
        self.pos -= 1;
        let item = self.class_escape()?;
        Ok(Some(Node::Class(Class {
            items: vec![item],
            negated: false,
            ignore_case: flags.ignore_case,
        })))
    }

    /// An escape that stands for characters (after `\`, at the escaped character).
    fn class_escape(&mut self) -> Result<ClassItem> {
        let Some(c) = self.peek() else {
            return self.err("trailing backslash");
        };
        self.pos += 1;
        let ch = |c| Ok(ClassItem::Range(c, c));
        match c {
            'd' => Ok(ClassItem::Digit(false)),
            'D' => Ok(ClassItem::Digit(true)),
            'w' => Ok(ClassItem::Word(false)),
            'W' => Ok(ClassItem::Word(true)),
            's' => Ok(ClassItem::Space(false)),
            'S' => Ok(ClassItem::Space(true)),
            't' => ch('\t'),
            'n' => ch('\n'),
            'r' => ch('\r'),
            'f' => ch('\u{c}'),
            'e' => ch('\u{1b}'),
            'a' => ch('\u{7}'),
            '0' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 && self.peek().is_some_and(|c| ('0'..='7').contains(&c)) {
                    v = v * 8 + self.peek().and_then(|c| c.to_digit(8)).expect("octal digit");
                    self.pos += 1;
                    n += 1;
                }
                if n == 0 {
                    return self.err("octal escape needs digits");
                }
                ch(char::from_u32(v).expect("small octal"))
            }
            'x' | 'u' => {
                let digits: String = if c == 'x' && self.eat('{') {
                    let s = self.pos;
                    while self.peek().is_some_and(|c| c != '}') {
                        self.pos += 1;
                    }
                    let d = self.chars[s..self.pos].iter().collect();
                    if !self.eat('}') {
                        return self.err("unterminated \\x{…}");
                    }
                    d
                } else {
                    let n = if c == 'x' { 2 } else { 4 };
                    let end = (self.pos + n).min(self.chars.len());
                    let d = self.chars[self.pos..end].iter().collect();
                    self.pos = end;
                    d
                };
                match u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32) {
                    Some(c) => ch(c),
                    None => self.err("invalid hexadecimal escape"),
                }
            }
            c if c.is_ascii_alphanumeric() => self.err(&format!("unsupported escape \\{c}")),
            c => ch(c),
        }
    }

    /// After `[`.
    fn class(&mut self, flags: &Flags) -> Result<Class> {
        let negated = self.eat('^');
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                return self.err("missing ']'");
            };
            if c == ']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            if c == '[' || (c == '&' && self.chars.get(self.pos + 1) == Some(&'&')) {
                return self.err("nested classes and class intersections are not supported");
            }
            let lo = self.class_atom()?;
            // A range `a-z`, unless the `-` is last.
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&c| c != ']') {
                self.pos += 1;
                let hi = self.class_atom()?;
                match (lo, hi) {
                    (ClassItem::Range(a, _), ClassItem::Range(b, _)) if a <= b => items.push(ClassItem::Range(a, b)),
                    (ClassItem::Range(..), ClassItem::Range(..)) => return self.err("class range is out of order"),
                    _ => return self.err("a class range needs single characters"),
                }
            } else {
                items.push(lo);
            }
        }
        Ok(Class {
            items,
            negated,
            ignore_case: flags.ignore_case,
        })
    }

    fn class_atom(&mut self) -> Result<ClassItem> {
        let c = self.peek().expect("caller checked");
        self.pos += 1;
        if c == '\\' {
            return self.class_escape();
        }
        Ok(ClassItem::Range(c, c))
    }
}

impl Regex {
    pub fn new(src: &str) -> Result<Regex> {
        let mut p = Parser {
            chars: src.chars().collect(),
            pos: 0,
            src,
        };
        let mut flags = Flags::default();
        let node = p.alternation(&mut flags)?;
        if p.pos < p.chars.len() {
            return p.err("unbalanced ')'");
        }
        let mut prog = Vec::new();
        compile(&node, &mut prog);
        if prog.len() > MAX_PROGRAM {
            return p.err("pattern is too large");
        }
        prog.push(Inst::Match);
        Ok(Regex { prog })
    }

    /// Whether the whole of `s` matches.
    pub fn is_full_match(&self, s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        let n = self.prog.len();
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        let mut seen = vec![usize::MAX; n];
        self.add(&mut current, &mut seen, 0, &chars, 0);
        for (i, &c) in chars.iter().enumerate() {
            if current.is_empty() {
                return false;
            }
            next.clear();
            for &pc in &current {
                let ok = match &self.prog[pc] {
                    Inst::Class(cls) => cls.test(c),
                    Inst::Any(all) => *all || !matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}'),
                    _ => false,
                };
                if ok {
                    self.add(&mut next, &mut seen, pc + 1, &chars, i + 1);
                }
            }
            std::mem::swap(&mut current, &mut next);
        }
        current.iter().any(|&pc| matches!(self.prog[pc], Inst::Match))
    }

    /// Adds the thread at `pc` and everything reachable from it without reading a
    /// character, at position `pos` (`seen` marks what was added at this position).
    fn add(&self, list: &mut Vec<usize>, seen: &mut [usize], pc: usize, chars: &[char], pos: usize) {
        let mut stack = vec![pc];
        while let Some(pc) = stack.pop() {
            if seen[pc] == pos {
                continue;
            }
            seen[pc] = pos;
            match &self.prog[pc] {
                Inst::Jmp(t) => stack.push(*t),
                Inst::Split(a, b) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Inst::Assert(a) => {
                    if holds(*a, chars, pos) {
                        stack.push(pc + 1);
                    }
                }
                _ => list.push(pc),
            }
        }
    }
}

fn holds(a: Assertion, chars: &[char], pos: usize) -> bool {
    let prev = pos.checked_sub(1).map(|i| chars[i]);
    let next = chars.get(pos).copied();
    let at_end = pos == chars.len();
    let before_final_newline = (pos + 1 == chars.len() && next == Some('\n'))
        || (pos + 2 == chars.len() && next == Some('\r') && chars[pos + 1] == '\n');
    match a {
        Assertion::InputStart => pos == 0,
        Assertion::InputEnd => at_end,
        Assertion::InputEndOrNewline => at_end || before_final_newline,
        Assertion::LineStart { multiline } => pos == 0 || (multiline && prev == Some('\n') && !at_end),
        Assertion::LineEnd { multiline } => {
            at_end || before_final_newline || (multiline && matches!(next, Some('\n' | '\r')))
        }
        Assertion::WordBoundary | Assertion::NotWordBoundary => {
            let boundary = prev.is_some_and(is_word) != next.is_some_and(is_word);
            boundary == (a == Assertion::WordBoundary)
        }
    }
}

fn compile(node: &Node, prog: &mut Vec<Inst>) {
    if prog.len() > MAX_PROGRAM {
        return; // reported by the caller
    }
    match node {
        Node::Empty => {}
        Node::Class(c) => prog.push(Inst::Class(c.clone())),
        Node::Any(all) => prog.push(Inst::Any(*all)),
        Node::Assert(a) => prog.push(Inst::Assert(*a)),
        Node::Concat(items) => items.iter().for_each(|n| compile(n, prog)),
        Node::Alt(alts) => {
            let mut jumps = Vec::new();
            for (i, alt) in alts.iter().enumerate() {
                if i + 1 < alts.len() {
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    compile(alt, prog);
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let after = prog.len();
                    prog[split] = Inst::Split(split + 1, after);
                } else {
                    compile(alt, prog);
                }
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Repeat { node, min, max } => {
            for _ in 0..*min {
                compile(node, prog);
            }
            match max {
                None => {
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    compile(node, prog);
                    prog.push(Inst::Jmp(split));
                    let end = prog.len();
                    prog[split] = Inst::Split(split + 1, end);
                }
                Some(max) => {
                    let mut splits = Vec::new();
                    for _ in *min..*max {
                        splits.push(prog.len());
                        prog.push(Inst::Split(prog.len() + 1, 0));
                        compile(node, prog);
                    }
                    let end = prog.len();
                    for s in splits {
                        prog[s] = Inst::Split(s + 1, end);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Regex;

    fn m(re: &str, s: &str) -> bool {
        Regex::new(re).unwrap_or_else(|e| panic!("{re}: {e}")).is_full_match(s)
    }

    #[test]
    fn whole_string_matches() {
        assert!(m("abc", "abc"));
        assert!(!m("abc", "abcd"));
        assert!(!m("b", "abc"));
        assert!(m(".*b.*", "abc"));
        assert!(m("", ""));
        assert!(!m("", "a"));
        assert!(m("a|ab|abc", "abc"));
        assert!(m("(a|b)*c", "ababbc"));
        assert!(m("a+b?c*", "aaac"));
        assert!(!m("a+", ""));
        assert!(m("a{2,3}", "aaa"));
        assert!(!m("a{2,3}", "aaaa"));
        assert!(m("a{2}", "aa"));
        assert!(m("a{2,}", "aaaaa"));
        assert!(m("a{,2}", "a{,2}"), "not a quantifier: literal braces");
        assert!(m("(?:ab)+", "ababab"));
        assert!(m("(?<x>ab)+?", "abab"));
        assert!(m("(a*)*b", "aaab"));
        assert!(m("(a|)+", "aa"));
    }

    #[test]
    fn classes_and_escapes() {
        assert!(m("[a-c]+", "abcab"));
        assert!(!m("[a-c]+", "abd"));
        assert!(m("[^a-c]+", "xyz"));
        assert!(m("[-a]+", "-a-"));
        assert!(m("[a-]+", "-a-"));
        assert!(m("[]a]+", "]a"));
        assert!(m("\\d{3}-\\d{4}", "555-1234"));
        assert!(m("\\w+@\\w+\\.com", "ann_1@acme.com"));
        assert!(!m("\\w+", "a b"));
        assert!(m("\\s*\\S+\\s*", "  x  "));
        assert!(m("[\\d.]+", "3.14"));
        assert!(m("a\\.b", "a.b"));
        assert!(!m("a\\.b", "axb"));
        assert!(m("\\Q.*\\E", ".*"));
        assert!(!m("\\Q.*\\E", "ab"));
        assert!(m("\\x41\\u0042\\t", "AB\t"));
        assert!(m("[\\D]", "x"));
        assert!(!m(".", "\n"));
        assert!(m("(?s).", "\n"));
    }

    #[test]
    fn flags_and_anchors() {
        assert!(m("(?i)ann.*", "ANNA"));
        assert!(m("(?i)[a-c]+", "ABC"));
        assert!(!m("ann.*", "ANNA"));
        assert!(m("a(?i:b)c", "aBc"));
        assert!(!m("a(?i:b)c", "aBC"));
        assert!(m("(?i)a(?-i)b", "Ab"));
        assert!(!m("(?i)a(?-i)b", "AB"));
        assert!(m("^abc$", "abc"));
        assert!(m("\\Aab\\z", "ab"));
        assert!(m("a\\bb|a b", "a b"));
        assert!(m("\\bab\\b", "ab"));
        assert!(m("(?m)a$\\n^b", "a\nb"));
        assert!(!m("a$\\n^b", "a\nb"));
        assert!(m("ab$\\n", "ab\n"));
    }

    #[test]
    fn rejects_what_it_cannot_do() {
        for re in ["(", ")", "[a", "a**", "*a", "(?=a)", "(?!a)", "(?<=a)", "(a)\\1", "a++", "[a&&b]", "a{5,2}", "\\", "\\y", "a{100000}"] {
            assert!(Regex::new(re).is_err(), "{re}");
        }
    }

    #[test]
    fn linear_time_on_pathological_patterns() {
        let s = "a".repeat(5000);
        assert!(!m("(a*)*b", &s));
        assert!(m("(a|aa)*", &s));
    }
}

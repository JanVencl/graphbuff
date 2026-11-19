//! The line editor of the interactive prompt: Cypher highlighted as it is typed, cursor
//! keys, and a history (Up/Down) kept in `~/.graphbuff_history`.
//!
//! The terminal is in raw mode only while a line is read; statements run with it as it
//! was, so Ctrl-C then stops the program as usual. A line longer than the terminal is
//! wide scrolls sideways.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::highlight;
use crate::style::{self, Color, paint, term};

/// Lines kept in the history.
const HISTORY: usize = 1000;

/// What reading a line gave.
#[derive(Debug, PartialEq)]
pub enum Input {
    Line(String),
    /// Ctrl-C: drop what was typed.
    Interrupted,
    /// Ctrl-D on an empty line, or the input ended.
    Eof,
}

#[derive(Debug, PartialEq)]
enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    Up,
    Down,
    Tab,
    Interrupt,
    CtrlD,
    KillToStart,
    KillToEnd,
    KillWord,
    Clear,
    Ignore,
}

pub struct Editor {
    color: bool,
    history: Vec<String>,
    file: Option<PathBuf>,
}

impl Editor {
    /// `color`: highlight what is typed.
    pub fn new(color: bool) -> Editor {
        let file = history_file();
        let mut history: Vec<String> = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        let excess = history.len().saturating_sub(HISTORY);
        history.drain(..excess);
        Editor { color, history, file }
    }

    /// Reads a line after `prompt`. `context` is the statement begun on earlier lines, so
    /// that a string or comment spanning lines is highlighted right.
    pub fn read_line(&mut self, prompt: &str, context: &str) -> io::Result<Input> {
        let Some(raw) = term::Raw::enter() else {
            return cooked(prompt);
        };
        let result = self.edit(prompt, context);
        drop(raw);
        if let Ok(Input::Line(line)) = &result {
            self.remember(line);
        }
        result
    }

    fn edit(&mut self, prompt: &str, context: &str) -> io::Result<Input> {
        let mut s = Line {
            prompt,
            prompt_width: style::width(prompt),
            context,
            color: self.color,
            width: term::width().unwrap_or(80),
            buf: Vec::new(),
            pos: 0,
            scroll: 0,
        };
        let mut out = io::stdout();
        let mut input = io::stdin().lock();
        // Up/Down: where in the history, and the line being typed before.
        let mut index = self.history.len();
        let mut draft: Vec<char> = Vec::new();
        s.render(&mut out, false)?;
        loop {
            let Some(key) = read_key(&mut input)? else {
                s.finish(&mut out, "")?;
                return Ok(Input::Eof);
            };
            match key {
                Key::Char(c) => s.insert(&[c]),
                Key::Tab => s.insert(&[' ', ' ']),
                Key::Enter => {
                    s.finish(&mut out, "")?;
                    return Ok(Input::Line(s.buf.iter().collect()));
                }
                Key::Interrupt => {
                    s.finish(&mut out, &paint(self.color, Color::Dim, "^C"))?;
                    return Ok(Input::Interrupted);
                }
                Key::CtrlD if s.buf.is_empty() => {
                    s.finish(&mut out, "")?;
                    return Ok(Input::Eof);
                }
                Key::CtrlD | Key::Delete => {
                    if s.pos < s.buf.len() {
                        s.buf.remove(s.pos);
                    }
                }
                Key::Backspace => {
                    if s.pos > 0 {
                        s.pos -= 1;
                        s.buf.remove(s.pos);
                    }
                }
                Key::Left => s.pos = s.pos.saturating_sub(1),
                Key::Right => s.pos = (s.pos + 1).min(s.buf.len()),
                Key::WordLeft => s.pos = s.word_start(),
                Key::WordRight => s.pos = s.word_end(),
                Key::Home => s.pos = 0,
                Key::End => s.pos = s.buf.len(),
                Key::KillToStart => {
                    s.buf.drain(..s.pos);
                    s.pos = 0;
                }
                Key::KillToEnd => s.buf.truncate(s.pos),
                Key::KillWord => {
                    let start = s.word_start();
                    s.buf.drain(start..s.pos);
                    s.pos = start;
                }
                Key::Up => {
                    if index > 0 {
                        if index == self.history.len() {
                            draft = s.buf.clone();
                        }
                        index -= 1;
                        s.set(self.history[index].chars().collect());
                    }
                }
                Key::Down => {
                    if index < self.history.len() {
                        index += 1;
                        let next = match self.history.get(index) {
                            Some(h) => h.chars().collect(),
                            None => draft.clone(),
                        };
                        s.set(next);
                    }
                }
                Key::Clear => {
                    out.write_all(b"\x1b[2J\x1b[H")?;
                }
                Key::Ignore => continue,
            }
            s.render(&mut out, false)?;
        }
    }

    /// Adds `line` to the history (and its file), unless it is blank, repeats the last
    /// one, or may hold a password.
    fn remember(&mut self, line: &str) {
        let line = line.trim_end();
        if line.trim().is_empty()
            || self.history.last().is_some_and(|l| l == line)
            || line.to_ascii_lowercase().contains("password")
        {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        if let Some(f) = &self.file
            && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(f)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// `GRAPHBUFF_HISTORY` (empty: none), else `.graphbuff_history` in the home folder.
fn history_file() -> Option<PathBuf> {
    if let Some(f) = std::env::var_os("GRAPHBUFF_HISTORY") {
        return (!f.is_empty()).then(|| PathBuf::from(f));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".graphbuff_history"))
}

/// Without raw mode: the prompt, and a line as the terminal reads it.
fn cooked(prompt: &str) -> io::Result<Input> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        println!();
        return Ok(Input::Eof);
    }
    Ok(Input::Line(line.trim_end_matches(['\r', '\n']).to_string()))
}

/// The line being edited.
struct Line<'a> {
    prompt: &'a str,
    prompt_width: usize,
    context: &'a str,
    color: bool,
    width: usize,
    buf: Vec<char>,
    pos: usize,
    /// The first character shown, when the line is wider than the terminal.
    scroll: usize,
}

impl Line<'_> {
    fn insert(&mut self, chars: &[char]) {
        for (k, c) in chars.iter().enumerate() {
            self.buf.insert(self.pos + k, *c);
        }
        self.pos += chars.len();
    }

    fn set(&mut self, buf: Vec<char>) {
        self.pos = buf.len();
        self.buf = buf;
    }

    fn word_start(&self) -> usize {
        let mut i = self.pos;
        while i > 0 && !self.buf[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.buf[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_end(&self) -> usize {
        let mut i = self.pos;
        while i < self.buf.len() && !self.buf[i].is_alphanumeric() {
            i += 1;
        }
        while i < self.buf.len() && self.buf[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// The colors of the line's characters.
    fn colors(&self) -> Vec<Option<Color>> {
        if !self.color {
            return Vec::new();
        }
        let text: String = self.context.chars().chain(self.buf.iter().copied()).collect();
        let mut colors = highlight::colors(&text);
        colors.drain(..colors.len() - self.buf.len());
        colors
    }

    /// Redraws the line; `whole`: all of it (wrapping), for leaving it.
    fn render(&mut self, out: &mut impl Write, whole: bool) -> io::Result<()> {
        let colors = self.colors();
        let (from, to) = if whole {
            (0, self.buf.len())
        } else {
            let room = self.width.saturating_sub(self.prompt_width + 1).max(1);
            if self.pos < self.scroll {
                self.scroll = self.pos;
            } else if self.pos >= self.scroll + room {
                self.scroll = self.pos + 1 - room;
            }
            (self.scroll, self.buf.len().min(self.scroll + room))
        };
        let shown = if self.color {
            highlight::paint_chars(&self.buf[from..to], &colors[from..to])
        } else {
            self.buf[from..to].iter().collect()
        };
        let mut s = format!("\r{}{shown}\x1b[K", self.prompt);
        if !whole {
            let column = self.prompt_width + self.pos - from;
            s.push('\r');
            if column > 0 {
                s.push_str(&format!("\x1b[{column}C"));
            }
        }
        out.write_all(s.as_bytes())?;
        out.flush()
    }

    /// Shows the whole line, then `tail`, and goes to the next line.
    fn finish(&mut self, out: &mut impl Write, tail: &str) -> io::Result<()> {
        self.render(out, true)?;
        write!(out, "{tail}\r\n")?;
        out.flush()
    }
}

fn byte(r: &mut impl io::Read) -> io::Result<Option<u8>> {
    let mut b = [0u8];
    loop {
        match r.read(&mut b) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(b[0])),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// The next key; `None` when the input ends.
fn read_key(r: &mut impl io::Read) -> io::Result<Option<Key>> {
    let Some(b) = byte(r)? else { return Ok(None) };
    let key = match b {
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        b'\t' => Key::Tab,
        0x01 => Key::Home,
        0x02 => Key::Left,
        0x03 => Key::Interrupt,
        0x04 => Key::CtrlD,
        0x05 => Key::End,
        0x06 => Key::Right,
        0x0b => Key::KillToEnd,
        0x0c => Key::Clear,
        0x0e => Key::Down,
        0x10 => Key::Up,
        0x15 => Key::KillToStart,
        0x17 => Key::KillWord,
        0x1b => escape(r)?,
        b if b < 0x20 => Key::Ignore,
        b if b < 0x80 => Key::Char(b as char),
        b => {
            // UTF-8: the lead byte says how many follow.
            let n = match b {
                0xc0..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf7 => 3,
                _ => return Ok(Some(Key::Ignore)),
            };
            let mut bytes = vec![b];
            for _ in 0..n {
                match byte(r)? {
                    Some(c) => bytes.push(c),
                    None => return Ok(None),
                }
            }
            match std::str::from_utf8(&bytes).ok().and_then(|s| s.chars().next()) {
                Some(c) => Key::Char(c),
                None => Key::Ignore,
            }
        }
    };
    Ok(Some(key))
}

/// The key of an escape sequence (after ESC).
fn escape(r: &mut impl io::Read) -> io::Result<Key> {
    let Some(b) = byte(r)? else { return Ok(Key::Ignore) };
    Ok(match b {
        b'[' => {
            let mut params = String::new();
            let last = loop {
                match byte(r)? {
                    Some(c) if (0x40..=0x7e).contains(&c) => break c,
                    Some(c) => params.push(c as char),
                    None => return Ok(Key::Ignore),
                }
            };
            let modified = params.ends_with(";5") || params.ends_with(";3");
            match (last, params.as_str()) {
                (b'A', _) => Key::Up,
                (b'B', _) => Key::Down,
                (b'C', _) if modified => Key::WordRight,
                (b'D', _) if modified => Key::WordLeft,
                (b'C', _) => Key::Right,
                (b'D', _) => Key::Left,
                (b'H', _) => Key::Home,
                (b'F', _) => Key::End,
                (b'~', "1" | "7") => Key::Home,
                (b'~', "4" | "8") => Key::End,
                (b'~', "3") => Key::Delete,
                _ => Key::Ignore,
            }
        }
        b'O' => match byte(r)? {
            Some(b'A') => Key::Up,
            Some(b'B') => Key::Down,
            Some(b'C') => Key::Right,
            Some(b'D') => Key::Left,
            Some(b'H') => Key::Home,
            Some(b'F') => Key::End,
            _ => Key::Ignore,
        },
        b'b' => Key::WordLeft,
        b'f' => Key::WordRight,
        0x7f => Key::KillWord,
        _ => Key::Ignore,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut r = bytes;
        let mut out = Vec::new();
        while let Some(k) = read_key(&mut r).unwrap() {
            out.push(k);
        }
        out
    }

    #[test]
    fn decodes_keys() {
        assert_eq!(
            keys("a\u{e4}\x1b[A\x1b[1;5D\x1b[3~\x1bOH\x7f\r\x03".as_bytes()),
            [
                Key::Char('a'),
                Key::Char('ä'),
                Key::Up,
                Key::WordLeft,
                Key::Delete,
                Key::Home,
                Key::Backspace,
                Key::Enter,
                Key::Interrupt
            ]
        );
    }

    fn line(text: &str, width: usize) -> Line<'static> {
        let buf: Vec<char> = text.chars().collect();
        Line {
            prompt: "db> ",
            prompt_width: 4,
            context: "",
            color: false,
            width,
            pos: buf.len(),
            buf,
            scroll: 0,
        }
    }

    #[test]
    fn renders_and_scrolls() {
        let mut l = line("RETURN 1", 80);
        let mut out = Vec::new();
        l.render(&mut out, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\rdb> RETURN 1\x1b[K\r\x1b[12C");

        // 10 columns: 5 for the text after the prompt and the cursor's cell.
        let mut l = line("abcdefghij", 10);
        let mut out = Vec::new();
        l.render(&mut out, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\rdb> ghij\x1b[K\r\x1b[8C");
        l.pos = 0;
        let mut out = Vec::new();
        l.render(&mut out, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\rdb> abcde\x1b[K\r\x1b[4C");
    }

    #[test]
    fn words() {
        let mut l = line("MATCH (n:Person)", 80);
        assert_eq!(l.word_start(), 9);
        l.pos = 0;
        assert_eq!(l.word_end(), 5);
    }
}

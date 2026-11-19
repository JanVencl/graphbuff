//! Terminal colors (ANSI escape codes): on for a terminal unless `NO_COLOR` is set,
//! `TERM` is `dumb` or `--color never` says so.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static OUT: AtomicBool = AtomicBool::new(false);
static ERR: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum When {
    Auto,
    Always,
    Never,
}

impl When {
    pub fn parse(s: &str) -> Result<When, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(When::Auto),
            "always" | "on" | "yes" => Ok(When::Always),
            "never" | "off" | "no" => Ok(When::Never),
            _ => Err(format!("unknown color setting `{s}`; use auto, always or never")),
        }
    }
}

/// Decides whether standard output and standard error get colors.
pub fn init(when: When) {
    let auto = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").map_or(true, |t| t != "dumb");
    let want = |terminal: bool| match when {
        When::Always => true,
        When::Never => false,
        When::Auto => auto && terminal,
    };
    let out = want(std::io::stdout().is_terminal()) && term::enable_ansi(term::Output::Stdout);
    let err = want(std::io::stderr().is_terminal()) && term::enable_ansi(term::Output::Stderr);
    OUT.store(out, Ordering::Relaxed);
    ERR.store(err, Ordering::Relaxed);
}

/// Colors on standard output.
pub fn out() -> bool {
    OUT.load(Ordering::Relaxed)
}

/// Colors on standard error.
pub fn err() -> bool {
    ERR.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    Keyword,
    Function,
    String,
    Number,
    /// `true`, `false`.
    Literal,
    Null,
    Param,
    Comment,
    /// Labels and relationship types.
    Label,
    /// A `:` command.
    Command,
    Header,
    Border,
    Dim,
    Error,
    Good,
    Warn,
}

impl Color {
    fn code(self) -> &'static str {
        match self {
            Color::Keyword => "1;34",
            Color::Function => "36",
            Color::String => "32",
            Color::Number => "33",
            Color::Literal => "35",
            Color::Null | Color::Comment | Color::Border | Color::Dim => "90",
            Color::Param => "1;36",
            Color::Label => "35",
            Color::Command => "1;33",
            Color::Header => "1",
            Color::Error => "1;31",
            Color::Good => "32",
            Color::Warn => "33",
        }
    }

    pub fn start(self) -> String {
        format!("\x1b[{}m", self.code())
    }
}

pub const RESET: &str = "\x1b[0m";

/// `text` in `color` when `on`.
pub fn paint(on: bool, color: Color, text: &str) -> String {
    if on && !text.is_empty() { format!("{}{text}{RESET}", color.start()) } else { text.to_string() }
}

/// The width of `s` on the terminal: its characters, without escape sequences.
pub fn width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ parameters final-byte
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

/// Console modes: ANSI output on Windows, raw input for the line editor, the width.
pub mod term {
    #[derive(Clone, Copy)]
    pub enum Output {
        Stdout,
        Stderr,
    }

    #[cfg(windows)]
    mod win {
        use std::ffi::c_void;

        pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
        pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
        pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;

        #[repr(C)]
        #[derive(Default)]
        pub struct Coord {
            pub x: i16,
            pub y: i16,
        }

        #[repr(C)]
        #[derive(Default)]
        pub struct SmallRect {
            pub left: i16,
            pub top: i16,
            pub right: i16,
            pub bottom: i16,
        }

        #[repr(C)]
        #[derive(Default)]
        pub struct ScreenBufferInfo {
            pub size: Coord,
            pub cursor: Coord,
            pub attributes: u16,
            pub window: SmallRect,
            pub max_window: Coord,
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub fn GetStdHandle(which: u32) -> *mut c_void;
            pub fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
            pub fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
            pub fn GetConsoleScreenBufferInfo(console: *mut c_void, info: *mut ScreenBufferInfo) -> i32;
        }

        pub fn mode(which: u32) -> Option<(*mut c_void, u32)> {
            unsafe {
                let h = GetStdHandle(which);
                let mut mode = 0;
                if h.is_null() || GetConsoleMode(h, &mut mode) == 0 {
                    return None;
                }
                Some((h, mode))
            }
        }
    }

    /// Makes the console understand ANSI codes; false if it cannot.
    #[cfg(windows)]
    pub fn enable_ansi(which: Output) -> bool {
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        let handle = match which {
            Output::Stdout => win::STD_OUTPUT_HANDLE,
            Output::Stderr => win::STD_ERROR_HANDLE,
        };
        match win::mode(handle) {
            Some((h, mode)) => {
                mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                    || unsafe { win::SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0 }
            }
            // Not a console (a pipe, or a terminal emulator's pty): codes pass through.
            None => true,
        }
    }

    #[cfg(not(windows))]
    pub fn enable_ansi(_: Output) -> bool {
        true
    }

    /// The terminal's width in columns.
    #[cfg(windows)]
    pub fn width() -> Option<usize> {
        let (h, _) = win::mode(win::STD_OUTPUT_HANDLE)?;
        let mut info = win::ScreenBufferInfo::default();
        if unsafe { win::GetConsoleScreenBufferInfo(h, &mut info) } == 0 {
            return None;
        }
        usize::try_from(info.window.right - info.window.left + 1).ok().filter(|&w| w > 0)
    }

    #[cfg(unix)]
    pub fn width() -> Option<usize> {
        let size = stty(&["size"])?;
        size.split_whitespace().nth(1)?.parse().ok().filter(|&w: &usize| w > 0)
    }

    #[cfg(not(any(unix, windows)))]
    pub fn width() -> Option<usize> {
        None
    }

    /// Runs `stty` on the terminal and returns what it prints.
    #[cfg(unix)]
    fn stty(args: &[&str]) -> Option<String> {
        use std::process::{Command, Stdio};
        let tty = std::fs::File::open("/dev/tty").ok()?;
        let out = Command::new("stty").args(args).stdin(tty).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Raw keyboard input while it lives: no echo, no line buffering, Ctrl-C as a key.
    pub struct Raw {
        #[cfg(unix)]
        saved: String,
        #[cfg(windows)]
        saved: (*mut std::ffi::c_void, u32),
    }

    impl Raw {
        #[cfg(unix)]
        pub fn enter() -> Option<Raw> {
            let saved = stty(&["-g"])?;
            stty(&["raw", "-echo"])?;
            Some(Raw { saved })
        }

        #[cfg(windows)]
        pub fn enter() -> Option<Raw> {
            const PROCESSED_INPUT: u32 = 0x0001;
            const LINE_INPUT: u32 = 0x0002;
            const ECHO_INPUT: u32 = 0x0004;
            const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
            let (h, mode) = win::mode(win::STD_INPUT_HANDLE)?;
            let raw = (mode & !(PROCESSED_INPUT | LINE_INPUT | ECHO_INPUT)) | VIRTUAL_TERMINAL_INPUT;
            (unsafe { win::SetConsoleMode(h, raw) } != 0).then_some(Raw { saved: (h, mode) })
        }

        #[cfg(not(any(unix, windows)))]
        pub fn enter() -> Option<Raw> {
            None
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            #[cfg(unix)]
            stty(&[self.saved.as_str()]);
            #[cfg(windows)]
            unsafe {
                win::SetConsoleMode(self.saved.0, self.saved.1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_skips_escapes() {
        assert_eq!(width(&paint(true, Color::Keyword, "MATCH")), 5);
        assert_eq!(width("ä\x1b[1;31mb\x1b[0m"), 2);
    }
}

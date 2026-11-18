//! Reading a new password for `graphbuff-server users` and the first administrator: on a
//! terminal without echo and twice (the two must match); otherwise the first line of
//! standard input.

use std::io::{self, BufRead, IsTerminal, Write};

/// Whether someone can be asked: standard input and standard error are a terminal.
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

/// Asks for a line, shown as it is typed.
pub fn line(label: &str) -> io::Result<String> {
    eprint!("{label}");
    io::stderr().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "standard input ended"));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Reads a new password: asked twice without echo when standard input is a terminal,
/// else the first line of standard input.
pub fn new_password() -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return read_line();
    }
    let first = ask("password: ")?;
    let again = ask("again: ")?;
    if first != again {
        return Err(io::Error::other("the passwords do not match"));
    }
    Ok(first)
}

fn ask(label: &str) -> io::Result<String> {
    eprint!("{label}");
    io::stderr().flush()?;
    let line = {
        let _quiet = echo::Off::new();
        read_line()
    };
    eprintln!();
    line
}

fn read_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Turns terminal echo off while it lives.
#[cfg(unix)]
mod echo {
    use std::process::{Command, Stdio};

    pub struct Off(bool);

    fn stty(arg: &str) -> bool {
        // stty acts on the terminal that is its standard input.
        let Ok(tty) = std::fs::File::open("/dev/tty") else { return false };
        Command::new("stty")
            .arg(arg)
            .stdin(tty)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    impl Off {
        pub fn new() -> Off {
            Off(stty("-echo"))
        }
    }

    impl Drop for Off {
        fn drop(&mut self) {
            if self.0 {
                stty("echo");
            }
        }
    }
}

#[cfg(windows)]
mod echo {
    use std::ffi::c_void;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
    }

    /// The console and its mode before echo was turned off.
    pub struct Off(Option<(*mut c_void, u32)>);

    impl Off {
        pub fn new() -> Off {
            unsafe {
                let console = GetStdHandle(STD_INPUT_HANDLE);
                let mut mode = 0;
                if console.is_null() || GetConsoleMode(console, &mut mode) == 0 {
                    return Off(None);
                }
                if SetConsoleMode(console, mode & !ENABLE_ECHO_INPUT) == 0 {
                    return Off(None);
                }
                Off(Some((console, mode)))
            }
        }
    }

    impl Drop for Off {
        fn drop(&mut self) {
            if let Some((console, mode)) = self.0 {
                unsafe {
                    SetConsoleMode(console, mode);
                }
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod echo {
    pub struct Off;

    impl Off {
        pub fn new() -> Off {
            Off
        }
    }
}

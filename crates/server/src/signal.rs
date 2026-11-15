//! Stop requests from the operating system: SIGTERM and SIGINT (Ctrl-C) on Unix
//! (`docker stop`, `systemctl stop`, `kill`), Ctrl-C, Ctrl-Break and closing the console
//! window on Windows.
//!
//! The first request sets a flag that [`requested`] reports; the server then shuts down
//! cleanly. A second one ends the process at once.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set when the clean shutdown is over, so a Windows handler that must not return
/// before then can let the process end.
static DONE: AtomicBool = AtomicBool::new(false);

/// Whether a stop was requested.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Marks the clean shutdown as finished.
pub fn done() {
    DONE.store(true, Ordering::SeqCst);
}

/// Installs the handlers.
pub fn install() {
    imp::install();
}

#[cfg(unix)]
mod imp {
    use super::REQUESTED;
    use std::ffi::c_int;
    use std::sync::atomic::Ordering;

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;

    unsafe extern "C" {
        fn signal(signum: c_int, handler: extern "C" fn(c_int)) -> usize;
        fn _exit(status: c_int) -> !;
    }

    extern "C" fn handle(sig: c_int) {
        // Only async-signal-safe work here: an atomic swap, and `_exit`.
        if REQUESTED.swap(true, Ordering::SeqCst) {
            unsafe { _exit(128 + sig) }
        }
    }

    pub fn install() {
        unsafe {
            signal(SIGINT, handle);
            signal(SIGTERM, handle);
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{DONE, REQUESTED};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    unsafe extern "system" fn handle(event: u32) -> i32 {
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                // A second Ctrl-C: not handled, so the default handler ends the process.
                i32::from(!REQUESTED.swap(true, Ordering::SeqCst))
            }
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                // Windows ends the process when this returns (or after a few seconds),
                // so wait here for the clean shutdown.
                REQUESTED.store(true, Ordering::SeqCst);
                while !DONE.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                1
            }
            _ => 0,
        }
    }

    pub fn install() {
        unsafe {
            SetConsoleCtrlHandler(Some(handle), 1);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn install() {}
}

//! Memory accounting for query limits.
//!
//! [`CountingAllocator`], installed as a program's global allocator, counts the bytes
//! each thread allocates and frees. A query runs on one thread, so the bytes its thread
//! holds now minus what it held when the query started is what the query holds: the
//! engine compares that with the query's memory limit while it runs.
//!
//! Shared caches that a query fills on the way (the persistent store's page cache, the
//! engine's kernel and vector caches) outlive it and are not its memory: their builders
//! run inside [`exempt`], whose net allocations are not counted.
//!
//! Without the allocator nothing is counted ([`is_counting`] is `false`) and memory
//! limits are not enforced.

// The thread locals below are `const`-initialized; this lint misreads the macro.
#![allow(clippy::missing_const_for_thread_local)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

// NET: bytes allocated minus bytes freed by this thread. EXEMPT: net bytes allocated
// inside [`exempt`] on this thread.
thread_local! {
    static NET: Cell<i64> = const { Cell::new(0) };
    static EXEMPT: Cell<i64> = const { Cell::new(0) };
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// A global allocator that counts what each thread holds, on top of the system
/// allocator:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: graphbuff_core::accounting::CountingAllocator = graphbuff_core::accounting::CountingAllocator;
/// ```
pub struct CountingAllocator;

#[inline]
fn add(n: i64) {
    // `try_with`: the thread's counter may be gone while the thread exits.
    let _ = NET.try_with(|c| c.set(c.get().wrapping_add(n)));
}

#[inline]
fn installed() {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

// SAFETY: every call is passed to `System` unchanged; only counters are updated.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `System::alloc`.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            installed();
            add(layout.size() as i64);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `System::alloc_zeroed`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            installed();
            add(layout.size() as i64);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as for `System::dealloc`.
        unsafe { System.dealloc(ptr, layout) };
        add(-(layout.size() as i64));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `System::realloc`.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            add(new_size as i64 - layout.size() as i64);
        }
        p
    }
}

/// Whether [`CountingAllocator`] is the global allocator (it has allocated something).
pub fn is_counting() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// What this thread holds by the count: bytes allocated minus bytes freed, not counting
/// those inside [`exempt`].
pub fn thread_bytes() -> i64 {
    let net = NET.try_with(Cell::get).unwrap_or(0);
    let exempt = EXEMPT.try_with(Cell::get).unwrap_or(0);
    net.wrapping_sub(exempt)
}

/// Runs `f` without counting what it allocates or frees on this thread: for shared caches
/// that outlive the query that fills them.
pub fn exempt<R>(f: impl FnOnce() -> R) -> R {
    let before = NET.try_with(Cell::get).unwrap_or(0);
    let r = f();
    let grown = NET.try_with(Cell::get).unwrap_or(0).wrapping_sub(before);
    let _ = EXEMPT.try_with(|e| e.set(e.get().wrapping_add(grown)));
    r
}

/// The memory of this computer, or of the container it runs in when that is less: the
/// cgroup limit or physical memory on Linux, physical memory on Windows and macOS.
/// `None` if it cannot be found.
pub fn total_memory() -> Option<u64> {
    platform::total_memory()
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn total_memory() -> Option<u64> {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        let physical = read("/proc/meminfo").and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
            let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
            Some(kib * 1024)
        });
        // cgroup v2, then v1 (where "no limit" is a huge number).
        let cgroup = read("/sys/fs/cgroup/memory.max")
            .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n < (1 << 60));
        match (physical, cgroup) {
            (Some(p), Some(c)) => Some(p.min(c)),
            (p, c) => p.or(c),
        }
    }
}

#[cfg(windows)]
mod platform {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    pub fn total_memory() -> Option<u64> {
        let mut status = MemoryStatusEx {
            length: size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with its length set, as the call requires.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        (ok != 0 && status.total_phys > 0).then_some(status.total_phys)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    pub fn total_memory() -> Option<u64> {
        let mut bytes: u64 = 0;
        let mut len = size_of::<u64>();
        // SAFETY: a NUL-terminated name and an output buffer of `len` bytes.
        let r = unsafe {
            sysctlbyname(c"hw.memsize".as_ptr(), (&mut bytes as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0)
        };
        (r == 0 && bytes > 0).then_some(bytes)
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod platform {
    pub fn total_memory() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exempt_allocations_are_not_counted() {
        // Without the allocator installed (tests use the default one) the counters stay
        // still; the bookkeeping itself is checked by adding by hand.
        let start = thread_bytes();
        add(1000);
        assert_eq!(thread_bytes() - start, 1000);
        exempt(|| add(500));
        assert_eq!(thread_bytes() - start, 1000);
        exempt(|| add(-200));
        assert_eq!(thread_bytes() - start, 1000);
        add(-1000);
        assert_eq!(thread_bytes(), start);
    }

    #[test]
    fn total_memory_is_plausible() {
        if let Some(n) = total_memory() {
            assert!(n >= 64 << 20, "{n}");
        }
    }
}

//! SQLite's memory, counted as the script's.
//!
//! SQLite allocates through the program's allocator (`init`), so its page
//! caches, temporary tables, sorts and statements are counted with the rest of
//! the thread's memory, and the script's memory limit covers them (found by the
//! 1.1.1 audit: each connection had a 2 MB page cache that no limit counted, so
//! a script opening the same file many times held hundreds of MB). While a
//! script's statement runs (`Budget`), an allocation that would take the script
//! past its limit fails, which SQLite reports as out of memory.

use phv_vm::alloc::thread_allocated;
use rusqlite::ffi;
use std::alloc::Layout;
use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::sync::OnceLock;
use std::time::Instant;

/// The bytes before each allocation that hold its size (and keep it aligned).
const HEADER: usize = 16;

/// While a budget with a deadline is set, the deadline is looked at again
/// once SQLite has allocated this many bytes since the last look.
const LOOK_EVERY: usize = 1 << 20;

thread_local! {
    // Const-initialized and without a destructor: read from inside the allocator.
    /// The thread's allocated bytes above which SQLite's allocations fail.
    static CAP: Cell<isize> = const { Cell::new(isize::MAX) };
    /// Whether an allocation was refused since the budget was set.
    static REFUSED: Cell<bool> = const { Cell::new(false) };
    /// The script's deadline, after which SQLite's allocations fail.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// Bytes allocated since the deadline was last looked at.
    static SINCE: Cell<usize> = const { Cell::new(0) };
    /// Whether an allocation was refused because the deadline had passed.
    static LATE: Cell<bool> = const { Cell::new(false) };
}

/// Makes SQLite allocate through the program's allocator, and starts SQLite.
/// Every path that opens a database calls it first (`db.rs`, the VFS, and
/// the tests: `tests::sqlite_is_opened_through_init` in `lib.rs` checks the
/// sources), so SQLite is never started by anything else while the
/// allocator is being switched. False if SQLite was already in use (its
/// memory is then not counted).
///
/// Starting SQLite here matters: `sqlite3_config` refuses only once
/// `sqlite3_initialize` has finished, so a switch while another thread was
/// starting SQLite left memory of the default allocator for ours to free,
/// which aborted the process (found by CI on macOS, where the thread timing
/// differed: a test opened a database without calling this first).
pub(super) fn init() -> bool {
    static DONE: OnceLock<bool> = OnceLock::new();
    *DONE.get_or_init(|| {
        let methods = ffi::sqlite3_mem_methods {
            xMalloc: Some(malloc),
            xFree: Some(free),
            xRealloc: Some(realloc),
            xSize: Some(size),
            xRoundup: Some(roundup),
            xInit: Some(init_methods),
            xShutdown: Some(shutdown),
            pAppData: std::ptr::null_mut(),
        };
        // SAFETY: SQLite copies the methods; they are thread-safe. Nothing
        // else starts SQLite while this runs (see above), so the allocator
        // is in place for every allocation SQLite ever makes.
        unsafe {
            ffi::sqlite3_config(ffi::SQLITE_CONFIG_MALLOC, &raw const methods) == ffi::SQLITE_OK
                && ffi::sqlite3_initialize() == ffi::SQLITE_OK
        }
    })
}

/// Ends the process with `what` on stderr: the allocator cannot unwind, and a
/// panic in it would abort without a word.
fn give_up(what: &str) -> ! {
    eprintln!("phv: SQLite memory: {what}");
    std::process::abort()
}

/// While it lives, SQLite's allocations on this thread fail once the script
/// would pass its memory limit, and once its time limit has passed.
///
/// The deadline is looked at every `LOOK_EVERY` bytes SQLite allocates (found
/// by the 1.2.0 audit: SQLite's progress handler runs only between steps, and
/// a step may make a value of many MB; a run of such steps without a jump
/// between them never calls the handler).
pub(super) struct Budget {
    prev: isize,
    prev_deadline: Option<Instant>,
}

impl Budget {
    pub(super) fn enter(vm: &phv_vm::Vm) -> Budget {
        let cap = match vm.memory_limit() {
            Some(_) => thread_allocated().saturating_add(vm.memory_left().min(isize::MAX as usize) as isize),
            None => isize::MAX,
        };
        let prev = CAP.with(|c| c.replace(cap));
        let prev_deadline = DEADLINE.with(|d| d.replace(vm.deadline()));
        REFUSED.with(|r| r.set(false));
        LATE.with(|r| r.set(false));
        SINCE.with(|s| s.set(0));
        Budget { prev, prev_deadline }
    }

    /// Whether SQLite was refused memory since the budget was set.
    pub(super) fn refused() -> bool {
        REFUSED.with(Cell::get)
    }

    /// Whether SQLite was refused memory since the budget was set because
    /// the time limit had passed.
    pub(super) fn late() -> bool {
        LATE.with(Cell::get)
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        CAP.with(|c| c.set(self.prev));
        DEADLINE.with(|d| d.set(self.prev_deadline));
    }
}

/// Whether `more` bytes, which code of ours (an SQL function) is about to
/// allocate while SQLite runs, fit in the budget (and notes that they did
/// not). They are refused as SQLite's own would be.
pub(super) fn room(more: usize) -> bool {
    !over(more)
}

/// Whether the deadline has passed, looked at once `more` bytes bring the
/// bytes allocated since the last look to `LOOK_EVERY` (and notes that it had).
fn late(more: usize) -> bool {
    let Some(deadline) = DEADLINE.try_with(Cell::get).ok().flatten() else { return false };
    let since = SINCE.try_with(Cell::get).unwrap_or(0).saturating_add(more);
    if since < LOOK_EVERY {
        let _ = SINCE.try_with(|s| s.set(since));
        return false;
    }
    let _ = SINCE.try_with(|s| s.set(0));
    if Instant::now() <= deadline {
        return false;
    }
    let _ = LATE.try_with(|r| r.set(true));
    true
}

/// Whether `more` bytes would pass the budget (and notes that they did).
fn over(more: usize) -> bool {
    if late(more) {
        return true;
    }
    let cap = CAP.try_with(Cell::get).unwrap_or(isize::MAX);
    if cap == isize::MAX || thread_allocated().saturating_add(more.min(isize::MAX as usize) as isize) <= cap {
        return false;
    }
    let _ = REFUSED.try_with(|r| r.set(true));
    true
}

fn layout(n: usize) -> Option<Layout> {
    Layout::from_size_align(n.checked_add(HEADER)?, HEADER).ok()
}

unsafe extern "C" fn malloc(n: c_int) -> *mut c_void {
    let n = n.max(0) as usize;
    let Some(l) = layout(n) else { return std::ptr::null_mut() };
    if over(l.size()) {
        return std::ptr::null_mut();
    }
    // SAFETY: the layout has a non-zero size; the header is HEADER bytes, aligned.
    unsafe {
        let p = std::alloc::alloc(l);
        if p.is_null() {
            return p.cast();
        }
        p.cast::<usize>().write(n);
        p.add(HEADER).cast()
    }
}

/// The start of the allocation of `p`, and the size SQLite asked for.
///
/// # Safety
/// `p` came from `malloc` or `realloc` and was not freed.
unsafe fn base(p: *mut c_void) -> (*mut u8, usize) {
    unsafe {
        let b = p.cast::<u8>().sub(HEADER);
        (b, b.cast::<usize>().read())
    }
}

unsafe extern "C" fn free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    // SAFETY: `p` is one of ours; its layout is the one it was allocated with.
    unsafe {
        let (b, n) = base(p);
        let Some(l) = layout(n) else { give_up("freeing memory that was not allocated here") };
        std::alloc::dealloc(b, l);
    }
}

unsafe extern "C" fn realloc(p: *mut c_void, n: c_int) -> *mut c_void {
    if p.is_null() {
        return unsafe { malloc(n) };
    }
    let n = n.max(0) as usize;
    // SAFETY: `p` is one of ours; on failure it stays valid, as SQLite expects.
    unsafe {
        let (b, old) = base(p);
        let Some(new) = layout(n) else { return std::ptr::null_mut() };
        if n > old && over(n - old) {
            return std::ptr::null_mut();
        }
        let Some(was) = layout(old) else { give_up("resizing memory that was not allocated here") };
        let q = std::alloc::realloc(b, was, new.size());
        if q.is_null() {
            return q.cast();
        }
        q.cast::<usize>().write(n);
        q.add(HEADER).cast()
    }
}

unsafe extern "C" fn size(p: *mut c_void) -> c_int {
    if p.is_null() {
        return 0;
    }
    // SAFETY: `p` is one of ours.
    unsafe { base(p).1 as c_int }
}

unsafe extern "C" fn roundup(n: c_int) -> c_int {
    // SQLite's own allocator rounds to 8 bytes as well.
    n.saturating_add(7) & !7
}

unsafe extern "C" fn init_methods(_: *mut c_void) -> c_int {
    ffi::SQLITE_OK
}

unsafe extern "C" fn shutdown(_: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Many threads starting SQLite and switching its allocator at once:
    /// `init` serializes them, so no database is opened while the allocator
    /// changes (found by CI on macOS: an abort when a test opened a database
    /// without `init` first).
    #[test]
    fn databases_opened_at_once_share_one_allocator() {
        std::thread::scope(|s| {
            for i in 0..8 {
                s.spawn(move || {
                    for _ in 0..200 {
                        if i % 2 == 0 {
                            init();
                        }
                        init();
                        let c = rusqlite::Connection::open_in_memory().unwrap();
                        c.execute_batch("CREATE TABLE t (x TEXT); INSERT INTO t VALUES ('mk1'); DROP TABLE t").unwrap();
                    }
                });
            }
        });
    }
}

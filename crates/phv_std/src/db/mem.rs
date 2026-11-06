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

/// The bytes before each allocation that hold its size (and keep it aligned).
const HEADER: usize = 16;

thread_local! {
    // Const-initialized and without a destructor: read from inside the allocator.
    /// The thread's allocated bytes above which SQLite's allocations fail.
    static CAP: Cell<isize> = const { Cell::new(isize::MAX) };
    /// Whether an allocation was refused since the budget was set.
    static REFUSED: Cell<bool> = const { Cell::new(false) };
}

/// Makes SQLite allocate through the program's allocator. It must run before
/// SQLite is first used; every use goes through `db.rs`, which calls it first.
/// False if SQLite was already in use (its memory is then not counted).
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
        // SAFETY: SQLite copies the methods; they are thread-safe.
        unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_MALLOC, &raw const methods) == ffi::SQLITE_OK }
    })
}

/// While it lives, SQLite's allocations on this thread fail once the script
/// would pass its memory limit.
pub(super) struct Budget {
    prev: isize,
}

impl Budget {
    pub(super) fn enter(vm: &phv_vm::Vm) -> Budget {
        let cap = match vm.memory_limit() {
            Some(_) => thread_allocated().saturating_add(vm.memory_left().min(isize::MAX as usize) as isize),
            None => isize::MAX,
        };
        let prev = CAP.with(|c| c.replace(cap));
        REFUSED.with(|r| r.set(false));
        Budget { prev }
    }

    /// Whether SQLite was refused memory since the budget was set.
    pub(super) fn refused() -> bool {
        REFUSED.with(Cell::get)
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        CAP.with(|c| c.set(self.prev));
    }
}

/// Whether `more` bytes would pass the budget (and notes that they did).
fn over(more: usize) -> bool {
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
        std::alloc::dealloc(b, layout(n).expect("an allocated size"));
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
        let q = std::alloc::realloc(b, layout(old).expect("an allocated size"), new.size());
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

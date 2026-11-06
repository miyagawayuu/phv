//! A global allocator that counts the bytes allocated by each thread, used
//! for the per-request memory limit. Binaries opt in with:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: phv_vm::alloc::CountingAlloc = phv_vm::alloc::CountingAlloc;
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // Const-initialized and without a destructor, so it is safe to use from
    // inside the allocator (no lazy initialization, never torn down).
    static ALLOCATED: Cell<isize> = const { Cell::new(0) };
    /// The highest value of ALLOCATED since the last `reset_thread_peak()`.
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn add(n: isize) {
    let _ = ALLOCATED.try_with(|a| {
        let now = a.get() + n;
        a.set(now);
        if n > 0 {
            let _ = PEAK.try_with(|p| p.set(p.get().max(now)));
        }
    });
}

/// Counts `bytes` that another thread allocated and this one now holds (it
/// will free them, which subtracts them here): background work's results.
pub fn charge_thread(bytes: usize) {
    add(bytes.min(isize::MAX as usize) as isize);
}

/// The reverse of `charge_thread`: `bytes` this thread held and hands on
/// without freeing them (a kept SQLite connection going back to its pool).
pub fn uncharge_thread(bytes: usize) {
    add(-(bytes.min(isize::MAX as usize) as isize));
}

/// Bytes currently allocated (minus freed) by this thread.
pub fn thread_allocated() -> isize {
    ALLOCATED.try_with(|a| a.get()).unwrap_or(0)
}

/// The most bytes this thread had allocated at once since `reset_thread_peak()`.
pub fn thread_peak() -> isize {
    PEAK.try_with(|p| p.get()).unwrap_or(0)
}

/// Starts measuring the peak from the bytes allocated now.
pub fn reset_thread_peak() {
    let now = thread_allocated();
    let _ = PEAK.try_with(|p| p.set(now));
}

pub struct CountingAlloc;

// SAFETY: every method forwards to the system allocator unchanged and only
// updates a thread-local counter.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            add(layout.size() as isize);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(layout);
        if !p.is_null() {
            add(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        add(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            add(new_size as isize - layout.size() as isize);
        }
        p
    }
}

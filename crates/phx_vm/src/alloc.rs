//! A global allocator that counts the bytes allocated by each thread, used
//! for the per-request memory limit. Binaries opt in with:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: phx_vm::alloc::CountingAlloc = phx_vm::alloc::CountingAlloc;
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // Const-initialized and without a destructor, so it is safe to use from
    // inside the allocator (no lazy initialization, never torn down).
    static ALLOCATED: Cell<isize> = const { Cell::new(0) };
}

fn add(n: isize) {
    let _ = ALLOCATED.try_with(|a| a.set(a.get() + n));
}

/// Bytes currently allocated (minus freed) by this thread.
pub fn thread_allocated() -> isize {
    ALLOCATED.try_with(|a| a.get()).unwrap_or(0)
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

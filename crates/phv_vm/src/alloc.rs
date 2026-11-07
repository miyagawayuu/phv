//! A global allocator that counts the bytes allocated by each thread, used
//! for the per-request memory limit. Binaries opt in with:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: phv_vm::alloc::CountingAlloc = phv_vm::alloc::CountingAlloc;
//! ```
//!
//! The memory itself comes from mimalloc (`INNER`), not from the C library's
//! `malloc`: it serves a thread's small allocations from pages of its own
//! without a lock, which is what a request's run mostly does (objects,
//! strings, map entries). The counting does not depend on the allocator
//! underneath: every byte a thread asks for is added to its counter whatever
//! hands it out, so the limits fire at the same sizes with either.

use mimalloc::MiMalloc;
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;

/// The allocator that provides the memory. Its own bookkeeping (pages,
/// arenas) is not counted; the counters see what the program asked for.
const INNER: MiMalloc = MiMalloc;

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

// SAFETY: every method forwards to `INNER` unchanged and only updates a
// thread-local counter.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = INNER.alloc(layout);
        if !p.is_null() {
            add(layout.size() as isize);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = INNER.alloc_zeroed(layout);
        if !p.is_null() {
            add(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        INNER.dealloc(ptr, layout);
        add(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = INNER.realloc(ptr, layout, new_size);
        if !p.is_null() {
            add(new_size as isize - layout.size() as isize);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    extern "C" {
        // Exported by the mimalloc the sys crate links; not declared by the
        // crate without its `extended` feature.
        fn mi_is_in_heap_region(p: *const c_void) -> bool;
        fn mi_usable_size(p: *const c_void) -> usize;
    }

    /// What the counting allocator hands out comes from mimalloc, and the
    /// counter moves by exactly the bytes asked for, not by what mimalloc
    /// rounds them up to.
    #[test]
    fn counts_the_bytes_asked_for_and_serves_them_from_mimalloc() {
        let alloc = CountingAlloc;
        for (size, align) in [(1usize, 1usize), (24, 8), (1000, 16), (70_000, 64), (3 << 20, 4096)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            let before = thread_allocated();
            // SAFETY: the layouts are valid and every pointer is freed once,
            // with the layout it was last given.
            unsafe {
                let p = alloc.alloc(layout);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "alignment {align}");
                assert!(mi_is_in_heap_region(p.cast()), "{size} bytes not from mimalloc");
                assert!(mi_usable_size(p.cast()) >= size);
                assert_eq!(thread_allocated() - before, size as isize, "after alloc of {size}");

                let p = alloc.realloc(p, layout, size * 2);
                assert!(!p.is_null());
                assert!(mi_is_in_heap_region(p.cast()));
                assert_eq!(thread_allocated() - before, 2 * size as isize, "after realloc of {size}");

                let grown = Layout::from_size_align(size * 2, align).unwrap();
                alloc.dealloc(p, grown);
                assert_eq!(thread_allocated(), before, "after dealloc of {size}");

                let z = alloc.alloc_zeroed(layout);
                assert!(!z.is_null());
                assert!(std::slice::from_raw_parts(z, size).iter().all(|&b| b == 0));
                assert_eq!(thread_allocated() - before, size as isize, "after alloc_zeroed of {size}");
                alloc.dealloc(z, layout);
                assert_eq!(thread_allocated(), before);
            }
        }
    }

    /// The peak follows the counter, whatever allocator is underneath.
    #[test]
    fn peak_follows_the_counter() {
        let alloc = CountingAlloc;
        let layout = Layout::from_size_align(1 << 16, 8).unwrap();
        reset_thread_peak();
        let base = thread_allocated();
        assert_eq!(thread_peak(), base);
        // SAFETY: as above.
        unsafe {
            let a = alloc.alloc(layout);
            let b = alloc.alloc(layout);
            assert_eq!(thread_peak() - base, 2 << 16);
            alloc.dealloc(a, layout);
            alloc.dealloc(b, layout);
        }
        assert_eq!(thread_peak() - base, 2 << 16);
        assert_eq!(thread_allocated(), base);
        reset_thread_peak();
        assert_eq!(thread_peak(), base);
    }
}

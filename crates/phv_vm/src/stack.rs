//! The end of the current thread's stack, so that native code and callbacks
//! of builtins stop before they reach it (found by the 1.0.0 audit: the JIT's
//! budget was measured from where it was entered, wherever that was).

use std::cell::Cell;

/// Stack kept free below everything the VM measures: for the helpers native
/// code calls, and for a builtin's own work between two checks.
pub const STACK_MARGIN: usize = 1 << 20;

thread_local! {
    /// The lowest address of this thread's stack: 0 before it is looked up,
    /// 1 when the platform does not tell it.
    static LOW: Cell<usize> = const { Cell::new(0) };
}

/// The lowest address of the current thread's stack, when the platform tells
/// it (Windows, Linux, Android and Apple's systems). Looked up once per thread.
pub fn stack_low() -> Option<usize> {
    let low = LOW.with(|l| {
        let mut low = l.get();
        if low == 0 {
            low = lookup().filter(|&a| a > 1).unwrap_or(1);
            l.set(low);
        }
        low
    });
    (low > 1).then_some(low)
}

/// An address in the caller's stack frame.
#[inline(always)]
pub fn here() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Whether less than `bytes` of this thread's stack is left below the caller
/// (false when its end is not known).
#[inline]
pub fn left_below(bytes: usize) -> bool {
    stack_low().is_some_and(|low| here() < low.saturating_add(bytes))
}

#[cfg(windows)]
fn lookup() -> Option<usize> {
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: both pointers are to live locals; the call only writes them.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits(&mut low, &mut high) };
    (low != 0 && high > low).then_some(low)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn lookup() -> Option<usize> {
    // SAFETY: `attr` is initialized by `pthread_getattr_np` before it is read,
    // and destroyed once.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let r = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        (r == 0 && !addr.is_null() && size > 0).then_some(addr as usize)
    }
}

#[cfg(target_vendor = "apple")]
fn lookup() -> Option<usize> {
    // SAFETY: plain queries about the calling thread.
    unsafe {
        let me = libc::pthread_self();
        let top = libc::pthread_get_stackaddr_np(me) as usize;
        let size = libc::pthread_get_stacksize_np(me);
        top.checked_sub(size).filter(|_| size > 0)
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn lookup() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_end_is_below_the_caller() {
        if cfg!(any(windows, target_os = "linux", target_vendor = "apple")) {
            assert!(stack_low().is_some());
        }
        if let Some(low) = stack_low() {
            let at = here();
            assert!(low < at, "{low:#x} {at:#x}");
            // A spawned thread's stack is about the size asked for.
            let used = std::thread::Builder::new()
                .stack_size(4 << 20)
                .spawn(|| here() - stack_low().unwrap())
                .unwrap()
                .join()
                .unwrap();
            assert!((3 << 20..=8 << 20).contains(&used), "{used}");
            assert!(!left_below(64 << 10));
        }
    }
}

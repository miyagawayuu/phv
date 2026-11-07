//! When a blocking operation must give up, derived from the script's time
//! limit (`Vm::deadline`) in one place.
//!
//! A script has a deadline when it runs with a time limit, and none otherwise.
//! Three questions come up wherever the standard library waits for something
//! (a socket, a database, a lock): how long a wait of its own bounded length
//! may take (`until`), when a stop that must come just after the script's own
//! limit falls (`after`), and how much time the script has left (`remaining`).
//! Each is answered here, so every caller agrees on the instants and on what a
//! script without a limit gets.

use phv_vm::Vm;
use std::time::{Duration, Instant};

/// `cap` from now, but never past the script's deadline: the longest a
/// blocking operation may wait. Without a time limit the wait is `cap`.
pub(crate) fn until(vm: &Vm, cap: Duration) -> Instant {
    let now = Instant::now();
    vm.deadline().map_or(now + cap, |d| d.min(now + cap))
}

/// The script's deadline plus `margin`, or `None` without a time limit: a stop
/// that must come just after the script's own limit, so the limit's error, not
/// the stop's, is what the script sees (a server's statement timeout, a client
/// cutting a statement off).
pub(crate) fn after(vm: &Vm, margin: Duration) -> Option<Instant> {
    vm.deadline().map(|d| d + margin)
}

/// The time left until the script's deadline (zero once it has passed), or
/// `None` without a time limit.
pub(crate) fn remaining(vm: &Vm) -> Option<Duration> {
    vm.deadline().map(|d| d.saturating_duration_since(Instant::now()))
}

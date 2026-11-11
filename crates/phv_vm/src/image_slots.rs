//! The HTTP server's image conversion slots (`std.image`): how many pages
//! decode, resize and encode images at once, whatever the workers.
//!
//! A conversion waits for a slot in the order it asked, for as long as its
//! page may run (the page's time limit ends the wait). No client address
//! holds more than a share of the slots at once, so one client that keeps
//! every slot busy makes the others wait for one conversion at most, never
//! fail (the 20th audit's R23-8: in 1.3.1 a conversion was refused at once
//! when no slot was free). A waiting conversion whose client has gone (its
//! connection closed) gives up its place and takes no slot.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How often a waiting conversion checks its page's limits and its client.
const TICK: Duration = Duration::from_millis(20);

/// The slots of a server.
#[derive(Debug)]
pub struct ImageSlots {
    state: Mutex<SlotState>,
    freed: Condvar,
    total: usize,
    per_client: usize,
}

#[derive(Debug)]
struct SlotState {
    free: usize,
    /// Slots held, by client address.
    held: HashMap<String, usize>,
    /// The conversions waiting, in the order they asked: their ticket and client.
    waiting: VecDeque<(u64, String)>,
    next: u64,
}

impl ImageSlots {
    /// `total` slots, of which one client address holds at most half
    /// (rounded down, at least one).
    pub fn new(total: usize) -> ImageSlots {
        let total = total.max(1);
        ImageSlots {
            state: Mutex::new(SlotState { free: total, held: HashMap::new(), waiting: VecDeque::new(), next: 0 }),
            freed: Condvar::new(),
            total,
            per_client: (total / 2).max(1),
        }
    }

    /// The slots in all.
    pub fn total(&self) -> usize {
        self.total
    }

    /// The most slots one client address holds at once.
    pub fn per_client(&self) -> usize {
        self.per_client
    }

    /// The slots free now.
    pub fn free(&self) -> usize {
        self.lock().free
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A page's way to its server's slots: its client's address (as the rate
/// limit keys it) and whether that client is gone.
#[derive(Clone, Debug)]
pub struct ImageSlotsFor {
    slots: Arc<ImageSlots>,
    client: String,
    gone: Arc<AtomicBool>,
}

/// Why no slot was taken.
#[derive(Debug)]
pub enum NoSlot<E> {
    /// The client is gone.
    Gone,
    /// The page's limits ended the wait.
    Stopped(E),
}

/// A slot held; given back when dropped (also when a conversion unwinds).
#[derive(Debug)]
pub struct ImageSlot {
    slots: Arc<ImageSlots>,
    client: String,
}

impl Drop for ImageSlot {
    fn drop(&mut self) {
        let mut st = self.slots.lock();
        st.free += 1;
        if let Some(n) = st.held.get_mut(&self.client) {
            *n -= 1;
            if *n == 0 {
                st.held.remove(&self.client);
            }
        }
        drop(st);
        self.slots.freed.notify_all();
    }
}

impl ImageSlotsFor {
    /// The way to `slots` of a page for the client at `client`; `gone` is set
    /// when the client's connection closes.
    pub fn new(slots: Arc<ImageSlots>, client: String, gone: Arc<AtomicBool>) -> ImageSlotsFor {
        ImageSlotsFor { slots, client, gone }
    }

    /// Waits for a slot, in turn. `check` is called every few milliseconds
    /// while it waits (the page's limits): an error from it ends the wait.
    pub fn take<E>(&self, mut check: impl FnMut() -> Result<(), E>) -> Result<ImageSlot, NoSlot<E>> {
        if self.gone.load(Ordering::Relaxed) {
            return Err(NoSlot::Gone);
        }
        let slots = &self.slots;
        let mut st = slots.lock();
        let ticket = st.next;
        st.next += 1;
        st.waiting.push_back((ticket, self.client.clone()));
        loop {
            // The slot goes to the first waiting conversion whose client
            // holds less than its share.
            let first = st
                .waiting
                .iter()
                .find(|(_, c)| st.held.get(c).copied().unwrap_or(0) < slots.per_client)
                .map(|&(t, _)| t);
            if st.free > 0 && first == Some(ticket) {
                st.free -= 1;
                *st.held.entry(self.client.clone()).or_insert(0) += 1;
                st.waiting.retain(|&(t, _)| t != ticket);
                drop(st);
                // More slots may be free for the next in line.
                slots.freed.notify_all();
                return Ok(ImageSlot { slots: slots.clone(), client: self.client.clone() });
            }
            st = slots.freed.wait_timeout(st, TICK).unwrap_or_else(|e| e.into_inner()).0;
            drop(st);
            let stopped = check().err();
            st = slots.lock();
            if stopped.is_some() || self.gone.load(Ordering::Relaxed) {
                st.waiting.retain(|&(t, _)| t != ticket);
                drop(st);
                slots.freed.notify_all();
                return Err(stopped.map_or(NoSlot::Gone, NoSlot::Stopped));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn page(slots: &Arc<ImageSlots>, client: &str) -> ImageSlotsFor {
        ImageSlotsFor::new(slots.clone(), client.into(), Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn one_address_holds_at_most_half_of_the_slots() {
        for (total, share) in [(1, 1), (2, 1), (3, 1), (4, 2), (6, 3), (256, 128)] {
            assert_eq!(ImageSlots::new(total).per_client(), share, "{total} slots");
        }
        let slots = Arc::new(ImageSlots::new(6));
        let attacker = page(&slots, "203.0.113.9");
        let held: Vec<_> = (0..3).map(|_| attacker.take(|| Ok::<(), ()>(())).unwrap()).collect();
        // A fourth conversion from the same address waits (here until its check stops it).
        let mut checks = 0;
        let r = attacker.take(|| {
            checks += 1;
            if checks > 3 {
                Err("time limit")
            } else {
                Ok(())
            }
        });
        assert!(matches!(r, Err(NoSlot::Stopped("time limit"))));
        // Another address gets the other three at once.
        let other = page(&slots, "198.51.100.7");
        let mine: Vec<_> = (0..3).map(|_| other.take(|| Err::<(), _>("waited")).unwrap()).collect();
        assert_eq!(slots.free(), 0);
        drop((held, mine));
        assert_eq!(slots.free(), 6);
    }

    #[test]
    fn a_conversion_waits_for_a_slot_in_turn_and_gets_one() {
        let slots = Arc::new(ImageSlots::new(2));
        let a = page(&slots, "203.0.113.9");
        let b = page(&slots, "198.51.100.7");
        let held_a = a.take(|| Ok::<(), ()>(())).unwrap();
        let held_b = b.take(|| Ok::<(), ()>(())).unwrap();
        // Both slots are busy: a third client waits, and gets the first one freed.
        let c = page(&slots, "192.0.2.1");
        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            let slot = c.take(|| Ok::<(), ()>(())).unwrap();
            (start.elapsed(), slot)
        });
        std::thread::sleep(Duration::from_millis(150));
        drop(held_a);
        let (waited, slot) = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        drop((slot, held_b));
        assert_eq!(slots.free(), 2);
    }

    #[test]
    fn a_client_that_is_gone_takes_no_slot() {
        let slots = Arc::new(ImageSlots::new(1));
        let busy = page(&slots, "203.0.113.9").take(|| Ok::<(), ()>(())).unwrap();
        let gone = Arc::new(AtomicBool::new(false));
        let left = ImageSlotsFor::new(slots.clone(), "198.51.100.7".into(), gone.clone());
        let waiter = std::thread::spawn(move || left.take(|| Ok::<(), ()>(())).map(|_| ()));
        std::thread::sleep(Duration::from_millis(60));
        gone.store(true, Ordering::Relaxed);
        assert!(matches!(waiter.join().unwrap(), Err(NoSlot::Gone)));
        drop(busy);
        assert_eq!(slots.free(), 1);
        // Gone before it asks: no wait at all.
        let after = ImageSlotsFor::new(slots.clone(), "198.51.100.7".into(), gone);
        assert!(matches!(after.take(|| Ok::<(), ()>(())), Err(NoSlot::Gone)));
        assert_eq!(slots.free(), 1);
    }

    /// Waiting conversions are served in the order they asked, skipping only
    /// those whose address holds its share already.
    #[test]
    fn slots_go_in_turn_past_an_address_at_its_share() {
        let slots = Arc::new(ImageSlots::new(2));
        let a = page(&slots, "a");
        let first = a.take(|| Ok::<(), ()>(())).unwrap();
        let second = page(&slots, "b").take(|| Ok::<(), ()>(())).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for (i, client) in ["a", "c", "d"].into_iter().enumerate() {
            let p = page(&slots, client);
            let order = order.clone();
            waiters.push(std::thread::spawn(move || {
                let slot = p.take(|| Ok::<(), ()>(())).unwrap();
                order.lock().unwrap().push(client);
                std::thread::sleep(Duration::from_millis(30));
                drop(slot);
            }));
            // In this order.
            std::thread::sleep(Duration::from_millis(40 + i as u64));
        }
        // "b" frees its slot: "a" holds its share (1 of 2), so "c" goes first.
        drop(second);
        std::thread::sleep(Duration::from_millis(10));
        drop(first);
        for w in waiters {
            w.join().unwrap();
        }
        let order = order.lock().unwrap().clone();
        assert_eq!(order[0], "c", "{order:?}");
        assert_eq!(slots.free(), 2);
    }
}

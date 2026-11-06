// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compression worker pool and its memory budget.
//!
//! All compression in a run shares one [`Pool`]: one worker per core,
//! limited by a memory budget. A compressor takes a [`Lease`] for as many
//! workers as it can use and hands them back by dropping it.

use std::cell::Cell;
use std::io;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use crate::report::Warning;
use crate::sysmem::{SysmemError, physical_memory};

const MIB: u64 = 1024 * 1024;

/// The share of the machine's physical memory that all compressors in a
/// run may hold together.
pub const MEMORY_PERCENT: u8 = 25;

/// What an archive encoder counts against the memory budget whatever its
/// worker count: the round buffer zstd allocates for long-distance matching.
pub const ARCHIVE_FIXED_BYTES: u64 = 448 * MIB;

/// What each worker of an archive encoder adds.
pub const ARCHIVE_WORKER_BYTES: u64 = 160 * MIB;

/// zstd's default job size for archives. A piece smaller than this
/// cannot keep a second worker busy.
pub const ARCHIVE_JOB_BYTES: u64 = 64 * MIB;

thread_local! {
    // Set while this thread holds a lease: asking for a second one could
    // wait for the first forever.
    static HOLDING_LEASE: Cell<bool> = const { Cell::new(false) };
}

/// Workers and memory shared by all compression in a run.
#[derive(Debug)]
pub struct Pool {
    cores: usize,
    budget: u64,
    state: Mutex<State>,
    changed: Condvar,
}

/// Counters only. Every critical section leaves them consistent, which is
/// why a poisoned lock is safe to keep using.
#[derive(Debug, Default)]
struct State {
    workers_out: usize,
    bytes_out: u64,
    warned: bool,
    /// Blocking leases are served in the order they asked, so a thread
    /// that drops a lease and asks again cannot overtake a waiting one.
    next_ticket: u64,
    serving: u64,
    waiting: usize,
    /// How often a request went to sleep on `changed`, for tests that wait
    /// for a wake-up to have been seen and answered.
    #[cfg(test)]
    waits: u64,
}

/// Workers taken from a [`Pool`], returned when the lease is dropped.
///
/// A thread holds one lease at a time, around the encoder it runs. The
/// lease stays on the thread that took it, which the following shows:
///
/// ```compile_fail
/// // `Lease` is not `Send`: it cannot be moved to another thread.
/// fn is_send<T: Send>() {}
/// is_send::<antiq::archive::pool::Lease<'static>>();
/// ```
#[derive(Debug)]
pub struct Lease<'a> {
    pool: &'a Pool,
    workers: usize,
    bytes: u64,
    warning: Option<Warning>,
    not_send: PhantomData<*const ()>,
}

impl Lease<'_> {
    /// How many workers the holder may run.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Set on the first lease that had to exceed the memory limit to get
    /// its one worker.
    pub fn warning(&self) -> Option<&Warning> {
        self.warning.as_ref()
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut state = self.pool.state();
        state.workers_out -= self.workers;
        state.bytes_out -= self.bytes;
        drop(state);
        self.pool.changed.notify_all();
        HOLDING_LEASE.set(false);
    }
}

impl Pool {
    /// A pool of `cores` workers (at least one) and `memory_budget` bytes.
    pub fn new(cores: usize, memory_budget: u64) -> Pool {
        Pool {
            cores: cores.max(1),
            budget: memory_budget,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }

    /// The pool of this machine: one worker per core and
    /// [`MEMORY_PERCENT`] of the physical memory.
    pub fn for_machine() -> Result<Pool, SysmemError> {
        for_machine_with(
            physical_memory(),
            cores_from(std::thread::available_parallelism()),
            MEMORY_PERCENT,
        )
    }

    /// Takes up to `want` workers and waits until at least one is
    /// available. Requests are served in the order they arrive.
    ///
    /// The lease counts `fixed_bytes + workers * bytes_per_worker` against
    /// the memory budget. Archives pass [`ARCHIVE_FIXED_BYTES`] and
    /// [`ARCHIVE_WORKER_BYTES`]; other compression passes the figures of
    /// its own settings. A cost of `(0, 0)` counts against the cores only.
    ///
    /// A thread must not call this while it holds a lease, since it could
    /// wait for itself.
    pub fn lease(&self, want: usize, fixed_bytes: u64, bytes_per_worker: u64) -> Lease<'_> {
        assert!(
            !HOLDING_LEASE.get(),
            "this thread already holds a lease and would wait for itself"
        );
        let cost = Cost {
            fixed: fixed_bytes,
            per_worker: bytes_per_worker,
        };
        let mut state = self.state();
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.waiting += 1;
        // Nothing from taking the ticket to the grant may panic: the ticket
        // would never be served and every request behind it would wait for
        // good.
        loop {
            if state.serving == ticket
                && let Some(lease) = self.grant(&mut state, want, cost)
            {
                state.waiting -= 1;
                state.serving += 1;
                drop(state);
                // The next in line may be able to go now.
                self.changed.notify_all();
                return lease;
            }
            #[cfg(test)]
            {
                state.waits += 1;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Like [`Pool::lease`], but gives up instead of waiting, and also
    /// when another request is already waiting.
    pub fn try_lease(
        &self,
        want: usize,
        fixed_bytes: u64,
        bytes_per_worker: u64,
    ) -> Option<Lease<'_>> {
        assert!(
            !HOLDING_LEASE.get(),
            "this thread already holds a lease and would wait for itself"
        );
        let cost = Cost {
            fixed: fixed_bytes,
            per_worker: bytes_per_worker,
        };
        let mut state = self.state();
        if state.next_ticket != state.serving {
            return None;
        }
        self.grant(&mut state, want, cost)
    }

    /// Requests currently blocked in [`Pool::lease`].
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.state().waiting
    }

    /// Times requests went to sleep in [`Pool::lease`] so far.
    #[cfg(test)]
    fn waits(&self) -> u64 {
        self.state().waits
    }

    // A poisoned lock is taken over: the counters are consistent after any
    // panic, and a panic in `Drop` while another one unwinds would abort.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands out workers if the cores and the memory allow it.
    fn grant(&self, state: &mut State, want: usize, cost: Cost) -> Option<Lease<'_>> {
        let by_cores = self.cores - state.workers_out;
        // The fixed part comes off first. A worker size of 0 has no
        // division result: no further memory limit.
        let free = self.budget.saturating_sub(state.bytes_out);
        let by_memory = free.checked_sub(cost.fixed).map_or(0, |rest| {
            rest.checked_div(cost.per_worker)
                .map_or(usize::MAX, |workers| {
                    usize::try_from(workers).unwrap_or(usize::MAX)
                })
        });
        let mut workers = want.max(1).min(by_cores).min(by_memory);
        let mut warning = None;

        if workers == 0 {
            // Nothing out means one worker is always granted, even when the
            // memory limit is smaller than an encoder with one worker. It
            // counts in full, so a lease with a memory cost waits for it;
            // one that costs no memory still starts next to it.
            if state.workers_out > 0 {
                return None;
            }
            workers = 1;
            if !state.warned {
                state.warned = true;
                warning = Some(Warning::new(format!(
                    "the memory limit of {} MiB is below the {} MiB of one compression worker, \
                     compressing with one worker at a time",
                    self.budget / MIB,
                    cost.of(1) / MIB
                )));
            }
        }

        // A normal grant fits the free memory by the checks above, and a
        // forced one starts from nothing out; only the forced one can reach
        // the saturation.
        let bytes = cost.of(workers);
        state.workers_out += workers;
        state.bytes_out += bytes;
        HOLDING_LEASE.set(true);
        Some(Lease {
            pool: self,
            workers,
            bytes,
            warning,
            not_send: PhantomData,
        })
    }
}

/// What a lease counts against the memory budget.
#[derive(Debug, Clone, Copy)]
struct Cost {
    fixed: u64,
    per_worker: u64,
}

impl Cost {
    /// `fixed + workers * per_worker`, saturating at `u64::MAX`.
    fn of(self, workers: usize) -> u64 {
        self.fixed
            .saturating_add((workers as u64).saturating_mul(self.per_worker))
    }
}

/// How many workers a piece can keep busy: one per started job, at least
/// one. A zero job size counts as one job.
pub fn workers_for_piece(piece_bytes: u64, job_bytes: u64) -> usize {
    if job_bytes == 0 {
        return 1;
    }
    let jobs = piece_bytes.div_ceil(job_bytes).max(1);
    usize::try_from(jobs).unwrap_or(usize::MAX)
}

/// `percent` of `ram`, with `percent` held to the 1 to 100 of the config.
fn budget_for(ram: u64, percent: u8) -> u64 {
    let percent = u128::from(percent.clamp(1, 100));
    u64::try_from(u128::from(ram) * percent / 100).expect("at most 100 percent of a u64 fits")
}

/// One core when the platform cannot say.
fn cores_from(parallelism: io::Result<NonZeroUsize>) -> usize {
    parallelism.map_or(1, NonZeroUsize::get)
}

fn for_machine_with(
    ram: Result<u64, SysmemError>,
    cores: usize,
    memory_percent: u8,
) -> Result<Pool, SysmemError> {
    Ok(Pool::new(cores, budget_for(ram?, memory_percent)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Failure bound for a thread that should have been woken: a test that
    /// waits this long has failed.
    const WAKE_BOUND: Duration = Duration::from_secs(30);

    /// Failure bound for the tests that run many rounds, where a missed
    /// wake-up is rescued and reported instead of waited for.
    const ROUND_BOUND: Duration = Duration::from_secs(3);

    /// Rounds of the tests that look for a missed wake-up.
    const ROUNDS: usize = 300;

    /// Waits until `count` requests block in the pool. The spin yields
    /// instead of sleeping and fails when it runs out.
    fn wait_until_waiting(pool: &Pool, count: usize) {
        for _ in 0..50_000_000 {
            if pool.waiting() == count {
                return;
            }
            thread::yield_now();
        }
        panic!("the thread never waited");
    }

    /// Waits until requests have gone to sleep in the pool `count` times.
    fn wait_until_waits(pool: &Pool, count: u64) {
        for _ in 0..50_000_000 {
            if pool.waits() >= count {
                return;
            }
            thread::yield_now();
        }
        panic!("the thread never went back to waiting");
    }

    /// Receives `count` messages. When one does not arrive in time, the
    /// pool's waiters are woken so that their threads can end, and the
    /// result says the wake-up was missed.
    fn receive_all<T>(pool: &Pool, received: &mpsc::Receiver<T>, count: usize) -> bool {
        for _ in 0..count {
            if received.recv_timeout(ROUND_BOUND).is_err() {
                pool.changed.notify_all();
                return false;
            }
        }
        true
    }

    #[test]
    fn two_leases_never_exceed_the_cores() {
        let pool = Pool::new(4, 0);
        let first = pool.try_lease(3, 0, 0).expect("cores are free");
        assert_eq!(first.workers(), 3);
        // The test thread holds `first`, so the second lease runs on another.
        thread::scope(|scope| {
            scope.spawn(|| {
                let second = pool.try_lease(3, 0, 0).expect("one core is left");
                assert_eq!(second.workers(), 1);
                // A thread holds one lease at a time, so a third one is asked for elsewhere.
                thread::scope(|inner| {
                    inner.spawn(|| assert!(pool.try_lease(1, 0, 0).is_none(), "all cores are out"));
                });
            });
        });
    }

    #[test]
    fn an_archive_encoder_counts_a_fixed_part_and_each_worker() {
        let pool = Pool::new(8, 1024 * MIB);
        let lease = pool
            .try_lease(8, ARCHIVE_FIXED_BYTES, ARCHIVE_WORKER_BYTES)
            .expect("fits");
        // 1024 - 448 = 576 MiB leave room for three workers of 160 MiB.
        assert_eq!(lease.workers(), 3);
        assert!(lease.warning().is_none());
    }

    #[test]
    fn a_quarter_of_one_gibibyte_forces_one_archive_worker_and_warns() {
        let pool = Pool::new(8, budget_for(1024 * MIB, 25));
        let lease = pool
            .try_lease(8, ARCHIVE_FIXED_BYTES, ARCHIVE_WORKER_BYTES)
            .expect("forced");
        assert_eq!(lease.workers(), 1);
        assert_eq!(
            lease.warning().expect("256 MiB hold no encoder").message,
            "the memory limit of 256 MiB is below the 608 MiB of one compression worker, \
             compressing with one worker at a time"
        );
    }

    #[test]
    fn a_second_encoder_waits_when_only_the_per_worker_part_is_free() {
        let pool = Pool::new(8, 1024 * MIB);
        let first = pool
            .try_lease(1, ARCHIVE_FIXED_BYTES, ARCHIVE_WORKER_BYTES)
            .expect("fits");
        assert_eq!(first.workers(), 1);
        // 1024 - 608 = 416 MiB are free: room for a worker, not for the
        // fixed part of another encoder.
        thread::scope(|scope| {
            scope.spawn(|| {
                assert!(
                    pool.try_lease(1, ARCHIVE_FIXED_BYTES, ARCHIVE_WORKER_BYTES)
                        .is_none()
                );
            });
        });
    }

    #[test]
    fn a_fixed_part_that_fits_leaves_workers_without_a_memory_limit() {
        let pool = Pool::new(4, 100 * MIB);
        let lease = pool.try_lease(4, 50 * MIB, 0).expect("fits");
        assert_eq!(lease.workers(), 4);
        assert!(lease.warning().is_none());
    }

    #[test]
    fn a_fixed_part_above_the_budget_forces_one_worker() {
        let pool = Pool::new(4, 100 * MIB);
        let lease = pool.try_lease(4, 200 * MIB, 0).expect("forced");
        assert_eq!(lease.workers(), 1);
        assert!(lease.warning().is_some());
    }

    #[test]
    fn budget_for_takes_a_percentage_of_ram() {
        assert_eq!(budget_for(1024 * MIB, 25), 256 * MIB);
        assert_eq!(budget_for(u64::MAX, 100), u64::MAX);
        assert_eq!(budget_for(1000, 0), 10, "0 percent counts as 1");
        assert_eq!(budget_for(1000, 101), 1000, "101 percent counts as 100");
    }

    #[test]
    fn a_budget_below_one_worker_still_grants_one_and_warns() {
        let pool = Pool::new(4, 100 * MIB);
        let lease = pool.try_lease(4, 0, 256 * MIB).expect("forced");
        assert_eq!(lease.workers(), 1);
        let warning = lease.warning().expect("the limit is below one worker");
        assert_eq!(
            warning.message,
            "the memory limit of 100 MiB is below the 256 MiB of one compression worker, \
             compressing with one worker at a time"
        );
    }

    #[test]
    fn the_warning_is_given_once_per_pool() {
        let pool = Pool::new(4, 100 * MIB);
        let first = pool.try_lease(1, 0, 256 * MIB).expect("forced");
        assert!(first.warning().is_some());
        drop(first);
        let second = pool.try_lease(1, 0, 256 * MIB).expect("forced");
        assert!(second.warning().is_none());
    }

    #[test]
    fn a_forced_lease_charges_its_full_size() {
        let pool = Pool::new(4, 100 * MIB);
        let forced = pool.try_lease(1, 0, 256 * MIB).expect("forced");
        thread::scope(|scope| {
            scope.spawn(|| {
                assert!(
                    pool.try_lease(1, 0, 10 * MIB).is_none(),
                    "the forced worker is over the limit"
                );
            });
        });
        drop(forced);
        assert!(pool.try_lease(1, 0, 10 * MIB).is_some());
    }

    #[test]
    fn a_lease_that_costs_no_memory_starts_next_to_a_forced_one() {
        let pool = Pool::new(4, 100 * MIB);
        let _forced = pool.try_lease(1, 0, 256 * MIB).expect("forced");
        thread::scope(|scope| {
            scope.spawn(|| {
                let free = pool.try_lease(1, 0, 0).expect("only cores count");
                assert_eq!(free.workers(), 1);
            });
        });
    }

    #[test]
    fn try_lease_refuses_when_the_cores_are_out() {
        let pool = Pool::new(1, 1024 * MIB);
        let held = pool.try_lease(1, 0, 0).expect("free");
        thread::scope(|scope| {
            scope.spawn(|| assert!(pool.try_lease(1, 0, 0).is_none()));
        });
        drop(held);
        assert!(pool.try_lease(1, 0, 0).is_some());
    }

    #[test]
    fn try_lease_refuses_when_the_memory_is_out() {
        let pool = Pool::new(8, 256 * MIB);
        let held = pool.try_lease(1, 0, 256 * MIB).expect("free");
        thread::scope(|scope| {
            scope.spawn(|| assert!(pool.try_lease(1, 0, 256 * MIB).is_none()));
        });
        drop(held);
        assert!(pool.try_lease(1, 0, 256 * MIB).is_some());
    }

    /// Takes the only worker on the test thread, lets a second thread block
    /// in `lease`, then drops the first and reports what the second got.
    fn blocked_lease_gets_woken(pool: &Pool, bytes_per_worker: u64) -> usize {
        let held = pool.try_lease(1, 0, bytes_per_worker).expect("free");
        let (granted, received) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let lease = pool.lease(1, 0, bytes_per_worker);
                granted.send(lease.workers()).expect("the test listens");
            });
            wait_until_waiting(pool, 1);
            assert!(received.try_recv().is_err(), "nothing was free yet");
            drop(held);
            received.recv_timeout(WAKE_BOUND).expect("the waiter woke")
        })
    }

    #[test]
    fn a_waiting_lease_proceeds_when_the_cores_come_back() {
        let pool = Pool::new(1, 1024 * MIB);
        assert_eq!(blocked_lease_gets_woken(&pool, 0), 1);
    }

    #[test]
    fn a_waiting_lease_proceeds_when_the_memory_comes_back() {
        let pool = Pool::new(8, 256 * MIB);
        assert_eq!(blocked_lease_gets_woken(&pool, 256 * MIB), 1);
    }

    #[test]
    fn a_waiter_goes_before_a_new_lease() {
        let pool = Pool::new(2, 0);
        let held = pool.try_lease(2, 0, 0).expect("free");
        let (granted, received) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let pool = &pool;
        thread::scope(|scope| {
            scope.spawn(move || {
                let lease = pool.lease(2, 0, 0);
                granted.send(lease.workers()).expect("the test listens");
                released
                    .recv_timeout(WAKE_BOUND)
                    .expect("the test releases");
            });
            wait_until_waiting(pool, 1);
            drop(held);
            assert!(
                pool.try_lease(1, 0, 0).is_none(),
                "a new request must not overtake the waiter"
            );
            assert_eq!(received.recv_timeout(WAKE_BOUND).expect("woke"), 2);
            release.send(()).expect("the thread waits");
        });
    }

    #[test]
    fn a_request_that_fits_does_not_overtake_an_earlier_waiter() {
        // A needs an idle pool; B costs no memory and would fit next to the
        // held lease, so only its place in the queue holds it back.
        let pool = Pool::new(4, 100 * MIB);
        let held = pool.try_lease(1, 0, 0).expect("free");
        let (tx, rx) = mpsc::channel::<&'static str>();
        let (release, released) = mpsc::channel::<()>();
        let pool = &pool;
        thread::scope(|scope| {
            let a = tx.clone();
            scope.spawn(move || {
                let _lease = pool.lease(1, 200 * MIB, 0);
                a.send("A").expect("the test listens");
                released
                    .recv_timeout(WAKE_BOUND)
                    .expect("the test releases");
            });
            wait_until_waiting(pool, 1);
            let b = tx.clone();
            scope.spawn(move || {
                let _lease = pool.lease(1, 0, 0);
                b.send("B").expect("the test listens");
            });
            // B was granted at once if it did not stay in the queue, and
            // the count of waiting requests would stay at one.
            wait_until_waiting(pool, 2);
            drop(held);
            // Both threads send after their grant, so their messages may
            // arrive in either order.
            let mut served = [
                rx.recv_timeout(WAKE_BOUND).expect("A or B"),
                rx.recv_timeout(WAKE_BOUND).expect("B or A"),
            ];
            served.sort_unstable();
            assert_eq!(served, ["A", "B"]);
            release.send(()).expect("A waits");
        });
    }

    #[test]
    fn a_drop_wakes_every_waiter_in_turn() {
        // Two waiters on one core: the drop has to reach the head one, and
        // its own drop the next. A missed wake-up leaves one of them stuck.
        for round in 0..ROUNDS {
            let pool = Pool::new(1, 0);
            let held = pool.try_lease(1, 0, 0).expect("free");
            let (tx, rx) = mpsc::channel::<u8>();
            let pool = &pool;
            let all_served = thread::scope(|scope| {
                for id in 0..2 {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let _lease = pool.lease(1, 0, 0);
                        tx.send(id).expect("the test listens");
                    });
                    wait_until_waiting(pool, usize::from(id) + 1);
                }
                drop(held);
                receive_all(pool, &rx, 2)
            });
            assert!(all_served, "a waiter was not woken in round {round}");
        }
    }

    #[test]
    fn a_waiter_that_slept_again_is_woken_by_the_next_drop() {
        // The first drop frees too little for the head waiter A, which goes
        // back to sleep behind B in the condition variable's queue. The
        // second drop must still reach A.
        for round in 0..ROUNDS {
            let pool = Pool::new(4, 256 * MIB);
            let held = pool.try_lease(1, 0, 128 * MIB).expect("free");
            let (tx, rx) = mpsc::channel::<&'static str>();
            let (got, got_it) = mpsc::channel::<()>();
            let (go, may_go) = mpsc::channel::<()>();
            let pool = &pool;
            let all_served = thread::scope(|scope| {
                scope.spawn(move || {
                    let _lease = pool.lease(1, 0, 128 * MIB);
                    got.send(()).expect("the test listens");
                    may_go.recv_timeout(WAKE_BOUND).expect("the test lets go");
                });
                got_it.recv_timeout(WAKE_BOUND).expect("the first lease");
                let a = tx.clone();
                scope.spawn(move || {
                    let _lease = pool.lease(1, 0, 256 * MIB);
                    a.send("A").expect("the test listens");
                });
                wait_until_waiting(pool, 1);
                let b = tx.clone();
                scope.spawn(move || {
                    let _lease = pool.lease(1, 0, 0);
                    b.send("B").expect("the test listens");
                });
                wait_until_waiting(pool, 2);
                let slept = pool.waits();
                go.send(()).expect("the thread waits");
                // Some waiter looked again and went back to sleep.
                wait_until_waits(pool, slept + 1);
                drop(held);
                receive_all(pool, &rx, 2)
            });
            assert!(all_served, "a waiter was not woken in round {round}");
        }
    }

    #[test]
    fn a_grant_passes_the_wake_up_on() {
        // One drop frees both: A needs the memory that came back, B needs
        // only a core. B can go only after A's grant has woken it.
        for round in 0..ROUNDS {
            let pool = Pool::new(2, 256 * MIB);
            let held = pool.try_lease(1, 0, 256 * MIB).expect("free");
            let (tx, rx) = mpsc::channel::<u8>();
            let (release, released) = mpsc::channel::<()>();
            let pool = &pool;
            let all_served = thread::scope(|scope| {
                let a = tx.clone();
                scope.spawn(move || {
                    let _lease = pool.lease(1, 0, 256 * MIB);
                    a.send(0).expect("the test listens");
                    let _ = released.recv_timeout(WAKE_BOUND);
                });
                wait_until_waiting(pool, 1);
                let b = tx.clone();
                scope.spawn(move || {
                    let _lease = pool.lease(1, 0, 0);
                    b.send(1).expect("the test listens");
                });
                wait_until_waiting(pool, 2);
                drop(held);
                let all_served = receive_all(pool, &rx, 2);
                release.send(()).expect("A waits for the release");
                all_served
            });
            assert!(all_served, "a waiter was not woken in round {round}");
        }
    }

    #[test]
    fn a_zero_cost_has_no_memory_limit() {
        let pool = Pool::new(4, 0);
        let lease = pool.try_lease(4, 0, 0).expect("cores only");
        assert_eq!(lease.workers(), 4);
        assert!(lease.warning().is_none());
    }

    #[test]
    fn a_huge_cost_does_not_overflow() {
        let pool = Pool::new(4, 1024 * MIB);
        let huge = u64::MAX / 2 + 1;
        let lease = pool.try_lease(4, huge, huge).expect("forced");
        assert_eq!(lease.workers(), 1);
        drop(lease);
        let lease = pool.try_lease(4, 0, u64::MAX).expect("forced");
        assert_eq!(lease.workers(), 1);
    }

    #[test]
    fn workers_for_piece_counts_started_jobs() {
        let job = ARCHIVE_JOB_BYTES;
        assert_eq!(workers_for_piece(0, job), 1);
        assert_eq!(workers_for_piece(1, job), 1);
        assert_eq!(workers_for_piece(job, job), 1);
        assert_eq!(workers_for_piece(job + 1, job), 2);
        assert_eq!(workers_for_piece(5 * job, job), 5);
        assert_eq!(workers_for_piece(u64::MAX, 1), usize::MAX);
        assert_eq!(workers_for_piece(10 * 1024, 0), 1, "no job size, one job");
    }

    #[test]
    fn a_want_of_zero_is_taken_as_one() {
        let pool = Pool::new(2, 0);
        let first = pool.try_lease(0, 0, 0).expect("free");
        assert_eq!(first.workers(), 1);
        assert!(first.warning().is_none());
        // The clamp matters while the first is held: a want of 0 would
        // otherwise look like "no worker fits" and wait for an idle pool.
        thread::scope(|scope| {
            scope.spawn(|| {
                let second = pool.try_lease(0, 0, 0).expect("a core is left");
                assert_eq!(second.workers(), 1);
            });
        });
    }

    #[test]
    fn a_pool_has_at_least_one_core() {
        let pool = Pool::new(0, 0);
        assert_eq!(pool.try_lease(8, 0, 0).expect("free").workers(), 1);
    }

    #[test]
    fn dropping_a_lease_returns_its_workers_and_memory() {
        let pool = Pool::new(2, 512 * MIB);
        drop(pool.try_lease(2, 0, 256 * MIB).expect("free"));
        let again = pool
            .try_lease(2, 0, 256 * MIB)
            .expect("everything came back");
        assert_eq!(again.workers(), 2);
    }

    #[test]
    fn a_poisoned_pool_still_leases() {
        let pool = Pool::new(2, 0);
        thread::scope(|scope| {
            let poisoner = scope.spawn(|| {
                let _state = pool.state();
                panic!("poisons the lock on purpose");
            });
            poisoner.join().expect_err("the thread panicked");
        });
        assert!(pool.state.is_poisoned(), "the precondition of this test");
        assert_eq!(pool.try_lease(2, 0, 0).expect("free").workers(), 2);
    }

    #[test]
    #[should_panic(expected = "already holds a lease")]
    fn a_second_lease_on_the_same_thread_is_refused() {
        let pool = Pool::new(4, 0);
        let _first = pool.try_lease(1, 0, 0).expect("free");
        let _second = pool.try_lease(1, 0, 0);
    }

    #[test]
    #[should_panic(expected = "already holds a lease")]
    fn a_blocking_second_lease_on_the_same_thread_is_refused() {
        let pool = Pool::new(4, 0);
        let _first = pool.try_lease(1, 0, 0).expect("free");
        let _second = pool.lease(1, 0, 0);
    }

    #[test]
    fn cores_from_falls_back_to_one() {
        let four = NonZeroUsize::new(4).expect("not zero");
        assert_eq!(cores_from(Ok(four)), 4);
        assert_eq!(cores_from(Err(io::Error::other("unknown"))), 1);
    }

    #[test]
    fn for_machine_passes_the_memory_error_on() {
        let result = for_machine_with(Err(SysmemError::Unavailable), 4, 25);
        assert!(matches!(result, Err(SysmemError::Unavailable)));
    }

    #[test]
    fn for_machine_sizes_the_pool_from_memory_and_cores() {
        let pool = for_machine_with(Ok(1024 * MIB), 4, 25).expect("memory is known");
        assert_eq!((pool.cores, pool.budget), (4, 256 * MIB));
    }

    #[test]
    fn the_pool_of_this_machine_has_its_cores_and_a_quarter_of_its_memory() {
        let pool = Pool::for_machine().expect("the test machine reports its memory");
        let cores = std::thread::available_parallelism()
            .expect("the test machine reports its cores")
            .get();
        let memory = physical_memory().expect("the test machine reports its memory");
        assert_eq!((pool.cores, pool.budget), (cores, budget_for(memory, 25)));
    }

    #[test]
    fn the_shared_pool_is_one_instance() {
        assert!(std::ptr::eq(
            crate::testing::shared_pool(),
            crate::testing::shared_pool()
        ));
    }
}

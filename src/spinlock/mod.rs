//! A spinlock: mutual exclusion by busy-waiting rather than parking.
//!
//! # Contention and fairness
//!
//! Every waiter spins on the same flag, so a release writes a line
//! that all N of them hold Shared: all N copies are invalidated, all
//! N re-read it, all N attempt the CAS, and N-1 lose and go round
//! again. A handoff costs more the more waiters there are.
//!
//! The same mechanism decides *who* wins, and not in arrival order.
//! The releasing store leaves the line Modified in that core's own
//! cache, so a thread that unlocks and immediately asks again can
//! take the CAS with no coherence traffic at all, while every other
//! waiter is still fetching the line. Re-acquisition by the previous
//! holder is the cheapest outcome available and therefore the
//! likeliest one -- the lock barges, and the shorter the gap between
//! release and re-acquire the more it does so. Among the remaining
//! waiters the same logic sorts by distance: the line goes to
//! whichever core the fabric reaches first, which is a property of
//! the topology rather than of who asked first, so a thread can lose
//! repeatedly.
//!
//! Neither effect shows up in a throughput number -- an acquisition
//! that skipped the transfer is a fast acquisition -- which is why
//! `benches/fairness.rs` measures it separately, and what
//! [`McsSpinlock`](crate::mcs_spinlock::McsSpinlock) removes by
//! giving each waiter a flag of its own.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cache::Aligned;
use crate::spin::spin_hint;

// Re-exported rather than defined here. The padding table and the
// spin hint moved to `crate::cache` and `crate::spin` when the MCS
// lock started needing them too; this is the path the docs and the
// tests below have always used, so it stays.
pub use crate::cache::CACHE_LINE_ALIGN;

// `T: ?Sized` -- the one bound that REMOVES a requirement
//
// Every generic parameter in Rust carries an implicit `T: Sized`
// bound. `?Sized` opts out of it, widening what T may be to include
// slices (`[u8]`) and trait objects (`dyn Debug`). So unlike every
// other bound, adding `?Sized` makes the type MORE general, not
// less, and it costs you nothing at the definition site.
//
// The catch is that a relaxation has to be repeated at every impl
// block that wants it, because each impl re-declares its own T with
// its own fresh implicit Sized bound. Hence the `?Sized` sprinkled
// through the impls below: they are not restating this line, each
// one is opting out again on its own behalf.
//
// The unsized field must be the last field: the compiler lays out
// everything before it at fixed offsets and lets the tail be
// variable-length. `data` satisfies that here.

/// A mutual-exclusion lock that busy-waits rather than parking the
/// thread.
///
/// Access the protected value through [`Spinlock::lock`] or
/// [`Spinlock::try_lock`], which return a guard; the lock is
/// released when that guard is dropped.
///
/// Suited to critical sections short enough that spinning costs
/// less than a context switch. There is no poisoning: a panic while
/// a guard is alive releases the lock and leaves the value as it
/// was.
///
/// # Examples
///
/// ```
/// use lockfree_rs::spinlock::Spinlock;
///
/// let lock = Spinlock::new(0u32);
/// *lock.lock() += 1;
/// assert_eq!(lock.into_inner(), 1);
/// ```
#[derive(Debug)]
pub struct Spinlock<T: ?Sized> {
    locked: Aligned<AtomicBool>,
    // UnsafeCell is not Sync, but we want Spinlock<T> to be sync
    // So we'll need to mark Spinlock<T> as sync since it is safe
    // to share a &Spinlock<T> between threads.
    data: UnsafeCell<T>,
}

// SAFETY: we can share a &Spinlock<T> between threads since access
// to the data it protects is via atomics.
//
// Send and Sync are *auto traits*: the compiler normally derives
// them structurally, a struct gets them if every field has them.
// UnsafeCell is the one type deliberately excluded from Sync,
// because it is the only legal way to mutate through a shared
// reference. Spinlock inherits that exclusion, so we opt back in
// by hand.
//
// Why the bound is `Send` and not `Send + Sync`:
//
//   Send IS needed. A T locked on thread A and dropped on thread B
//   has effectively crossed a thread boundary, so a
//   `Spinlock<Rc<..>>` shared between threads would let two threads
//   race on a non-atomic refcount.
//
//   Sync is NOT needed.
//   T: Sync would mean "two threads may hold `&T` simultaneously",
//   but the lock's whole job is to make sure that never happens.
//   The guard hands out `&T`, yet only one guard exists at a time
//   and the borrow checker ties that reference to the guard's
//   lifetime so it cannot escape the critical section. This is why
//   `Spinlock<Cell<u32>>` is legitimately Sync even though
//   `Cell<u32>` is not, and it is the same bound std::sync::Mutex
//   chooses.
//
// There is deliberately no `unsafe impl Send`. Send derives
// structurally without help (AtomicBool is Send; `UnsafeCell<T>` is
// Send whenever T is), so only Sync was ever load-bearing. Writing
// the Send impl anyway would be inert today but would silently keep
// asserting Send if a raw-pointer field were added later.
unsafe impl<T: Send + ?Sized> Sync for Spinlock<T> {}

/// RAII guard granting access to the value protected by a
/// [`Spinlock`].
///
/// Returned by [`Spinlock::lock`] and [`Spinlock::try_lock`].
/// Dereferences to the protected value, and releases the lock when
/// dropped.
///
/// Deliberately not [`Send`]: the lock must be released by the
/// thread that acquired it.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct SpinlockGuard<'a, T: ?Sized> {
    lock: &'a Spinlock<T>,
    // A zero-sized field whose only job is to SUBTRACT auto traits.
    // Raw pointers are neither Send nor Sync, so a struct holding
    // `PhantomData<*const ()>` is neither, which stops a lock taken
    // on one thread from being released on another.
    //
    // On stable Rust this is the only way to say "not Send"; std
    // writes `impl !Send for MutexGuard`, but negative impls are
    // still unstable. The blunt part is that it removes Sync too,
    // which we did not want -- see the impl immediately below.
    _not_send: PhantomData<*const ()>,
}

// SAFETY: restores the Sync that PhantomData<*const ()> took away,
// since we only ever wanted !Send. An explicit impl overrides the
// auto-trait inference, so this wins over the raw pointer's
// contribution.
//
// Sound because sharing `&SpinlockGuard` only ever yields `&T` (via
// Deref; DerefMut needs `&mut self`, which a shared guard cannot
// give you). `T: Sync` is precisely the promise that concurrent
// `&T` is safe, so the bound is exactly right -- and note this is
// the mirror image of the Spinlock impl above: there we needed Send
// and not Sync, here we need Sync and not Send.
unsafe impl<T: ?Sized + Sync> Sync for SpinlockGuard<'_, T> {}

// None of these three impls need anything OF T: Deref and DerefMut
// just reborrow a pointer, and Drop only touches the AtomicBool.
// The `?Sized` is therefore the only bound present, and it is there
// to permit unsized T rather than to demand anything.
impl<'a, T: ?Sized> Deref for SpinlockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: since the guard exists we must be the only
        // holder of the lock, so no other thread can have a
        // reference to the data.
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T: ?Sized> DerefMut for SpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: since the guard exists we must be the only
        // holder of the lock, so no other thread can have a
        // reference to the data.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        // Release pairs with the Acquire on the CAS below: every
        // write made inside the critical section happens-before the
        // next thread's successful acquire, so the incoming holder
        // is guaranteed to see them.
        self.lock.locked.store(false, Ordering::Release);
    }
}

// This block is `impl<T>`, NOT `impl<T: ?Sized>`, and the split
// between it and the block below is entirely driven by that.
//
// Both methods here move a T across the function boundary: new()
// takes one by value, into_inner() returns one. Values can only be
// passed or returned when their size is known at compile time, so
// these two genuinely require Sized and are stuck here. The methods
// in the next block only ever deal in references (which are sized
// even when their target isn't), so they are free to be generic
// over unsized T.
//
// This is the normal pattern for a ?Sized container: a small Sized
// impl for construction and consumption, a larger ?Sized impl for
// everything you do in between.
impl<T> Spinlock<T> {
    /// Creates a new `Spinlock`, unlocked, wrapping `data`.
    pub fn new(data: T) -> Self {
        Self {
            locked: Aligned(AtomicBool::new(false)),
            data: UnsafeCell::new(data),
        }
    }

    /// Consumes the lock and returns the protected value.
    ///
    /// Takes `self` by value, so no locking is performed.
    pub fn into_inner(self) -> T {
        // Same reasoning as get_mut below, with a stronger premise:
        // taking `self` by value means the lock is being consumed, so
        // no reference to it can exist anywhere.
        //
        // This compiles only because Spinlock has no Drop impl -- you
        // cannot move a field out of a type that implements Drop. If
        // one is ever added (say, to debug_assert the lock is free on
        // destruction) this will need ManuallyDrop or ptr::read.
        self.data.into_inner()
    }
}

impl<T: ?Sized> Spinlock<T> {
    /// Acquires the lock, spinning until it becomes available.
    ///
    /// Returns a [`SpinlockGuard`] that releases the lock when
    /// dropped.
    ///
    /// The calling thread is never parked, so it occupies its core
    /// for as long as it waits. The lock is not reentrant:
    /// acquiring it from a thread that already holds a guard
    /// deadlocks.
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        // Test-and-test-and-set. The inner loop spins on a plain
        // relaxed load, which sits in the local cache in Shared
        // state and generates no coherence traffic. Only once the
        // lock looks free do we attempt the CAS, which is the
        // expensive part because it needs the line in Exclusive.
        // Spinning on the CAS directly would have every waiter
        // ping-ponging the line between cores.
        loop {
            loop {
                if !self.locked.load(Ordering::Relaxed) {
                    break;
                }

                spin_hint();
            }

            // The weak variant is right here precisely because we
            // are already inside a retry loop: on LL/SC machines
            // (ARM, RISC-V) it may fail spuriously, and one more
            // trip round a loop we were in anyway is cheaper than
            // the extra inner loop the strong variant compiles to.
            //
            // Acquire on success, so nothing in the critical
            // section can be reordered before the lock is taken,
            // and so we see the previous holder's Release store.
            // Relaxed on failure, because failing tells us nothing
            // worth ordering; we just go back to spinning.
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return SpinlockGuard {
                    lock: self,
                    _not_send: PhantomData,
                };
            }
        }
    }

    /// Returns a mutable reference to the protected value.
    ///
    /// Takes `&mut self`, which statically guarantees exclusive
    /// access, so no locking is performed.
    pub fn get_mut(&mut self) -> &mut T {
        // No atomics, no unsafe, no lock. `&mut self` is the compiler's
        // proof that no other reference to this Spinlock exists
        // anywhere in the program, so no other thread can be inside
        // lock() and there is nothing to exclude.
        //
        // The lock exists to recover exclusivity at RUNTIME when the
        // type system cannot prove it. Here it can, so we skip it.
        self.data.get_mut()
    }

    /// Attempts to acquire the lock without spinning.
    ///
    /// Makes exactly one attempt. Returns `Some(guard)` if the lock
    /// was free, or `None` if it was already held.
    ///
    /// # Examples
    ///
    /// ```
    /// use lockfree_rs::spinlock::Spinlock;
    ///
    /// let lock = Spinlock::new(());
    /// let guard = lock.lock();
    /// assert!(lock.try_lock().is_none());
    ///
    /// drop(guard);
    /// assert!(lock.try_lock().is_some());
    /// ```
    pub fn try_lock(&self) -> Option<SpinlockGuard<'_, T>> {
        // Strong, not weak. There is no retry loop to absorb a spurious
        // failure, so the weak variant would occasionally report an
        // uncontended lock as busy. Exactly the opposite call from the
        // one made in lock().
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinlockGuard {
                lock: self,
                _not_send: PhantomData,
            })
    }
}

// Sized (implicitly), because from() takes a T by value.
//
// This does not collide with core's blanket `impl<T> From<T> for
// T`, even though it looks like it should. Substituting
// T = Spinlock<U> here gives `From<Spinlock<U>> for
// Spinlock<Spinlock<U>>`, whose target type differs from the
// blanket impl's `From<Spinlock<U>> for Spinlock<U>`, so the two
// never overlap.
impl<T> From<T> for Spinlock<T> {
    fn from(data: T) -> Self {
        Self::new(data)
    }
}

// `T: Default` is a requirement in the ordinary direction, the
// opposite of ?Sized above: the body calls T::default(), so T must
// actually have it. The bound is the price of the body, not a
// property of the container.
impl<T: Default> Default for Spinlock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[cfg(test)]
mod test {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use crate::spinlock::{CACHE_LINE_ALIGN, Spinlock, SpinlockGuard, spin_hint};

    // Miri interprets every MIR statement and tracks the provenance
    // of every pointer, which costs somewhere around two orders of
    // magnitude. The concurrency tests below are tuned to run for a
    // few milliseconds natively; at full size they would run for
    // minutes each under Miri, so their iteration counts are divided
    // by this. Bug-finding power comes from interleaving, which Miri
    // explores per-scheduling-decision rather than per-iteration, so
    // the shorter runs lose much less than the ratio suggests -- and
    // `scripts/miri.sh --seeds` recovers the rest by re-running the
    // whole suite under many different schedules.
    //
    // 50 puts a full Miri pass at about 7 seconds on a 5600U. The
    // relationship is very sublinear -- 200 only brings that down to
    // 3.7s, while 20 pushes it up to 14s -- because a large part of
    // the cost is fixed setup rather than the loops themselves, so
    // dividing harder buys little and gives up interleavings.
    const SCALE: usize = if cfg!(miri) { 50 } else { 1 };

    fn threads() -> usize {
        // Under Miri, "threads" are interleaved on one interpreter,
        // so extra threads buy interleavings, not parallelism, and
        // each one costs real time. Three is enough for a handoff to
        // race against a third party. (available_parallelism reports
        // 1 under Miri anyway, unless -Zmiri-num-cpus says otherwise.)
        if cfg!(miri) {
            return 3;
        }

        std::thread::available_parallelism().map_or(4, |n| n.get().max(4))
    }

    /// The cache line size as the operating system reports it, or
    /// `None` where we have no dependency-free way to ask.
    ///
    /// Linux exposes the coherency line size of the L1 data cache in
    /// sysfs; macOS answers `sysctl hw.cachelinesize`. Neither is
    /// available to `repr(align(..))`, which needs a literal at
    /// compile time -- this exists only to check the constant we did
    /// compile in, in the one place a runtime value is actually
    /// usable: an assertion.
    fn platform_cache_line() -> Option<usize> {
        // Miri emulates neither sysfs nor sysctl, and shelling out is
        // not supported under isolation.
        if cfg!(miri) {
            return None;
        }

        if cfg!(target_os = "linux") {
            let path = "/sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size";
            return std::fs::read_to_string(path).ok()?.trim().parse().ok();
        }

        if cfg!(target_os = "macos") {
            let out = std::process::Command::new("sysctl")
                .args(["-n", "hw.cachelinesize"])
                .output()
                .ok()?;
            return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
        }

        None
    }

    // ---------------------------------------------------------------
    // Trait bounds
    //
    // These are compile-time assertions: the bodies are empty and the
    // whole point is whether the file builds. Each one pins down a
    // claim made in the comments above.
    // ---------------------------------------------------------------

    fn assert_send<T: Send + ?Sized>() {}
    fn assert_sync<T: Sync + ?Sized>() {}

    #[test]
    fn auto_trait_bounds() {
        assert_send::<Spinlock<u32>>();
        assert_sync::<Spinlock<u32>>();

        // The payoff of `T: Send` rather than `T: Send + Sync`: Cell
        // is Send but pointedly not Sync, and the lock is still Sync
        // because it never hands out two `&T` at once.
        assert_sync::<Spinlock<Cell<u32>>>();

        // Unsized payloads, which only work because ?Sized reaches
        // every impl and not just the struct definition.
        assert_sync::<Spinlock<[u8]>>();
        assert_sync::<Spinlock<dyn Adder + Send + Sync>>();

        // The guard is Sync when T is, restored by hand after
        // PhantomData<*const ()> removed it along with Send.
        assert_sync::<SpinlockGuard<'static, u32>>();

        // Not asserted here: that the guard is !Send, and that
        // Spinlock<Rc<u32>> is !Sync. Proving a negative needs a
        // compile-fail harness such as trybuild.
    }

    // ---------------------------------------------------------------
    // Layout
    // ---------------------------------------------------------------

    #[test]
    fn flag_does_not_share_a_cache_line_with_data() {
        let lock = Spinlock::new(0u8);

        assert_eq!(align_of::<Spinlock<u8>>(), CACHE_LINE_ALIGN);

        // The test module is a child of `spinlock`, so the private
        // fields are in scope. Both addresses are relative to a
        // CACHE_LINE_ALIGN-aligned base, so dividing by it gives the
        // line index.
        let flag = &lock.locked as *const _ as usize;
        let data = lock.data.get() as usize;
        assert_ne!(
            flag / CACHE_LINE_ALIGN,
            data / CACHE_LINE_ALIGN,
            "false sharing: same cache line"
        );
    }

    #[test]
    fn alignment_covers_the_platform_cache_line() {
        // The compile-time guess only has to be an over-estimate:
        // padding to more than a line still separates the two, while
        // padding to less puts them back on one. So the assertion is
        // >=, not ==, and on x86_64 it is expected to be strictly
        // greater (128 chosen against a 64-byte line, to defeat the
        // adjacent-line prefetcher).
        let Some(reported) = platform_cache_line() else {
            eprintln!("skipped: no cache line size available on this platform");
            return;
        };

        assert!(reported.is_power_of_two(), "implausible: {reported} bytes");
        assert!(
            CACHE_LINE_ALIGN >= reported,
            "under-aligned: padding to {CACHE_LINE_ALIGN} bytes but the CPU \
             reports {reported}-byte cache lines, so the flag and the data \
             can share a line"
        );
    }

    // ---------------------------------------------------------------
    // Single-threaded behaviour
    // ---------------------------------------------------------------

    #[test]
    fn guard_reads_and_writes_through() {
        let lock = Spinlock::new(vec![2, 3, 4]);

        let mut g = lock.lock();
        assert_eq!(*g, vec![2, 3, 4]);
        g.push(1);
        drop(g);

        assert_eq!(*lock.lock(), vec![2, 3, 4, 1]);
        assert_eq!(lock.into_inner(), vec![2, 3, 4, 1]);
    }

    #[test]
    fn assorted_sized_payloads() {
        assert_eq!(*Spinlock::new(()).lock(), ());
        assert_eq!(*Spinlock::new(u128::MAX).lock(), u128::MAX);
        assert_eq!(*Spinlock::new(String::from("hi")).lock(), "hi");
        assert_eq!(Spinlock::new([1u64; 32]).lock().len(), 32);
        assert_eq!(*Spinlock::new(Some(Box::new(7))).lock(), Some(Box::new(7)));

        let map = Spinlock::new(HashMap::new());
        map.lock().insert("k", 1);
        assert_eq!(map.lock()["k"], 1);

        // Non-Sync payload, legal because the lock serialises access.
        let cell = Spinlock::new(Cell::new(1u32));
        cell.lock().set(2);
        assert_eq!(cell.into_inner().get(), 2);
    }

    #[test]
    fn try_lock_reports_contention() {
        let lock = Spinlock::new(0u32);

        let guard = lock.lock();
        assert!(lock.try_lock().is_none(), "must fail while held");
        drop(guard);

        let guard = lock.try_lock().expect("must succeed once free");
        assert!(lock.try_lock().is_none(), "try_lock guard must exclude too");
        drop(guard);

        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn get_mut_bypasses_the_lock() {
        let mut lock = Spinlock::new(41);

        *lock.get_mut() += 1;
        assert_eq!(*lock.lock(), 42);

        // Taking &mut proves exclusivity statically, so the flag is
        // never touched and the lock is still free afterwards.
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn constructors_agree() {
        assert_eq!(*Spinlock::from(7u8).lock(), 7);
        assert_eq!(*Spinlock::<u8>::default().lock(), 0);
        assert_eq!(*Spinlock::<Vec<u8>>::default().lock(), Vec::new());
    }

    #[test]
    fn payload_dropped_exactly_once() {
        struct Tracked(Arc<AtomicUsize>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.fetch_add(1, SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));

        drop(Spinlock::new(Tracked(drops.clone())));
        assert_eq!(drops.load(SeqCst), 1, "dropping the lock must drop T");

        // into_inner moves T out; the lock's own storage must not
        // also drop it, or this reads 2.
        let recovered = Spinlock::new(Tracked(drops.clone())).into_inner();
        assert_eq!(drops.load(SeqCst), 1, "into_inner must not drop T");
        drop(recovered);
        assert_eq!(drops.load(SeqCst), 2);
    }

    #[test]
    fn unwinding_releases_the_lock() {
        let lock = Arc::new(Spinlock::new(0));
        let moved = lock.clone();

        // Silence the panic backtrace this deliberately provokes.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::thread::spawn(move || {
            *moved.lock() = 1;
            panic!("boom");
        })
        .join();
        std::panic::set_hook(hook);

        assert!(result.is_err(), "the thread was supposed to panic");
        // No poisoning: the guard's Drop still ran while unwinding,
        // so the lock is free and the write it made is visible.
        assert!(lock.try_lock().is_some(), "guard must release on unwind");
        assert_eq!(*lock.lock(), 1);
    }

    // ---------------------------------------------------------------
    // Concurrency
    //
    // A broken lock shows up here as a short total: two threads that
    // read the same value and write back value+1 lose an increment.
    // Non-atomic `u64` is the point, any serialisation failure is a
    // lost update rather than something the hardware papers over.
    // ---------------------------------------------------------------

    #[test]
    fn concurrent_increments_are_not_lost() {
        const ITERS: u64 = (20_000 / SCALE) as u64;
        let n = threads();
        let lock = Spinlock::new(0u64);

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        *lock.lock() += 1;
                    }
                });
            }
        });

        assert_eq!(lock.into_inner(), n as u64 * ITERS);
    }

    #[test]
    fn critical_sections_never_overlap() {
        const ITERS: usize = 2_000 / SCALE;
        let n = threads();
        let lock = Spinlock::new(0usize);
        let occupancy = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        let mut g = lock.lock();
                        // Directly observe how many threads are inside
                        // the section rather than inferring it from the
                        // final total.
                        let inside = occupancy.fetch_add(1, SeqCst) + 1;
                        peak.fetch_max(inside, SeqCst);
                        *g += 1;
                        occupancy.fetch_sub(1, SeqCst);
                    }
                });
            }
        });

        assert_eq!(peak.load(SeqCst), 1, "two threads were inside at once");
        assert_eq!(lock.into_inner(), n * ITERS);
    }

    #[test]
    fn writes_are_published_to_the_next_holder() {
        const ITERS: u64 = (20_000 / SCALE) as u64;
        let n = threads();
        // Two plain fields that every writer keeps equal. A reader
        // that sees them disagree saw a partially published section,
        // which is what a missing Acquire/Release pair causes. x86 is
        // too strongly ordered to fail this; it earns its keep on ARM
        // and under Miri.
        let lock = Spinlock::new((0u64, 0u64));

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        let mut g = lock.lock();
                        assert_eq!(g.0, g.1, "observed a torn critical section");
                        g.0 += 1;
                        g.1 += 1;
                    }
                });
            }
        });

        let (a, b) = lock.into_inner();
        assert_eq!(a, n as u64 * ITERS);
        assert_eq!(b, a);
    }

    #[test]
    fn try_lock_under_contention_loses_nothing() {
        const ITERS: u64 = (5_000 / SCALE) as u64;
        let n = threads();
        let lock = Spinlock::new(0u64);
        let retries = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        loop {
                            match lock.try_lock() {
                                Some(mut g) => {
                                    *g += 1;
                                    break;
                                }
                                None => {
                                    retries.fetch_add(1, SeqCst);
                                    spin_hint();
                                }
                            }
                        }
                    }
                });
            }
        });

        assert_eq!(lock.into_inner(), n as u64 * ITERS);
        // Not asserted: that retries > 0. It is contention-dependent
        // and would be flaky on a single-core runner.
        let _ = retries;
    }

    // ---------------------------------------------------------------
    // Unsized payloads
    // ---------------------------------------------------------------

    #[test]
    fn slice_payload() {
        let mut sized = Spinlock::new([0u8; 8]);

        {
            // Unsizing coercion: Spinlock<[u8; 8]> -> Spinlock<[u8]>.
            let unsized_ref: &Spinlock<[u8]> = &sized;

            assert_eq!(unsized_ref.lock().len(), 8);
            unsized_ref.lock()[0] = 1;
            assert!(unsized_ref.try_lock().is_some());
        }

        let unsized_mut: &mut Spinlock<[u8]> = &mut sized;
        unsized_mut.get_mut()[1] = 2;

        assert_eq!(sized.into_inner(), [1, 2, 0, 0, 0, 0, 0, 0]);
    }

    trait Adder {
        fn add(&mut self, n: u64);
        fn total(&self) -> u64;
    }

    struct Sum(u64);
    impl Adder for Sum {
        fn add(&mut self, n: u64) {
            self.0 += n;
        }
        fn total(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn trait_object_payload() {
        let concrete = Spinlock::new(Sum(0));
        let object: &Spinlock<dyn Adder + Send + Sync> = &concrete;

        object.lock().add(20);
        object.lock().add(22);

        assert_eq!(object.lock().total(), 42);
        assert_eq!(concrete.into_inner().0, 42);
    }

    #[test]
    fn shared_unsized_payload_across_threads() {
        const ITERS: u64 = (5_000 / SCALE) as u64;
        let n = threads();
        // Arc coerces too, so the unsized lock can be shared rather
        // than merely borrowed. This is the case that fails to
        // compile if `unsafe impl Sync` forgets its ?Sized.
        let lock: Arc<Spinlock<[u64]>> = Arc::new(Spinlock::new([0u64; 4]));

        std::thread::scope(|s| {
            for _ in 0..n {
                let lock = Arc::clone(&lock);
                s.spawn(move || {
                    for _ in 0..ITERS {
                        let mut g = lock.lock();
                        for slot in g.iter_mut() {
                            *slot += 1;
                        }
                    }
                });
            }
        });

        let expected = n as u64 * ITERS;
        assert_eq!(*lock.lock(), [expected; 4]);
    }
}

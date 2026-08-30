//! An MCS spinlock: mutual exclusion by busy-waiting on a queue.
//!
//! [`Spinlock`](crate::spinlock::Spinlock) is a test-and-test-and-set
//! lock: every waiter spins on the same flag. That is cheap when the
//! lock is free and increasingly expensive when it is not. Releasing
//! writes one cache line that all N waiters hold in Shared state, so
//! all N are invalidated, all N re-read it, all N attempt the CAS and
//! N-1 lose. The cost of a handoff grows with the number of waiters,
//! and the winner is whichever core got the line back first, which is
//! an ordering the hardware picks and not one the program can rely on
//! -- a distant thread can lose repeatedly and starve.
//!
//! Mellor-Crummey and Scott (1991) fix both by making the waiters an
//! explicit FIFO queue. Each waiter contributes a [`Node`] and spins
//! on a flag *inside its own node*, so a release writes one flag in
//! one node: one cache line moves, one thread wakes, and the cost of
//! a handoff no longer depends on how many others are waiting.
//! Arrival order becomes service order.
//!
//! # The protocol
//!
//! The lock itself is a single pointer, `tail`, null when free.
//!
//! To acquire, a thread initialises a node (`locked = true`,
//! `next = null`) and atomically **swaps** it into `tail`, receiving
//! whatever was there before -- its predecessor.
//!
//! * Predecessor null: the queue was empty, the thread has the lock,
//!   and it never spins. Its `locked` flag is never read by anyone.
//! * Predecessor non-null: the thread publishes itself by storing its
//!   node into `pred.next`, then spins on its own `locked` until the
//!   predecessor clears it.
//!
//! ```text
//! tail ──────────────────────────────┐
//!                                    ▼
//!  [A: holds] ──next──> [B: spins] ──next──> [C: spins]
//! ```
//!
//! To release, the holder looks at its own `next`:
//!
//! * Non-null: store `false` into `next.locked`. That is the entire
//!   handoff -- one release store, one line transferred, one thread
//!   woken.
//! * Null: there may still be no successor, so try to CAS `tail` from
//!   "my node" back to null. If that succeeds the queue really is
//!   empty and the lock is now free.
//!
//! The interesting case is that CAS *failing*. It means somebody has
//! already swapped themselves into `tail` but has not yet reached
//! their store to `pred.next`, so they are our successor and they are
//! not yet visible to us. The releasing thread has no choice but to
//! spin until `next` appears and then hand off to it. That
//! swap-then-link window is the one place MCS waits on another
//! thread's progress, and it is why bolting a timeout onto MCS is a
//! research paper rather than a patch: leaving the queue early means
//! unlinking from a singly-linked list whose predecessor may be
//! mid-write.
//!
//! # Where the nodes come from
//!
//! A queued node's address is stored in another thread's `next`
//! field, so it must not move and must not be destroyed while it is
//! in the queue. The textbook C interface makes that the caller's
//! problem -- `mcs_lock(&lock, &my_node)`, node on the caller's
//! stack -- but that would force an API like
//! `lock(&self, node: Pin<&mut Node>)` here, which is a poor trade
//! against `Spinlock`'s `lock(&self)`.
//!
//! Instead each thread keeps a small pool of nodes and the guard
//! carries the one it used. Linux's qspinlock does the same thing
//! with a fixed per-CPU array of four, indexed by a nesting counter,
//! and can prove four is enough: preemption is off inside the
//! critical section, so the only way to nest is an interrupt, and
//! there are exactly four interrupt contexts (task, softirq, hardirq,
//! NMI) which unwind in LIFO order.
//!
//! None of those three premises survive in userspace. Nesting here
//! means "hold lock A, take lock B", which is ordinary lock
//! composition and has no fixed bound; and Rust lets guards be
//! dropped in any order, so a depth counter decremented on release
//! would hand back the wrong node the first time somebody writes
//! `drop(outer)` before `drop(inner)`. So the pool is a free list
//! rather than an array, and the node is identified by the pointer in
//! the guard rather than by a depth index -- see [`take_node`].
//!
//! # Cost
//!
//! Uncontended, this is strictly more work than
//! [`Spinlock`](crate::spinlock::Spinlock): a swap plus a store to
//! acquire and a load plus a CAS to release, against one CAS and one
//! store. MCS wins only once there are enough waiters
//! for O(1) handoff to beat O(N) invalidation, which is why
//! production designs (qspinlock, Java's `ObjectMonitor`) put a
//! test-and-set fast path in front of the queue and only fall into it
//! under contention.
//!
//! The FIFO discipline is a guarantee, not a heuristic: there is no
//! barging, so a thread that joins the queue is served after exactly
//! the threads already in it. That is the fairness win, and also the
//! throughput risk -- if the next thread in line is descheduled, the
//! whole queue waits behind it, where a TTAS lock would have handed
//! the lock to somebody runnable.

use std::cell::{Cell, UnsafeCell};
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::cache::Aligned;
use crate::spin::spin_hint;

/// One waiter's slot in a lock's queue.
///
/// Deliberately not generic in the payload: nodes are handed out from
/// a per-thread pool shared by every `McsSpinlock` in the program, so
/// there has to be exactly one node type.
///
/// `repr(C)` to pin the field order the layout below depends on.
#[repr(C)]
struct Node {
    /// Cleared by our predecessor to hand us the lock.
    ///
    /// This is the only field we spin on, so it gets a cache line to
    /// itself: our successor writes `next` while we are spinning
    /// here, and sharing a line with it would put us back to taking
    /// an invalidation for somebody else's traffic -- the exact cost
    /// this whole algorithm exists to avoid.
    locked: Aligned<AtomicBool>,

    /// Our successor's node, once it has published itself.
    ///
    /// Written by the successor, read by us on release.
    next: AtomicPtr<Node>,

    /// Free-list link, valid only while the node is *not* queued.
    ///
    /// A plain `Cell`, not an atomic, because it is only ever touched
    /// by the thread that owns the pool, and only in the window
    /// between the node coming off the queue and going back onto it.
    /// It shares a line with `next` on purpose: the two are never
    /// live at the same time.
    pool_next: Cell<*mut Node>,
}

impl Node {
    const fn new() -> Self {
        Self {
            locked: Aligned(AtomicBool::new(true)),
            next: AtomicPtr::new(ptr::null_mut()),
            pool_next: Cell::new(ptr::null_mut()),
        }
    }
}

/// This thread's free list of queue nodes.
///
/// Nodes are individually boxed, so the list can grow to any nesting
/// depth without moving the nodes already in it. A `Vec<Node>` would
/// be a use-after-free waiting to happen: growing it during a nested
/// acquire reallocates, every queued node moves, and the pointers
/// other threads stored in their `next` fields all dangle.
struct Pool {
    head: Cell<*mut Node>,
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Freeing at thread exit rather than leaking keeps Miri's
        // leak checker useful for the rest of the suite. It is sound
        // because a node only reaches the free list once `release`
        // has returned for it, at which point no other thread can
        // still reach it -- the argument is spelled out on
        // `McsSpinlock::release`. A node that is still queued (the
        // caller leaked its guard) is not on this list and is not
        // touched here.
        let mut node = self.head.replace(ptr::null_mut());

        while !node.is_null() {
            // SAFETY: every node on this list came from
            // `Box::into_raw` on this thread and is not queued.
            let next = unsafe { (*node).pool_next.get() };
            drop(unsafe { Box::from_raw(node) });
            node = next;
        }
    }
}

thread_local! {
    // `const` initialiser so the access compiles to a direct
    // thread-local load rather than a lazy-initialisation check.
    // Registering the destructor above still costs something on the
    // first access of each thread; that is the price of not leaking.
    static POOL: Pool = const { Pool { head: Cell::new(ptr::null_mut()) } };
}

/// Takes a node off this thread's free list, or allocates one.
///
/// The returned node is initialised for a fresh acquisition and is
/// owned by the caller until it is handed back to [`return_node`].
fn take_node() -> *mut Node {
    let recycled = POOL
        .try_with(|pool| {
            let node = pool.head.get();

            if !node.is_null() {
                // SAFETY: nodes on the free list were allocated by
                // this thread and are not queued on any lock, so
                // nothing else can be reading them.
                pool.head.set(unsafe { (*node).pool_next.get() });
            }

            node
        })
        // The pool's destructor has already run for this thread. That
        // only happens when a lock is taken from inside another
        // thread-local's destructor; allocate a one-off node, which
        // `return_node` will free rather than recycle.
        .unwrap_or(ptr::null_mut());

    let node = if recycled.is_null() {
        Box::into_raw(Box::new(Node::new()))
    } else {
        recycled
    };

    // SAFETY: `node` was either just allocated or just taken off this
    // thread's free list. Either way we are its only owner and it is
    // not linked into any queue.
    //
    // Relaxed is enough for all three: the swap that publishes this
    // pointer in `lock` is Release, which orders every one of these
    // writes ahead of the moment any other thread can observe the
    // node's address.
    unsafe {
        (*node).locked.store(true, Ordering::Relaxed);
        (*node).next.store(ptr::null_mut(), Ordering::Relaxed);
        (*node).pool_next.set(ptr::null_mut());
    }

    node
}

/// Returns a node to this thread's free list.
///
/// # Safety
///
/// `node` must have come from [`take_node`] on this thread, and
/// `McsSpinlock::release` must have returned for it, so that no other
/// thread can still reach it.
unsafe fn return_node(node: *mut Node) {
    let stored = POOL.try_with(|pool| {
        // SAFETY: by this function's contract the node is off the
        // queue and unreachable from any other thread.
        unsafe { (*node).pool_next.set(pool.head.get()) };
        pool.head.set(node);
    });

    if stored.is_err() {
        // No list left to put it on -- see `take_node`. Free it here
        // instead of leaking it.
        //
        // SAFETY: same contract, and the node came from Box::into_raw.
        drop(unsafe { Box::from_raw(node) });
    }
}

/// A mutual-exclusion lock that busy-waits on a FIFO queue of
/// waiters.
///
/// Access the protected value through [`McsSpinlock::lock`] or
/// [`McsSpinlock::try_lock`], which return a guard; the lock is
/// released when that guard is dropped.
///
/// Unlike [`Spinlock`](crate::spinlock::Spinlock), waiters spin on
/// per-thread state rather than on a shared flag, so a handoff costs
/// one cache line transfer no matter how many threads are waiting,
/// and the lock is granted in the order it was requested. There is no
/// poisoning: a panic while a guard is alive releases the lock and
/// leaves the value as it was.
///
/// Not reentrant. Acquiring it from a thread that already holds it
/// deadlocks -- and here the thread deadlocks against *itself* in the
/// queue, since its second node waits on a flag only its first node's
/// release will clear.
///
/// # Examples
///
/// ```
/// use spinlock_rs::mcs_spinlock::McsSpinlock;
///
/// let lock = McsSpinlock::new(0u32);
/// *lock.lock() += 1;
/// assert_eq!(lock.into_inner(), 1);
/// ```
#[derive(Debug)]
pub struct McsSpinlock<T: ?Sized> {
    /// The most recently enqueued node, or null when no thread holds
    /// or wants the lock.
    ///
    /// Padded for the same reason `Spinlock`'s flag is: this word is
    /// hit by an atomic swap on every single acquisition, and must
    /// not share a line with the data it protects or with a
    /// neighbouring lock.
    tail: Aligned<AtomicPtr<Node>>,

    // UnsafeCell is not Sync, but we want McsSpinlock<T> to be Sync.
    // See the impl below.
    data: UnsafeCell<T>,
}

// SAFETY: identical reasoning to `Spinlock`. `T: Send` is required
// because a value locked on one thread may be dropped on another;
// `T: Sync` is not, because the lock's entire job is to ensure only
// one `&T` exists at a time.
//
// `AtomicPtr<Node>` is unconditionally Send and Sync, so `Send`
// derives structurally for `McsSpinlock<T>` whenever `T: Send` and
// only `Sync` needs asserting by hand. Note that this says nothing
// about `Node` -- nodes are never reached through a `&McsSpinlock`,
// only through raw pointers.
unsafe impl<T: Send + ?Sized> Sync for McsSpinlock<T> {}

/// RAII guard granting access to the value protected by an
/// [`McsSpinlock`].
///
/// Returned by [`McsSpinlock::lock`] and [`McsSpinlock::try_lock`].
/// Dereferences to the protected value, and releases the lock when
/// dropped.
///
/// Deliberately not [`Send`]: the lock must be released by the thread
/// that acquired it, and more concretely the node inside must go back
/// to the pool it came from.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct McsSpinlockGuard<'a, T: ?Sized> {
    lock: &'a McsSpinlock<T>,

    /// The node this thread queued to get here.
    ///
    /// Carrying the node rather than a depth index is what makes
    /// out-of-order guard drops work; see the module docs.
    ///
    /// It also removes the need for `SpinlockGuard`'s
    /// `PhantomData<*const ()>`: a raw pointer field is neither Send
    /// nor Sync, so this field already subtracts both auto traits.
    /// Sync is the one we wanted back, restored immediately below.
    node: *mut Node,
}

// SAFETY: restores the Sync that the raw pointer field took away,
// since we only ever wanted !Send. Sound because sharing
// `&McsSpinlockGuard` only yields `&T` (DerefMut needs `&mut self`),
// and `T: Sync` is exactly the promise that concurrent `&T` is safe.
// Sharing a `&` to the guard gives no access to `node` at all.
unsafe impl<T: ?Sized + Sync> Sync for McsSpinlockGuard<'_, T> {}

impl<T: ?Sized> Deref for McsSpinlockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: since the guard exists we must be the only holder
        // of the lock, so no other thread can have a reference to the
        // data.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for McsSpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: since the guard exists we must be the only holder
        // of the lock, so no other thread can have a reference to the
        // data.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for McsSpinlockGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: the guard is the unique owner of `node`, which has
        // been queued on this lock since `lock` returned and has not
        // been released yet -- dropping the guard is the only thing
        // that releases it.
        unsafe { self.lock.release(self.node) };

        // SAFETY: `release` has returned, so the node is off the
        // queue and unreachable from any other thread.
        unsafe { return_node(self.node) };
    }
}

// Sized-only impl, for the same reason as `Spinlock`'s: both methods
// move a T across the function boundary.
impl<T> McsSpinlock<T> {
    /// Creates a new `McsSpinlock`, unlocked, wrapping `data`.
    pub fn new(data: T) -> Self {
        Self {
            tail: Aligned(AtomicPtr::new(ptr::null_mut())),
            data: UnsafeCell::new(data),
        }
    }

    /// Consumes the lock and returns the protected value.
    ///
    /// Takes `self` by value, so no locking is performed.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> McsSpinlock<T> {
    /// Acquires the lock, spinning until it is our turn.
    ///
    /// Returns an [`McsSpinlockGuard`] that releases the lock when
    /// dropped.
    ///
    /// The lock is granted in the order it was requested, so a thread
    /// waits for exactly the threads that were already queued ahead
    /// of it and no others. The calling thread is never parked, so it
    /// occupies its core for as long as it waits.
    pub fn lock(&self) -> McsSpinlockGuard<'_, T> {
        let node = take_node();

        // The swap is what makes the queue work: it publishes our
        // node and reads our predecessor in one indivisible step, so
        // two threads arriving together cannot both believe they are
        // at the head.
        //
        // Acquire is the load-bearing half. When `pred` comes back
        // null we have taken the lock directly, and the null we read
        // was written by the previous holder's release CAS below --
        // Acquire here against Release there is what publishes that
        // holder's critical section to us.
        //
        // Release is the other half of the invariant "any node
        // reachable from `tail` is fully initialised": it orders the
        // relaxed writes in `take_node` ahead of the instant our
        // address becomes visible to anyone.
        let pred = self.tail.swap(node, Ordering::AcqRel);

        if !pred.is_null() {
            // SAFETY: `pred` was in `tail`, so its owner is queued
            // ahead of us. It cannot recycle the node until it has
            // handed the lock to us, precisely because `release`
            // refuses to return while its `next` is null and its CAS
            // has failed -- which is the case it is now in.
            //
            // Release, so the predecessor's Acquire load of `next`
            // sees a fully initialised node when it comes to clear
            // our flag.
            unsafe { (*pred).next.store(node, Ordering::Release) };

            // Spin on OUR OWN flag. No other waiter reads this line,
            // and only our predecessor ever writes it, so this loop
            // generates no coherence traffic at all until the moment
            // the lock is handed over.
            //
            // SAFETY: `node` is ours for the duration.
            while unsafe { (*node).locked.load(Ordering::Acquire) } {
                spin_hint();
            }
        }

        McsSpinlockGuard { lock: self, node }
    }

    /// Attempts to acquire the lock without queueing.
    ///
    /// Makes exactly one attempt, which succeeds only if no thread
    /// holds the lock *or* is waiting for it. Returns `Some(guard)`
    /// on success and `None` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use spinlock_rs::mcs_spinlock::McsSpinlock;
    ///
    /// let lock = McsSpinlock::new(());
    /// let guard = lock.lock();
    /// assert!(lock.try_lock().is_none());
    ///
    /// drop(guard);
    /// assert!(lock.try_lock().is_some());
    /// ```
    pub fn try_lock(&self) -> Option<McsSpinlockGuard<'_, T>> {
        let node = take_node();

        // Strong, not weak: there is no retry loop here to absorb a
        // spurious failure, and reporting a free lock as busy would
        // be a lie. Same call as `Spinlock::try_lock` makes.
        //
        // An empty queue is the only state in which nobody holds the
        // lock: the tail keeps pointing at the holder until it
        // releases, so this correctly fails while the lock is held
        // even though the holder never spins.
        if self
            .tail
            .compare_exchange(ptr::null_mut(), node, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Some(McsSpinlockGuard { lock: self, node });
        }

        // SAFETY: the CAS failed, so the node was never published and
        // no other thread has ever seen it.
        unsafe { return_node(node) };

        None
    }

    /// Returns a mutable reference to the protected value.
    ///
    /// Takes `&mut self`, which statically guarantees exclusive
    /// access, so no locking is performed.
    pub fn get_mut(&mut self) -> &mut T {
        // `&mut self` is the compiler's proof that no other reference
        // to this lock exists anywhere in the program, so no other
        // thread can be inside `lock` and there is nothing to
        // exclude.
        self.data.get_mut()
    }

    /// Releases the lock held via `node`, handing it to the next
    /// thread in the queue if there is one.
    ///
    /// # Safety
    ///
    /// `node` must be a node this thread queued on `self` and has not
    /// yet released.
    ///
    /// On return, no other thread can reach `node`, so the caller may
    /// recycle it. That holds in both exit paths:
    ///
    /// * The CAS succeeded, so `tail` no longer contains `node` and
    ///   no thread swapped in between our read and our CAS -- nobody
    ///   ever obtained the address.
    /// * We handed off, which means we saw a successor's `next`
    ///   store. The successor had already overwritten `tail` with its
    ///   own node *before* making that store, so no later thread can
    ///   receive `node` as its predecessor, and the successor itself
    ///   is finished writing to it.
    unsafe fn release(&self, node: *mut Node) {
        // SAFETY: `node` is ours and still queued.
        let mut next = unsafe { (*node).next.load(Ordering::Acquire) };

        if next.is_null() {
            // No successor visible. Either there genuinely is none,
            // in which case this CAS empties the queue and we are
            // done, or one is in the swap-then-link window.
            //
            // Release on success publishes our critical section to
            // whichever thread next reads this null out of `tail`
            // with its Acquire swap. Relaxed on failure: failing
            // tells us only that we have a successor, and the
            // Acquire load below is what orders us against it.
            if self
                .tail
                .compare_exchange(node, ptr::null_mut(), Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }

            // The CAS failed, so a successor has already claimed the
            // tail but has not yet stored itself into our `next`. It
            // is committed to doing so and cannot be stopped, so the
            // only correct thing to do is wait for it. This is the
            // single point where MCS blocks on another thread's
            // progress.
            loop {
                // SAFETY: as above.
                next = unsafe { (*node).next.load(Ordering::Acquire) };

                if !next.is_null() {
                    break;
                }

                spin_hint();
            }
        }

        // The handoff. Release pairs with the successor's Acquire
        // load in its spin loop, so everything we wrote inside the
        // critical section happens-before it observes the lock as
        // granted.
        //
        // SAFETY: `next` is our successor's node, which it cannot
        // recycle until we clear this flag.
        unsafe { (*next).locked.store(false, Ordering::Release) };
    }
}

// Does not collide with core's blanket `impl<T> From<T> for T`, for
// the same reason `Spinlock`'s doesn't: substituting
// T = McsSpinlock<U> gives a different target type.
impl<T> From<T> for McsSpinlock<T> {
    fn from(data: T) -> Self {
        Self::new(data)
    }
}

impl<T: Default> Default for McsSpinlock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[cfg(test)]
mod test {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use crate::cache::CACHE_LINE_ALIGN;
    use crate::mcs_spinlock::{McsSpinlock, McsSpinlockGuard, Node, POOL};
    use crate::spin::spin_hint;

    // See the note on the same constant in `spinlock`: Miri costs
    // roughly two orders of magnitude, and bug-finding power here
    // comes from interleavings rather than iterations.
    const SCALE: usize = if cfg!(miri) { 50 } else { 1 };

    /// The thread count for the concurrency tests.
    ///
    /// A fixed small number, where the `spinlock` suite scales with
    /// `available_parallelism`, and the difference is the algorithm
    /// showing through rather than an arbitrary choice.
    ///
    /// `cargo test` runs its tests in parallel, so several of these
    /// are live at once and the machine is oversubscribed. A TTAS
    /// lock shrugs that off: it barges, so a descheduled waiter is
    /// simply overtaken by a runnable one. MCS cannot -- the queue is
    /// strict FIFO, so when the thread at the head of it loses its
    /// timeslice, every thread behind it waits out a full scheduler
    /// quantum for a handoff that would otherwise take a hundred
    /// nanoseconds.
    ///
    /// It is not a small effect. Measured on a 12-core box, running
    /// the suite with `available_parallelism()` threads here took 64
    /// seconds; this line takes it to 0.09. The same test in
    /// isolation, with the machine to itself, runs in 0.03 either
    /// way, which is the tell that this is scheduling and not the
    /// lock being slow.
    ///
    /// Worth knowing before reading benches/spinlock.rs: it is the
    /// same trap, and the reason the throughput numbers there are
    /// only meaningful at or below the core count.
    fn threads() -> usize {
        if cfg!(miri) {
            return 3;
        }

        4
    }

    /// How many nodes are sitting on this thread's free list.
    ///
    /// Only meaningful on a thread that is not currently holding any
    /// `McsSpinlock`, since a node in use is off the list.
    fn pool_len() -> usize {
        POOL.with(|pool| {
            let mut n = 0;
            let mut node = pool.head.get();

            while !node.is_null() {
                n += 1;
                // SAFETY: the list is owned by this thread and every
                // node on it is off the queue.
                node = unsafe { (*node).pool_next.get() };
            }

            n
        })
    }

    // ---------------------------------------------------------------
    // Trait bounds
    // ---------------------------------------------------------------

    fn assert_send<T: Send + ?Sized>() {}
    fn assert_sync<T: Sync + ?Sized>() {}

    #[test]
    fn auto_trait_bounds() {
        assert_send::<McsSpinlock<u32>>();
        assert_sync::<McsSpinlock<u32>>();

        // `T: Send` rather than `T: Send + Sync`: Cell is Send but
        // not Sync, and the lock is still Sync because it never hands
        // out two `&T` at once.
        assert_sync::<McsSpinlock<Cell<u32>>>();

        // Unsized payloads, which need ?Sized on every impl and not
        // just the struct definition.
        assert_sync::<McsSpinlock<[u8]>>();
        assert_sync::<McsSpinlock<dyn Adder + Send + Sync>>();

        // The guard is Sync when T is, restored by hand after the raw
        // `*mut Node` field removed it along with Send.
        assert_sync::<McsSpinlockGuard<'static, u32>>();

        // Not asserted: that the guard is !Send. Proving a negative
        // needs a compile-fail harness such as trybuild.
    }

    // ---------------------------------------------------------------
    // Layout
    // ---------------------------------------------------------------

    #[test]
    fn tail_does_not_share_a_cache_line_with_data() {
        let lock = McsSpinlock::new(0u8);

        assert_eq!(align_of::<McsSpinlock<u8>>(), CACHE_LINE_ALIGN);

        let tail = &lock.tail as *const _ as usize;
        let data = lock.data.get() as usize;
        assert_ne!(
            tail / CACHE_LINE_ALIGN,
            data / CACHE_LINE_ALIGN,
            "false sharing: same cache line"
        );
    }

    #[test]
    fn node_flag_does_not_share_a_cache_line_with_its_link() {
        // The flag a waiter spins on and the link its successor
        // writes must be on different lines, or the successor's one
        // store knocks the waiter's line out from under it on every
        // enqueue -- reintroducing exactly the traffic the queue
        // exists to remove.
        let node = Node::new();

        assert_eq!(align_of::<Node>(), CACHE_LINE_ALIGN);

        let locked = &node.locked as *const _ as usize;
        let next = &node.next as *const _ as usize;
        assert_ne!(
            locked / CACHE_LINE_ALIGN,
            next / CACHE_LINE_ALIGN,
            "false sharing between a waiter's flag and its link"
        );
    }

    // ---------------------------------------------------------------
    // Single-threaded behaviour
    // ---------------------------------------------------------------

    #[test]
    fn uncontended_round_trip() {
        let lock = McsSpinlock::new(0u64);

        for i in 1..=100u64 {
            *lock.lock() += i;
        }

        assert_eq!(lock.into_inner(), 5_050);
    }

    #[test]
    fn try_lock_reflects_whether_the_queue_is_empty() {
        let lock = McsSpinlock::new(0u32);

        let guard = lock.try_lock().expect("free lock");
        assert!(lock.try_lock().is_none(), "held lock reported as free");

        drop(guard);
        assert!(lock.try_lock().is_some(), "released lock reported as held");
    }

    #[test]
    fn get_mut_bypasses_the_queue() {
        let mut lock = McsSpinlock::new(vec![1, 2, 3]);
        lock.get_mut().push(4);

        assert_eq!(lock.into_inner(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn constructors_agree() {
        assert_eq!(*McsSpinlock::from(7u8).lock(), 7);
        assert_eq!(*McsSpinlock::<u8>::default().lock(), 0);
        assert_eq!(*McsSpinlock::<Vec<u8>>::default().lock(), Vec::new());
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

        let lock = McsSpinlock::new(Tracked(drops.clone()));
        drop(lock.lock());
        assert_eq!(drops.load(SeqCst), 0, "a guard must not drop the payload");

        drop(lock);
        assert_eq!(drops.load(SeqCst), 1);
    }

    #[test]
    fn unwinding_releases_the_lock() {
        let lock = Arc::new(McsSpinlock::new(0));
        let moved = lock.clone();

        let panicked = std::thread::spawn(move || {
            let mut guard = moved.lock();
            *guard = 1;
            panic!("inside the critical section");
        })
        .join();

        assert!(panicked.is_err());

        // The guard was dropped by the unwind, so the node went back
        // to that thread's pool and the queue is empty again. Without
        // poisoning the value is simply whatever the panicking thread
        // left behind.
        assert_eq!(*lock.lock(), 1);
    }

    // ---------------------------------------------------------------
    // The node pool
    // ---------------------------------------------------------------

    #[test]
    fn guards_may_be_dropped_out_of_order() {
        // The case a qspinlock-style depth counter gets wrong: the
        // outer guard is released first, so the "stack" of nodes
        // unwinds from the wrong end. Carrying the node in the guard
        // makes the order irrelevant.
        let outer = McsSpinlock::new(0);
        let inner = McsSpinlock::new(0);

        let outer_guard = outer.lock();
        let inner_guard = inner.lock();

        drop(outer_guard);
        assert!(outer.try_lock().is_some(), "outer lock still held");
        assert!(inner.try_lock().is_none(), "inner lock released early");

        drop(inner_guard);
        assert!(inner.try_lock().is_some(), "inner lock still held");
    }

    #[test]
    fn the_pool_recycles_nodes_rather_than_growing() {
        let a = McsSpinlock::new(0);
        let b = McsSpinlock::new(0);
        let c = McsSpinlock::new(0);

        // A fresh thread, so the pool starts empty no matter what
        // else the test binary has run on this one.
        std::thread::spawn(move || {
            assert_eq!(pool_len(), 0, "a new thread starts with no nodes");

            {
                let _a = a.lock();
                let _b = b.lock();
                let _c = c.lock();

                // All three nodes are in use, none on the list.
                assert_eq!(pool_len(), 0);
            }

            assert_eq!(pool_len(), 3, "nodes come back on release");

            for _ in 0..100 {
                let _guard = a.lock();
            }

            assert_eq!(
                pool_len(),
                3,
                "steady state: acquisitions reuse nodes instead of allocating"
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn locking_survives_the_pool_being_destroyed() {
        // The awkward corner of putting the nodes in a thread-local:
        // another thread-local's destructor can take a lock, and by
        // then the pool may already be gone. Thread-local
        // destructors run in reverse order of registration, so
        // touching LATE before taking any lock puts the pool's
        // destructor ahead of LATE's in the queue -- and LATE then
        // asks for a node from a pool that no longer exists.
        //
        // `take_node` answers with a one-off allocation and
        // `return_node` frees it on the spot rather than pushing it
        // onto a list that has been torn down. The test asserts the
        // lock still works from there; that the allocation is
        // balanced is Miri's leak checker to confirm.
        //
        // Registration order is not something the language promises,
        // so on a platform that ordered these the other way this
        // would simply exercise the ordinary path instead. It is
        // written to be correct either way.
        struct TakesALockWhenDropped(Arc<McsSpinlock<u64>>);

        impl Drop for TakesALockWhenDropped {
            fn drop(&mut self) {
                *self.0.lock() += 1;
            }
        }

        thread_local! {
            static LATE: RefCell<Option<TakesALockWhenDropped>> =
                const { RefCell::new(None) };
        }

        let lock = Arc::new(McsSpinlock::new(0u64));
        let moved = lock.clone();

        std::thread::spawn(move || {
            // Registers LATE's destructor, and nothing else yet.
            LATE.with(|slot| {
                *slot.borrow_mut() = Some(TakesALockWhenDropped(moved.clone()));
            });

            // Registers the pool's destructor, i.e. after LATE's, so
            // it runs before it.
            drop(moved.lock());
        })
        .join()
        .unwrap();

        assert_eq!(*lock.lock(), 1, "the destructor's increment was lost");
    }

    // ---------------------------------------------------------------
    // Concurrency
    //
    // The payload is a non-atomic integer on purpose: a failure to
    // serialise shows up as a lost update rather than as something
    // the hardware papers over.
    // ---------------------------------------------------------------

    #[test]
    fn concurrent_increments_are_not_lost() {
        const ITERS: u64 = (20_000 / SCALE) as u64;

        let n = threads();
        let lock = McsSpinlock::new(0u64);

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
        let inside = AtomicUsize::new(0);
        let lock = McsSpinlock::new(());

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        let _guard = lock.lock();

                        assert_eq!(
                            inside.fetch_add(1, SeqCst),
                            0,
                            "two threads inside the critical section"
                        );

                        // Widen the window a real overlap would land in.
                        for _ in 0..16 {
                            spin_hint();
                        }

                        assert_eq!(inside.fetch_sub(1, SeqCst), 1);
                    }
                });
            }
        });
    }

    #[test]
    fn writes_are_published_to_the_next_holder() {
        // Every holder writes a value derived from what it read, so a
        // stale read (a missing Acquire on the handoff, or a missing
        // Release on the release) breaks the chain and the final
        // count comes out short.
        const ITERS: u64 = (20_000 / SCALE) as u64;

        let n = threads();
        let lock = McsSpinlock::new((0u64, 0u64));

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        let mut guard = lock.lock();
                        let (seen, count) = *guard;
                        *guard = (seen + 1, count + 1);
                        assert_eq!(guard.0, guard.1);
                    }
                });
            }
        });

        assert_eq!(lock.into_inner(), (n as u64 * ITERS, n as u64 * ITERS));
    }

    #[test]
    fn try_lock_under_contention_loses_nothing() {
        const ITERS: u64 = (5_000 / SCALE) as u64;

        let n = threads();
        let taken = AtomicUsize::new(0);
        let lock = McsSpinlock::new(0u64);

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        if let Some(mut guard) = lock.try_lock() {
                            *guard += 1;
                            taken.fetch_add(1, SeqCst);
                        }
                    }
                });
            }
        });

        // Whatever the success rate was, every success must have
        // landed: the counter and the tally agree.
        assert_eq!(lock.into_inner(), taken.load(SeqCst) as u64);
    }

    #[test]
    fn nested_locks_across_threads() {
        // Exercises the pool under contention at depth: every thread
        // holds one lock while queueing for another, so nodes are in
        // flight on two queues at once.
        const ITERS: u64 = (2_000 / SCALE) as u64;

        let n = threads();
        let outer = McsSpinlock::new(0u64);
        let inner = McsSpinlock::new(0u64);

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        let mut a = outer.lock();
                        let mut b = inner.lock();
                        *a += 1;
                        *b += 1;
                    }
                });
            }
        });

        assert_eq!(outer.into_inner(), n as u64 * ITERS);
        assert_eq!(inner.into_inner(), n as u64 * ITERS);
    }

    // ---------------------------------------------------------------
    // Unsized payloads
    // ---------------------------------------------------------------

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
    fn slice_payload() {
        let mut sized = McsSpinlock::new([0u8; 8]);

        // Unsizing coercion: the lock is built around a fixed-size
        // array and then used through a `[u8]` view of itself.
        let unsized_view: &McsSpinlock<[u8]> = &sized;
        unsized_view.lock()[3] = 42;

        assert_eq!(unsized_view.lock().len(), 8);
        assert_eq!(sized.get_mut()[3], 42);
    }

    #[test]
    fn trait_object_payload() {
        let concrete = McsSpinlock::new(Sum(0));
        let object: &McsSpinlock<dyn Adder + Send + Sync> = &concrete;

        object.lock().add(7);
        assert_eq!(object.lock().total(), 7);
    }

    #[test]
    fn shared_unsized_payload_across_threads() {
        const ITERS: u64 = (5_000 / SCALE) as u64;

        let n = threads();
        let concrete = McsSpinlock::new(Sum(0));
        let object: &McsSpinlock<dyn Adder + Send + Sync> = &concrete;

        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..ITERS {
                        object.lock().add(1);
                    }
                });
            }
        });

        assert_eq!(object.lock().total(), n as u64 * ITERS);
    }

    // ---------------------------------------------------------------
    // Ordering
    // ---------------------------------------------------------------

    #[test]
    fn every_waiter_is_served() {
        // Not a proof of FIFO -- that needs the fixed-window
        // measurement in benches/fairness.rs, which can actually
        // observe the distribution. This is the weaker claim the test
        // suite can make deterministically: run a fixed number of
        // acquisitions per thread and confirm every thread completed
        // all of them, i.e. nobody was starved out of the queue.
        const ITERS: usize = 2_000 / SCALE;

        let n = threads();
        let lock = McsSpinlock::new(HashMap::<usize, usize>::new());

        std::thread::scope(|s| {
            // `move` is needed to copy `id` into each closure, so
            // borrow the lock explicitly first rather than moving it.
            let lock = &lock;

            for id in 0..n {
                s.spawn(move || {
                    for _ in 0..ITERS {
                        *lock.lock().entry(id).or_insert(0) += 1;
                    }
                });
            }
        });

        let counts = lock.into_inner();
        assert_eq!(counts.len(), n);

        for id in 0..n {
            assert_eq!(counts.get(&id), Some(&ITERS), "thread {id} came up short");
        }
    }
}

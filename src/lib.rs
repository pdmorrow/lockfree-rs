//! Concurrency primitives written from scratch, for study rather than
//! for production.
//!
//! The point of this crate is not the API, which is deliberately the
//! same shape as [`std::sync::Mutex`]'s, but the reasoning behind the
//! implementation: why a flag is padded to 128 bytes on x86_64, why a
//! queued lock cannot cheaply support a timeout, why the fair lock is
//! also the faster one past four threads. That reasoning lives in the
//! module documentation, so this crate is meant to be read rendered:
//!
//! ```sh
//! scripts/doc.sh --open
//! ```
//!
//! The script defaults to `--document-private-items` on purpose. The
//! machinery the prose is about -- the cache-line padding, the spin
//! hint, the MCS node pool -- is all private, and without that flag
//! rustdoc drops it and the links pointing at it go dead.
//!
//! # The locks
//!
//! | Type | Waiters spin on | Order |
//! | --- | --- | --- |
//! | [`Spinlock`](spinlock::Spinlock) | one shared flag | whatever the hardware picks |
//! | [`McsSpinlock`](mcs_spinlock::McsSpinlock) | a flag of their own | strict FIFO |
//!
//! Both busy-wait rather than parking, both hand out RAII guards,
//! neither poisons on panic, and both take `T: ?Sized`. They are
//! interchangeable at the call site:
//!
//! ```
//! use std::thread;
//!
//! use lockfree_rs::mcs_spinlock::McsSpinlock;
//! use lockfree_rs::spinlock::Spinlock;
//!
//! let counter = Spinlock::new(0u64);
//! thread::scope(|s| {
//!     for _ in 0..4 {
//!         s.spawn(|| {
//!             for _ in 0..1_000 {
//!                 *counter.lock() += 1;
//!             }
//!         });
//!     }
//! });
//! assert_eq!(counter.into_inner(), 4_000);
//!
//! // The same code, queued.
//! let queued = McsSpinlock::new(0u64);
//! *queued.lock() += 1;
//! assert_eq!(queued.into_inner(), 1);
//! ```
//!
//! # Choosing between them
//!
//! Neither is a general-purpose mutex: a spinlock burns a core while
//! it waits, so both are confined to critical sections short enough
//! that spinning beats a context switch, and both degrade badly if
//! the threads outnumber the cores.
//!
//! Within that constraint, [`Spinlock`](spinlock::Spinlock) owns the
//! uncontended acquire -- two atomic operations and nothing else --
//! and [`McsSpinlock`](mcs_spinlock::McsSpinlock) pays about 1.4 ns
//! more for its queue node to buy an O(1) handoff and genuine
//! fairness. The crossover on a 12-core machine is around four
//! threads, past which the queued lock is both the fairer and the
//! faster one. `README.md` has the measurements and the argument;
//! `benches/` has the code that produced them.
//!
//! [`std::sync::Mutex`]: https://doc.rust-lang.org/std/sync/struct.Mutex.html

mod cache;
mod spin;

pub mod mcs_spinlock;
pub mod spinlock;

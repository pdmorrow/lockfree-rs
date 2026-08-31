//! Concurrency primitives written from scratch.
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
//! use spinlock_rs::mcs_spinlock::McsSpinlock;
//! use spinlock_rs::spinlock::Spinlock;
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
//! Within that constraint the choice is decided by how deep the queue
//! gets. [`Spinlock`](spinlock::Spinlock) barges, which is what makes
//! it the faster of the two at two threads, where a queue has nothing
//! to organise; [`McsSpinlock`](mcs_spinlock::McsSpinlock) pays about
//! 1.3 ns for the node it swaps into its queue and buys an O(1)
//! handoff and genuine fairness with it. The crossover on a 12-core
//! machine is around four threads, past which the queued lock is both
//! the fairer and the faster one.
//!
//! Neither side of that comparison is the uncontended acquire. A lock
//! nobody is contending is not a case this crate measures or has
//! advice about -- if that is the shape of the workload, the lock is
//! not what is costing you anything. `README.md` has the measurements
//! and the argument; `benches/` has the code that produced them.
//!
//! [`std::sync::Mutex`]: https://doc.rust-lang.org/std/sync/struct.Mutex.html

mod cache;
mod spin;

pub mod mcs_spinlock;
pub mod spinlock;

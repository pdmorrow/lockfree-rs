//! The pause executed on each trip around a spin loop.

/// The pause executed on each trip around a spin loop.
///
/// Natively this is the architecture's spin hint (`pause` on x86,
/// `yield` on ARM): it does not give up the core, it just tells the
/// pipeline not to speculate its way through a tight loop and to let
/// a sibling hyperthread have the issue slots.
///
/// Under Miri there is no pipeline and no parallelism -- threads are
/// interleaved by an interpreter -- so a hint that does nothing
/// leaves the holder waiting on a scheduler that only preempts at a
/// low fixed rate. Yielding instead hands the interpreter an
/// explicit switch point, which turns a spin that takes thousands of
/// interpreted steps into one that takes a handful.
#[inline]
pub(crate) fn spin_hint() {
    // `cfg!` rather than `#[cfg]` so both arms are always compiled
    // and type-checked; the branch folds away at compile time.
    if cfg!(miri) {
        std::thread::yield_now();
    } else {
        std::hint::spin_loop();
    }
}

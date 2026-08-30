//! Cache-line padding.
//!
//! Shared by every lock in the crate: the whole point of a spinlock
//! is to keep contended atomics off each other's coherence unit, and
//! there is no reason for each implementation to arrive at the
//! alignment table independently.

// Forces whatever it wraps onto its own cache-line-sized slot, so
// a contended atomic can't share a line with the data it protects,
// or with a neighbouring lock in an array. Without this, a CAS on
// one lock would invalidate the line holding the other: "false
// sharing".
//
// Why the alignment is picked per target instead of asked of the OS
//
// The platform does know: Linux reports it in
// /sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size and
// via sysconf(_SC_LEVEL1_DCACHE_LINESIZE), macOS in
// sysctl hw.cachelinesize. But `repr(align(N))` needs N as a
// literal in the attribute, evaluated when the type is laid out, so
// a value read at runtime can never reach it -- by the time the
// process can call sysconf, every offset in the struct is already
// fixed. A build script could bridge the gap by emitting a cfg, but
// it would bake the *build* machine's cache geometry into the
// artifact, which is wrong the moment you cross-compile or ship a
// binary. So the choice is made from the target architecture, and
// the runtime value is used to CHECK that choice rather than to
// make it -- see `alignment_covers_the_platform_cache_line` in the
// spinlock tests, which reads the OS's number and fails if we
// guessed low.
//
// The numbers are the conservative ones, i.e. the largest line in
// use on each architecture, since over-aligning costs padding while
// under-aligning silently costs coherence traffic:
//
//   x86_64      128, not 64. The coherence granule is 64 -- that
//               is what the OS reports and what MESI tracks. The
//               extra 64 is for Intel's L2 adjacent-line prefetcher,
//               which completes every fetched line to an aligned
//               128-byte pair: a write to one line invalidates the
//               pair line other cores speculatively pulled in.
//               Weaker than true false sharing, and absent on AMD,
//               but the padding is cheap. folly and Java's
//               @Contended pad to 128 for the same reason.
//   aarch64     128. Apple silicon uses 128-byte lines; most other
//               ARM64 cores use 64.
//   powerpc64   128, s390x 256, both the hardware line size.
//   arm/riscv/  32 and 64 respectively on the common cores.
//   others      64, the near-universal default.
//
// This is the same table crossbeam's CachePadded arrived at.
#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
    ),
    repr(align(128))
)]
#[cfg_attr(target_arch = "s390x", repr(align(256)))]
#[cfg_attr(
    any(target_arch = "arm", target_arch = "mips", target_arch = "mips64"),
    repr(align(32))
)]
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
        target_arch = "s390x",
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips64",
    )),
    repr(align(64))
)]
#[derive(Debug)]
pub(crate) struct Aligned<T>(pub(crate) T);

/// The padding this crate assumes a cache line needs, in bytes.
///
/// Chosen at compile time from the target architecture, and always
/// at least the true line size, so two values this far apart never
/// share a line (or a prefetch pair) on the targets listed above.
///
/// Derived from the type rather than written out a second time, so
/// the constant cannot drift away from the layout it describes.
pub const CACHE_LINE_ALIGN: usize = align_of::<Aligned<u8>>();

impl<T> std::ops::Deref for Aligned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

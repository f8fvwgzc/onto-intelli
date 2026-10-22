//! A counting global allocator: live and peak heap bytes as requested from
//! the allocator, before malloc's own rounding. Unlike `ps` RSS it does not
//! drop pages the OS has compressed or swapped out.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

pub struct Counting;

fn grow(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrink(by: usize) {
    LIVE.fetch_sub(by, Ordering::Relaxed);
}

// SAFETY: `GlobalAlloc` can only be implemented with `unsafe`. Every method
// forwards to `System` unchanged and only updates two counters; it keeps no
// pointer state, so `System`'s guarantees carry over.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grow(new_size - layout.size());
            } else {
                shrink(layout.size() - new_size);
            }
        }
        p
    }
}

/// Live and peak heap in MB since the process started.
pub fn heap_mb() -> (f64, f64) {
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    (mb(LIVE.load(Ordering::Relaxed)), mb(PEAK.load(Ordering::Relaxed)))
}

/// Restarts peak tracking from the current live size.
pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

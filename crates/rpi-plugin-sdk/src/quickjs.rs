//! Bounded QuickJS allocations with an out-of-band, sticky OOM signal.
//!
//! Use `Runtime::new_with_alloc(Heap::new(limit, failed))`, without also setting
//! QuickJS's memory limit: its own pre-check could reject before our latch runs.
//! A caught guest OOM must still cause the caller to reject the run by checking
//! `failed` after execution. The latch does not prevent recovery allocations.
//!
//! This accounting depends on pinned rquickjs 0.8.1: RustAllocator rounds payloads
//! to `align_of::<u64>()` and prepends max(size_of::<usize>(), alignment) bytes.
//! We charge both, plus two pointer-sized words of conservative allocator overhead
//! per block. This is a live charged-byte limit, not a bound on process RSS or
//! global-allocator fragmentation. No pointer/header manipulation is done here;
//! layouts are only validated. RustAllocator owns allocation layout and ownership.

#![deny(unsafe_op_in_unsafe_fn)]

use rquickjs::allocator::{Allocator, RustAllocator};
use std::{
    alloc::Layout,
    mem::{align_of, size_of},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const ALIGN: usize = align_of::<u64>();
const HEADER: usize = if size_of::<usize>() > ALIGN {
    size_of::<usize>()
} else {
    ALIGN
};
const EXTRA_OVERHEAD: usize = 2 * size_of::<usize>();
const BLOCK_OVERHEAD: usize = HEADER + EXTRA_OVERHEAD;

pub struct Heap {
    allocator: RustAllocator,
    limit: usize,
    live: usize,
    failed: Arc<AtomicBool>,
}

impl Heap {
    pub fn new(limit: usize, failed: Arc<AtomicBool>) -> Self {
        Self {
            allocator: RustAllocator,
            limit,
            live: 0,
            failed,
        }
    }

    fn fail(&self) -> *mut u8 {
        self.failed.store(true, Ordering::Release);
        ptr::null_mut()
    }

    /// Validate every unchecked operation in the pinned delegate before calling
    /// it, including realloc's new layout (which it does not validate itself).
    fn charge(size: usize) -> Option<usize> {
        let rounded = size.checked_add(ALIGN - 1)? / ALIGN * ALIGN;
        let allocation = rounded.checked_add(HEADER)?;
        Layout::from_size_align(allocation, ALIGN).ok()?;
        allocation.checked_add(EXTRA_OVERHEAD)
    }

    fn replacement_live(&self, old: usize, new: usize) -> Option<usize> {
        self.live
            .checked_sub(old)?
            .checked_add(new)
            .filter(|&live| live <= self.limit)
    }
}

// SAFETY: All non-null blocks come exclusively from RustAllocator, which owns
// their layout, alignment, header, and usable size. Checked request validation
// protects its unchecked rounding/header math and calloc multiplication. A
// block's charge is added only after allocation succeeds and removed only on
// free or successful replacement. Failed realloc leaves both block and charge
// unchanged. &mut self serializes accounting; the shared latch is atomic. As
// required by Allocator, callers must not mix heaps, double-free, or use stale
// pointers. Null/zero libc callback cases are handled before delegating.
unsafe impl Allocator for Heap {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if size == 0 {
            return ptr::null_mut();
        }
        let Some(charge) = Self::charge(size) else {
            return self.fail();
        };
        let Some(live) = self.replacement_live(0, charge) else {
            return self.fail();
        };
        let block = self.allocator.alloc(size);
        if block.is_null() {
            return self.fail();
        }
        self.live = live;
        block
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        if count == 0 || size == 0 {
            return ptr::null_mut();
        }
        let Some(size_total) = count.checked_mul(size) else {
            return self.fail();
        };
        let Some(charge) = Self::charge(size_total) else {
            return self.fail();
        };
        let Some(live) = self.replacement_live(0, charge) else {
            return self.fail();
        };
        let block = self.allocator.calloc(count, size);
        if block.is_null() {
            return self.fail();
        }
        self.live = live;
        block
    }

    unsafe fn dealloc(&mut self, block: *mut u8) {
        if block.is_null() {
            return;
        }
        // SAFETY: The caller supplies a live block from this heap; the delegate
        // allocated it. Its rounded size plus overhead was checked on creation.
        let charge = unsafe { RustAllocator::usable_size(block) } + BLOCK_OVERHEAD;
        // SAFETY: Ownership and original layout remain with the delegate.
        unsafe { self.allocator.dealloc(block) };
        self.live -= charge;
    }

    unsafe fn realloc(&mut self, block: *mut u8, new_size: usize) -> *mut u8 {
        if block.is_null() {
            return self.alloc(new_size);
        }
        if new_size == 0 {
            // SAFETY: The caller supplies a live block from this heap.
            unsafe { self.dealloc(block) };
            return ptr::null_mut();
        }
        let Some(charge) = Self::charge(new_size) else {
            return self.fail();
        };
        // SAFETY: The caller supplies a live delegated block. Its charge was
        // checked when allocated, so the overhead addition cannot overflow.
        let old = unsafe { RustAllocator::usable_size(block) } + BLOCK_OVERHEAD;
        let Some(live) = self.replacement_live(old, charge) else {
            return self.fail();
        };
        // SAFETY: The block belongs to the delegate, new_size is nonzero, and
        // charge() validated rounding, header addition and the resulting layout.
        // RustAllocator preserves the old allocation if realloc returns null.
        let replacement = unsafe { self.allocator.realloc(block, new_size) };
        if replacement.is_null() {
            return self.fail();
        }
        self.live = live;
        replacement
    }

    unsafe fn usable_size(block: *mut u8) -> usize {
        if block.is_null() {
            return 0;
        }
        // SAFETY: The caller supplies a live block from this allocator, hence
        // from RustAllocator. Report payload only, not our accounting overhead.
        unsafe { RustAllocator::usable_size(block) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_heap(limit: usize) -> (Heap, Arc<AtomicBool>) {
        let failed = Arc::new(AtomicBool::new(false));
        (Heap::new(limit, failed.clone()), failed)
    }

    #[test]
    fn live_accounting_includes_rounded_payload_and_overhead() {
        let charge = Heap::charge(1).unwrap();
        assert_eq!(charge, ALIGN + BLOCK_OVERHEAD);
        let (mut heap, failed) = new_heap(charge * 2);
        let a = heap.alloc(1);
        let b = heap.alloc(1);
        assert!(!a.is_null() && !b.is_null());
        assert_eq!(heap.live, charge * 2);
        assert!(heap.alloc(1).is_null());
        assert!(failed.load(Ordering::Acquire));
        // SAFETY: Each pointer is a distinct live block from this heap.
        unsafe {
            assert_eq!(Heap::usable_size(a), ALIGN);
            heap.dealloc(a);
            assert_eq!(heap.live, charge);
            heap.dealloc(b);
        }
        assert_eq!(heap.live, 0);
    }

    #[test]
    fn realloc_growth_shrink_and_failure_preserve_contents_and_accounting() {
        let (mut heap, failed) = new_heap(Heap::charge(64).unwrap());
        let a = heap.alloc(8);
        assert!(!a.is_null());
        // SAFETY: a is live and has at least eight initialized writable bytes
        // after fill. Successful realloc transfers ownership; a rejected one
        // leaves ownership with its original pointer. Free the final block.
        unsafe {
            std::slice::from_raw_parts_mut(a, 8).fill(0x5a);
            let b = heap.realloc(a, 64);
            assert!(!b.is_null());
            assert_eq!(heap.live, Heap::charge(64).unwrap());
            assert_eq!(std::slice::from_raw_parts(b, 8), &[0x5a; 8]);
            assert!(heap.realloc(b, 65).is_null());
            assert!(failed.load(Ordering::Acquire));
            assert_eq!(heap.live, Heap::charge(64).unwrap());
            assert_eq!(Heap::usable_size(b), 64);
            assert_eq!(std::slice::from_raw_parts(b, 8), &[0x5a; 8]);
            let c = heap.realloc(b, 1);
            assert!(!c.is_null());
            assert_eq!(*c, 0x5a);
            assert_eq!(heap.live, Heap::charge(1).unwrap());
            heap.dealloc(c);
        }
        assert_eq!(heap.live, 0);
        assert!(failed.load(Ordering::Acquire));
        let recovered = heap.alloc(8);
        assert!(!recovered.is_null());
        // SAFETY: recovered is a live block from this heap.
        unsafe { heap.dealloc(recovered) };
        assert_eq!(heap.live, 0);
        assert!(failed.load(Ordering::Acquire));
    }

    #[test]
    fn overflow_and_invalid_layout_latch_without_delegating() {
        for size in [usize::MAX, usize::MAX - (ALIGN - 1), isize::MAX as usize] {
            let (mut heap, failed) = new_heap(usize::MAX);
            assert!(heap.alloc(size).is_null());
            assert!(failed.load(Ordering::Acquire));
            assert_eq!(heap.live, 0);
        }
        let (mut heap, failed) = new_heap(usize::MAX);
        assert!(heap.calloc(usize::MAX, 2).is_null());
        assert!(failed.load(Ordering::Acquire));
        assert_eq!(heap.live, 0);
        let (mut heap, failed) = new_heap(usize::MAX);
        assert!(heap.calloc(1, usize::MAX).is_null());
        assert!(failed.load(Ordering::Acquire));
        assert_eq!(heap.live, 0);
    }

    #[test]
    fn overflowing_realloc_preserves_the_old_block() {
        let (mut heap, failed) = new_heap(usize::MAX);
        let a = heap.calloc(1, 8);
        assert!(!a.is_null());
        // SAFETY: a remains live after a failed realloc and contains eight
        // initialized bytes from calloc. It is freed once below.
        unsafe {
            assert!(heap.realloc(a, usize::MAX).is_null());
            assert_eq!(std::slice::from_raw_parts(a, 8), &[0; 8]);
            assert_eq!(heap.live, Heap::charge(8).unwrap());
            heap.dealloc(a);
        }
        assert_eq!(heap.live, 0);
        assert!(failed.load(Ordering::Acquire));
    }

    #[test]
    fn calloc_zeroes_payload_and_charges_the_product() {
        let (mut heap, failed) = new_heap(1024);
        let a = heap.calloc(3, 7);
        assert!(!a.is_null());
        assert_eq!(heap.live, Heap::charge(21).unwrap());
        // SAFETY: calloc returned a live block with at least 21 zeroed bytes.
        unsafe {
            let usable = Heap::usable_size(a);
            assert_eq!(std::slice::from_raw_parts(a, usable), vec![0; usable]);
            heap.dealloc(a);
        }
        assert_eq!(heap.live, 0);
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn null_and_zero_callbacks_are_non_failures() {
        let (mut heap, failed) = new_heap(1024);
        assert!(heap.alloc(0).is_null());
        assert!(heap.calloc(0, usize::MAX).is_null());
        assert!(heap.calloc(usize::MAX, 0).is_null());
        // SAFETY: Heap explicitly supports null callback arguments. a is a
        // live allocation until realloc(a, 0) frees it; never use it afterward.
        unsafe {
            heap.dealloc(ptr::null_mut());
            assert_eq!(Heap::usable_size(ptr::null_mut()), 0);
            assert!(heap.realloc(ptr::null_mut(), 0).is_null());
            let a = heap.realloc(ptr::null_mut(), 8);
            assert!(!a.is_null());
            assert!(heap.realloc(a, 0).is_null());
        }
        assert_eq!(heap.live, 0);
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn calloc_limit_failure_is_sticky_but_allows_recovery() {
        let (mut heap, failed) = new_heap(Heap::charge(8).unwrap());
        assert!(heap.calloc(2, 8).is_null());
        assert_eq!(heap.live, 0);
        assert!(failed.load(Ordering::Acquire));
        let a = heap.calloc(1, 8);
        assert!(!a.is_null());
        // SAFETY: a is a live block allocated by this heap.
        unsafe { heap.dealloc(a) };
        assert_eq!(heap.live, 0);
        assert!(failed.load(Ordering::Acquire));
    }
}

//! The library's tests' allocator: passes every call on to `System` and
//! counts for the thread of a test that asks ([`peak_of`]), so that a unit
//! test can say what a read allocates whatever the other tests are doing.
//! A thread that is not asking only reads one thread local per call.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The bytes the thread holds beyond what it held when asked (below 0 when
/// it frees what it held before), and the most of them so far.
#[derive(Clone, Copy)]
struct Count {
    in_use: isize,
    peak: isize,
}

// No destructor and a constant start: reading it never allocates.
thread_local! {
    static COUNT: Cell<Option<Count>> = const { Cell::new(None) };
}

fn moved(bytes: isize) {
    let _ = COUNT.try_with(|cell| {
        if let Some(count) = cell.get() {
            let in_use = count.in_use + bytes;
            cell.set(Some(Count {
                in_use,
                peak: count.peak.max(in_use),
            }));
        }
    });
}

#[allow(clippy::cast_possible_wrap)]
const fn signed(bytes: usize) -> isize {
    bytes as isize
}

struct Counting;

// SAFETY: every call is passed on to `System` unchanged; only a counter of
// the thread moves, and that does not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is `System`'s.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            moved(signed(layout.size()));
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            moved(signed(layout.size()));
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(pointer, layout) };
        moved(-signed(layout.size()));
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `alloc`.
        let grown = unsafe { System.realloc(pointer, layout, new_size) };
        if !grown.is_null() {
            moved(signed(new_size) - signed(layout.size()));
        }
        grown
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The most bytes `read` held at once on this thread, what it returns
/// included. What it allocates on another thread is not counted.
pub(crate) fn peak_of<T>(read: impl FnOnce() -> T) -> usize {
    COUNT.with(|cell| {
        cell.set(Some(Count { in_use: 0, peak: 0 }));
    });
    let answer = read();
    let count = COUNT.with(|cell| cell.take());
    drop(answer);
    count.map_or(0, |count| count.peak.unsigned_abs())
}

#[test]
fn it_counts_what_the_thread_holds_at_its_most() {
    let held = peak_of(|| {
        drop(std::hint::black_box(vec![0_u8; 1000]));
        std::hint::black_box(vec![0_u8; 400])
    });
    assert_eq!(held, 1000);
    assert_eq!(peak_of(|| 1), 0);
}

//! Count allocations on the measuring test's thread without counting parallel tests.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static CALLS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record() {
    // TLS may be gone during thread teardown; no measurement can be active then.
    let _ = CALLS.try_with(|calls| {
        if let Some(count) = calls.get() {
            calls.set(Some(count + 1));
        }
    });
}

// SAFETY: every operation forwards the unchanged pointer/layout to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: the caller supplies the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: the caller supplies the layout required by GlobalAlloc.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        // SAFETY: the caller supplies System's live allocation and valid new size.
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller supplies System's live allocation and original layout.
        unsafe { System.dealloc(pointer, layout) }
    }
}

pub(crate) fn measure<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CALLS.set(None);
        }
    }
    assert!(CALLS.get().is_none(), "allocation measurements cannot nest");
    CALLS.set(Some(0));
    let reset = Reset;
    let value = operation();
    let calls = CALLS.get().expect("measurement active");
    drop(reset);
    (value, calls)
}

#[test]
fn detects_allocations() {
    let (values, calls) = measure(|| std::hint::black_box(vec![0_u64; 32]));
    assert!(calls >= 1);
    drop(values);
    assert_eq!(measure(|| 42), (42, 0));
}

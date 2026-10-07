//! What naming a request costs in heap, measured by a counting allocator.
//!
//! A binary of its own (docs/agent_guide/integration_test_binaries.md): a
//! `#[global_allocator]` is process-wide. The count is kept per thread, so
//! the tests here may still run at once.
//!
//! The server names every request it is sent before the request is
//! authenticated (finding 50). Naming it once decoded the whole frame into
//! a `ciborium::Value` tree, about 50 times the frame in heap. The count
//! of bytes allocated is the measure: unlike a timing it does not depend
//! on the machine, and it is what the finding is about.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use serde::Serialize;
use spoolframe_protocol::codec;

struct Counting;

thread_local! {
    // A const initializer and no destructor: reading it never allocates,
    // so the allocator may use it.
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + bytes));
}

// SAFETY: every call is passed to `System` unchanged; only a count is kept.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// What `f` returns, and how many bytes this thread allocated running it.
fn allocated_by<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATED.with(Cell::get);
    let value = f();
    (value, ALLOCATED.with(Cell::get) - before)
}

#[derive(Serialize)]
struct Wire<R> {
    envelope: Envelope<R>,
    operation: Option<()>,
    expected_revision: Option<u64>,
}

#[derive(Serialize)]
struct Envelope<R> {
    session: Option<String>,
    request: R,
}

/// A request of a type this build may not know, with fields the namer
/// must step over.
#[derive(Serialize)]
struct Padded {
    #[serde(rename = "type")]
    kind: String,
    /// An array of small integers: one byte each on the wire, one `Value`
    /// each when decoded into a tree.
    padding: Vec<u8>,
}

fn frame(kind: &str, padding: usize) -> Vec<u8> {
    codec::encode(&Wire {
        envelope: Envelope {
            session: None,
            request: Padded {
                kind: kind.to_owned(),
                padding: vec![0; padding],
            },
        },
        operation: None,
        expected_revision: None,
    })
    .unwrap()
}

#[test]
fn naming_a_request_allocates_nothing_for_the_rest_of_its_frame() {
    let frame = frame("ping", 512 << 10);
    let (name, allocated) = allocated_by(|| codec::rpc_request_type(&frame));
    assert_eq!(name.as_deref(), Some("ping"));
    assert!(
        allocated < 1024,
        "naming a request of {} bytes allocated {allocated} bytes",
        frame.len()
    );
}

/// A client chooses its request's type, up to a whole frame of it; the
/// name read is the first characters of it only (C-12).
#[test]
fn a_long_type_is_read_only_as_far_as_its_name_is_kept() {
    let frame = frame(&"x".repeat(512 << 10), 0);
    let (name, allocated) = allocated_by(|| codec::rpc_request_type(&frame));
    assert_eq!(name, Some("x".repeat(codec::MAX_REQUEST_TYPE)));
    assert!(
        allocated < 1024,
        "naming a request of {} bytes allocated {allocated} bytes",
        frame.len()
    );
}

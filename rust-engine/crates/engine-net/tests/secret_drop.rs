//! Proves `Secret`'s Drop wipes the buffer: a spy global allocator inspects the block at the moment it is
//! freed. (A control with a plain `String` shows the spy can see leftovers.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, Ordering};

use engine_net::Secret;

/// An unusual allocation size so unrelated allocations never match.
const MARKED_SIZE: usize = 4099;
const NEEDLE: &[u8] = b"needle-secret-xyz";

static LEFTOVER_SEEN: AtomicBool = AtomicBool::new(false);

struct Spy;

unsafe impl GlobalAlloc for Spy {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == MARKED_SIZE {
            let block = std::slice::from_raw_parts(ptr, layout.size());
            if block.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
                LEFTOVER_SEEN.store(true, Ordering::SeqCst);
            }
        }
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOCATOR: Spy = Spy;

fn marked_string() -> String {
    let mut value = String::with_capacity(MARKED_SIZE);
    value.push_str(std::str::from_utf8(NEEDLE).unwrap());
    value
}

#[test]
fn secret_drop_zeroizes_its_buffer() {
    // Control: dropping a plain String leaves the bytes in the freed block, and the spy notices.
    drop(marked_string());
    assert!(
        LEFTOVER_SEEN.swap(false, Ordering::SeqCst),
        "the spy allocator must be able to see leftovers"
    );

    let secret = Secret::new(marked_string());
    assert_eq!(secret.expose().as_bytes(), NEEDLE);
    drop(secret);
    assert!(
        !LEFTOVER_SEEN.load(Ordering::SeqCst),
        "Secret left its contents in freed memory"
    );
}

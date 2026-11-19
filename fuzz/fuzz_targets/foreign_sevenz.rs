#![no_main]
#![feature(alloc_error_hook)]

use libfuzzer_sys::fuzz_target;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Once;

/// sevenz-rust2 sizes vectors from header counts. In the helper, RLIMIT_AS or
/// the Job Object turns an absurd one into a reported crash; under the
/// fuzzer's sanitizer that would be an out-of-memory finding about a limit
/// the helper already has. So single allocations over `CAP` fail here, and
/// the failure is a panic the engine reports as a malformed archive.
const CAP: usize = 256 << 20;

struct Capped;

// SAFETY: forwards to the system allocator, only refusing (null) oversized
// requests, which `GlobalAlloc` permits.
unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() > CAP {
            return std::ptr::null_mut();
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() > CAP {
            return std::ptr::null_mut();
        }
        System.alloc_zeroed(l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n > CAP {
            return std::ptr::null_mut();
        }
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: Capped = Capped;

fn refuse(_: Layout) {
    panic!("allocation over the fuzz cap");
}

fuzz_target!(|data: &[u8]| {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        std::alloc::set_alloc_error_hook(refuse);
        // libFuzzer's hook aborts on any panic, even one the engine catches.
        // An uncaught panic still reaches libFuzzer's own catch and aborts.
        std::panic::set_hook(Box::new(|_| {}));
    });
    mochi_foreign::fuzz::exercise_sevenz_listing(data);
});

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    mochi_foreign::fuzz::exercise_tar_listing(data);
});

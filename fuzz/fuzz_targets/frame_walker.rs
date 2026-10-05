#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    mochi_testkit::fuzz::exercise_walker(data);
});

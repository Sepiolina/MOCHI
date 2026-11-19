#![no_main]

use libfuzzer_sys::fuzz_target;

// UnRAR opens files, not memory: the input goes through a temporary file here,
// in the fuzz harness (mochi-foreign itself never writes a file).
fuzz_target!(|data: &[u8]| {
    let dir = std::env::temp_dir().join(format!("mochi-fuzz-rar-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("in.rar");
    if std::fs::write(&path, data).is_ok() {
        mochi_foreign::fuzz::exercise_rar_file(&path);
    }
});

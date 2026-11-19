//! `mochi-foreign-helper`: the isolated process that parses foreign archives.
//! Protocol on stdin/stdout; see the crate README. Not meant to be run by hand.

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let code = mochi_foreign::helper::serve(stdin.lock(), stdout.lock());
    std::process::exit(code);
}

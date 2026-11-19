fn main() -> std::process::ExitCode {
    let code = mochi_cli::run(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::ExitCode::from(code)
}

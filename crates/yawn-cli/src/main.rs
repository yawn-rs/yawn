fn main() -> std::process::ExitCode {
    if let Some(code) = yawn_cli::dev::replay::shim() {
        return code;
    }
    yawn_cli::cli::run(std::env::args_os())
}

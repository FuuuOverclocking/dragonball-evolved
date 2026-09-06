use std::process::ExitCode;

fn main() -> ExitCode {
    if let Err(e) = dragonball_evolved::main() {
        // `{:?}` includes backtraces if available. Set RUST_BACKTRACE=1 to see them.
        eprintln!("Error: {e:?}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

use std::process::ExitCode;

fn main() -> ExitCode {
    match dragonball_evolved::main() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:?}` includes backtraces if available. Set RUST_BACKTRACE=1 to see them.
            eprintln!("Error: {:?}", e);
            ExitCode::FAILURE
        }
    }
}

use std::process::ExitCode;

#[tokio::main(flavor = "local")]
async fn main() -> ExitCode {
    if let Err(e) = dragonball_evolved::main().await {
        // `{:?}` includes backtraces if available. Set RUST_BACKTRACE=1 to see them.
        eprintln!("Error: {e:?}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

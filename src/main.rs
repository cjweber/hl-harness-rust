use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = hl_harness::cli::Cli::parse();
    match hl_harness::cli::dispatch(cli).await {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::from(1)
        }
    }
}

//! `weft-author probe-cli` — verify a local CLI backend is planner-safe.
//! Usage: weft-author-probe-cli --backend cli:codex

use clap::Parser;
use weft_author::cli_provider::{parse_backend_spec, probe_backend, BackendSpec};

#[derive(Parser)]
#[command(about = "Probe a local CLI backend for planner-safe stdin/final-text behavior")]
struct Args {
    /// Backend spec, e.g. cli:codex
    #[arg(long)]
    backend: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let name = match parse_backend_spec(&args.backend) {
        Ok(BackendSpec::Cli(n)) => n,
        Ok(BackendSpec::OpenRouter(_)) => {
            eprintln!("probe-cli only applies to cli: backends");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    match probe_backend(&name).await {
        Ok(()) => println!("probe passed: cli:{name} is planner-safe (cached)"),
        Err(e) => {
            eprintln!("probe FAILED: {e}");
            std::process::exit(1);
        }
    }
}

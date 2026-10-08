//! Separate read-only diagnostic binary; installing it never replaces the runtime.
use clap::Parser;
use lkjyolo::{diagnostics::inspect, util::home};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Inspect local receipt outcomes without executing or retrying work")]
struct Arguments {
    #[arg(long, env = "LKJYOLO_STATE")]
    state: Option<PathBuf>,
    /// Inspect this exact ID instead of a recent aggregate. No command is repeated.
    #[arg(long)]
    request_id: Option<String>,
    /// Bound the recent receipt sample; this is not a safety-block rate.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
    limit: u32,
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let result = (|| {
        let state = match arguments.state {
            Some(path) => path,
            None => home()?.join(".local/state/lkjyolo"),
        };
        inspect(
            &state,
            arguments.request_id.as_deref(),
            arguments.limit as usize,
        )
    })();
    match result {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            // Database errors and paths may contain private information. Do not
            // echo their arbitrary text into a support report or the conversation.
            eprintln!(
                "{}",
                json!({"error":"local_diagnostics_unavailable","platform_safety_verdict":"not_observable","automatic_replay":false})
            );
            ExitCode::from(2)
        }
    }
}

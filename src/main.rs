use clap::Parser;
use lkjyolo::cli::{Cli, dispatch};

fn main() {
    // Set before any HTTP client threads or subprocesses exist.
    unsafe { libc::umask(0o077) };
    match dispatch(Cli::parse()) {
        Ok(Some(value)) => {
            println!("{value}");
            if value["isError"] == true
                || matches!(value["status"].as_str(), Some("error" | "unknown"))
            {
                std::process::exit(1);
            }
        }
        Ok(None) => {}
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"isError":true,"error":format!("{error:#}")})
            );
            std::process::exit(1);
        }
    }
}

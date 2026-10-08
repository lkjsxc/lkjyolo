pub mod cli;
pub mod diagnostics;
pub mod engine;
pub mod files;
pub mod gateway;
pub mod journal;
pub mod mcp;
pub mod package;
pub mod runtime;
pub mod sessions;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ENGINE_VERSION: &str = "0.2.1";
pub const ENGINE_SHA256: &str = "790fc21733c51373d9967931a834325b2e9f49581e3e9187cf7fb54cb62435f6";

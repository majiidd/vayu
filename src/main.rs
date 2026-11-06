//! Vayu (وایو): a resilient command-line downloader.

mod app;
mod cli;
mod error;
mod fault;
mod integrity;
mod metrics;
mod presentation;
mod recovery;
mod scheduler;
mod state;
mod storage;
mod transport;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vayu-io")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("vayu: error: cannot start the async runtime: {err}");
            return ExitCode::from(error::Category::Internal.exit_code());
        }
    };
    let code = runtime.block_on(app::run(args));
    // Do not wait for lingering blocking work (e.g. a stuck terminal write).
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    ExitCode::from(code)
}

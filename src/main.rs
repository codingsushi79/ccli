mod cli;
mod config;
mod daemon;
mod endpoints;
mod hardware;
mod ipc;
mod log;
mod mining;
mod model;
mod nodes;
mod paths;
mod tls;
mod tui;

use clap::Parser;

fn main() {
    // rustls needs a process-wide crypto provider before any config is built.
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Err(err) = cli::run(cli::Cli::parse()) {
        eprintln!("cryptocli: {err:#}");
        std::process::exit(1);
    }
}

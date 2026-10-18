mod cachefile;
mod describe;
mod docbuild;
mod ops;
mod render;
mod server;
mod store;
mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

use store::Store;

/// API docs for a Rust project's dependencies at the versions in its Cargo.lock.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Directory inside the cargo workspace.
    #[arg(long, short = 'C', global = true, default_value = ".")]
    project: PathBuf,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server on stdio (the default).
    Serve,
    /// List direct dependencies with locked versions and features.
    Deps,
    /// Show an item, e.g. `serde::Serialize` or `bevy::prelude::Transform`.
    Item { path: String },
    /// Search public items by name under a crate or module.
    Search {
        #[arg(name = "CRATE")]
        krate: String,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let project = std::path::absolute(&cli.project)?;
    let text = match cli.command.unwrap_or(Cmd::Serve) {
        Cmd::Serve => {
            let service = server::Server::new(project)
                .serve(rmcp::transport::stdio())
                .await?;
            service.waiting().await?;
            return Ok(());
        }
        Cmd::Deps => ops::deps(&Store::new(project)?)?,
        Cmd::Item { path } => ops::item(&Store::new(project)?, &path)?,
        Cmd::Search {
            krate,
            query,
            limit,
        } => ops::search(&Store::new(project)?, &krate, &query, limit)?,
    };
    print!("{text}");
    Ok(())
}

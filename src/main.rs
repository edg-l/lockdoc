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
    Item {
        path: String,
        /// Crate or module the path is relative to.
        #[arg(long = "crate")]
        krate: Option<String>,
    },
    /// Search public items by name under a crate or module, or every direct dependency.
    Search {
        /// The crate or module to search under, or the query when it is the only argument.
        #[arg(value_name = "CRATE")]
        first: String,
        /// Without it, `CRATE` is the query and every direct dependency is searched.
        query: Option<String>,
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
        Cmd::Item { path, krate } => ops::item(&Store::new(project)?, krate.as_deref(), &path)?,
        Cmd::Search {
            first,
            query,
            limit,
        } => {
            let (krate, query) = match &query {
                Some(q) => (Some(first.as_str()), q),
                None => (None, &first),
            };
            ops::search(&Store::new(project)?, krate, query, limit)?
        }
    };
    print!("{text}");
    Ok(())
}

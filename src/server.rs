use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::docbuild::lock;
use crate::ops;
use crate::store::Store;

const INSTRUCTIONS: &str = "API docs for this Rust project's dependencies, at the exact versions \
and features in its Cargo.lock. Check a signature here instead of recalling it from memory. \
The first lookup in a crate builds its docs (seconds; a minute or more for a large facade such \
as bevy) and the result says what was built. Builds use lockdoc's own target directory under \
the user cache and `--frozen`, so they never touch the project's target/, its build lock or \
Cargo.lock, and running cargo in the project meanwhile is fine.";

#[derive(Clone)]
pub struct Server {
    default_dir: PathBuf,
    /// One store per project directory. The map's lock is held only to look a store up; each
    /// store synchronizes its own state, so queries run concurrently.
    stores: Arc<Mutex<HashMap<PathBuf, Arc<Store>>>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DepsArgs {
    /// Directory inside the cargo workspace; defaults to the server's working directory.
    project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ItemArgs {
    /// Path as written in code: `serde::Serialize`, `bevy::prelude::Transform`,
    /// `tokio::sync::Mutex::lock`. The first segment is the crate name.
    path: String,
    /// Directory inside the cargo workspace; defaults to the server's working directory.
    project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Crate or module to search under, such as `bevy` or `bevy::ecs`.
    #[serde(rename = "crate")]
    krate: String,
    /// Name or part of a name, case-insensitive. Matches items and their methods.
    query: String,
    /// Maximum results; default 20.
    limit: Option<usize>,
    /// Directory inside the cargo workspace; defaults to the server's working directory.
    project: Option<String>,
}

impl Server {
    pub fn new(default_dir: PathBuf) -> Self {
        Self {
            default_dir,
            stores: Arc::default(),
            tool_router: Self::tool_router(),
        }
    }

    /// Runs `f` against the store for `project` on a blocking thread, since builds shell out to
    /// cargo and can take minutes.
    async fn with_store(
        &self,
        project: Option<String>,
        f: impl FnOnce(&Store) -> anyhow::Result<String> + Send + 'static,
    ) -> Result<String, String> {
        let dir = project.map_or_else(|| self.default_dir.clone(), PathBuf::from);
        let store = {
            let mut stores = lock(&self.stores);
            match stores.entry(dir.clone()) {
                Entry::Occupied(e) => e.get().clone(),
                Entry::Vacant(e) => e
                    .insert(Arc::new(Store::new(dir).map_err(|e| format!("{e:#}"))?))
                    .clone(),
            }
        };
        tokio::task::spawn_blocking(move || f(&store).map_err(|e| format!("{e:#}")))
            .await
            .map_err(|e| format!("lockdoc task failed: {e}"))?
    }
}

#[tool_router]
impl Server {
    #[tool(
        description = "List the workspace's direct dependencies with the locked version and enabled features of each."
    )]
    async fn deps(&self, Parameters(a): Parameters<DepsArgs>) -> Result<String, String> {
        self.with_store(a.project, ops::deps).await
    }

    #[tool(
        description = "Show one item at the locked version: declaration, docs, and its methods, fields, variants, trait impls or module contents. Follows re-exports, so `bevy::prelude::Transform` works. `Type::method` shows one associated item."
    )]
    async fn item(&self, Parameters(a): Parameters<ItemArgs>) -> Result<String, String> {
        self.with_store(a.project, move |s| ops::item(s, &a.path))
            .await
    }

    #[tool(
        description = "Find public items by name under a crate or module, following re-exports, with their shortest public path and signature. Use when the path is unknown."
    )]
    async fn search(&self, Parameters(a): Parameters<SearchArgs>) -> Result<String, String> {
        let limit = a.limit.unwrap_or(20);
        self.with_store(a.project, move |s| {
            ops::search(s, &a.krate, &a.query, limit)
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("lockdoc", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

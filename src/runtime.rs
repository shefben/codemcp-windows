//! Shared, mutable gateway runtime.
//!
//! Holds the connected upstreams, the boot-time config (so a disabled server can
//! still be connected on demand), the executor (Python worker), and the current
//! SDK/tool-description state. The admin interface mutates this at runtime:
//! enabling/disabling an upstream reconnects it, regenerates the SDK, hot-reloads
//! the worker, and notifies MCP clients that the tool list changed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::service::{Peer, RoleServer};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::{self, ServerSpec};
use crate::env::Isolation;
use crate::error::Error;
use crate::exec::Executor;
use crate::launcher::Launcher;
use crate::prompt;
use crate::sdk::SdkRegistry;
use crate::upstream::SharedUpstreams;

/// The current generated SDK + its `execute_python` description.
pub struct SdkState {
    pub registry: SdkRegistry,
    pub description: String,
}

/// Status of one configured server, for `list`.
#[derive(Debug, Serialize)]
pub struct ServerStatus {
    pub name: String,
    pub kind: String,
    pub enabled_in_config: bool,
    pub connected: bool,
    pub tools: usize,
}

/// The shared runtime. Cheap to clone (everything behind `Arc`).
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

struct Inner {
    upstreams: SharedUpstreams,
    executor: Arc<dyn Executor>,
    isolation: Isolation,
    config_path: PathBuf,
    launcher: Launcher,
    /// Boot config: every server (enabled or not), interpolated.
    boot: Mutex<BTreeMap<String, ConfigEntry>>,
    /// Current SDK + description, regenerated on every change.
    sdk: Mutex<SdkState>,
    /// Connected MCP client peers to notify on tool-list changes.
    peers: Mutex<Vec<Peer<RoleServer>>>,
}

#[derive(Clone)]
struct ConfigEntry {
    spec: ServerSpec,
    enabled: bool,
}

impl Runtime {
    pub async fn new(
        upstreams: SharedUpstreams,
        executor: Arc<dyn Executor>,
        isolation: Isolation,
        config_path: PathBuf,
        launcher: Launcher,
        sdk: SdkState,
    ) -> Result<Self, Error> {
        let boot_list = config::load_all(&config_path)?;
        let mut boot = BTreeMap::new();
        for c in boot_list {
            boot.insert(
                c.name,
                ConfigEntry {
                    spec: c.spec,
                    enabled: c.enabled,
                },
            );
        }
        Ok(Self {
            inner: Arc::new(Inner {
                upstreams,
                executor,
                isolation,
                config_path,
                launcher,
                boot: Mutex::new(boot),
                sdk: Mutex::new(sdk),
                peers: Mutex::new(Vec::new()),
            }),
        })
    }

    /// The config path this gateway was started with.
    pub fn config_path(&self) -> &std::path::Path {
        &self.inner.config_path
    }

    /// The application that launched this gateway.
    pub fn launcher(&self) -> &Launcher {
        &self.inner.launcher
    }

    /// Current `execute_python` description (clone).
    pub async fn description(&self) -> String {
        self.inner.sdk.lock().await.description.clone()
    }

    /// The full MCP `Tool` object the model sees in `tools/list`: name,
    /// description, and the fixed `{code: string}` input schema.
    pub async fn tool_definition(&self) -> serde_json::Value {
        use serde_json::{json, Map};
        let schema: Map<String, serde_json::Value> = json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute. SDK functions are preloaded; \
                                    assign to `result` (or leave a final expression) to return a value."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        })
        .as_object()
        .cloned()
        .expect("schema is an object");
        json!({
            "name": "execute_python",
            "description": self.description().await,
            "inputSchema": schema,
        })
    }

    /// Current generated `sdk.py` source (clone).
    pub async fn sdk_py(&self) -> String {
        self.inner.sdk.lock().await.registry.generate_sdk_py()
    }

    /// Run user code in the Python worker.
    pub async fn executor_run(&self, code: String) -> Result<crate::control::RunOutput, Error> {
        self.inner.executor.run(code).await
    }

    /// Register a connected MCP client peer (for tool-list-changed notifications).
    pub async fn register_peer(&self, peer: Peer<RoleServer>) {
        self.inner.peers.lock().await.push(peer);
    }

    /// Status of every configured server.
    pub async fn list(&self) -> Vec<ServerStatus> {
        let boot = self.inner.boot.lock().await;
        let mut out = Vec::new();
        for (name, entry) in boot.iter() {
            let connected = self.inner.upstreams.is_connected(name).await;
            let tools = if connected {
                self.inner
                    .upstreams
                    .all_tools()
                    .await
                    .iter()
                    .filter(|t| &t.server == name)
                    .count()
            } else {
                0
            };
            let kind = match entry.spec {
                ServerSpec::Local { .. } => "local",
                ServerSpec::Remote { .. } => "remote",
            }
            .to_string();
            out.push(ServerStatus {
                name: name.clone(),
                kind,
                enabled_in_config: entry.enabled,
                connected,
                tools,
            });
        }
        out
    }

    /// Enable (connect) a server at runtime. Returns the number of tools it
    /// exposes. When `make_default`, also persists `enabled: true` to the config.
    pub async fn enable(&self, name: &str, make_default: bool) -> Result<usize, Error> {
        let spec = {
            let boot = self.inner.boot.lock().await;
            boot.get(name)
                .map(|e| e.spec.clone())
                .ok_or_else(|| Error::Config(format!("unknown server: {name}")))?
        };

        let count = self.inner.upstreams.connect_one(name, &spec).await?;
        self.regenerate_and_reload().await?;

        if make_default {
            config::set_enabled(&self.inner.config_path, name, true)?;
            if let Some(e) = self.inner.boot.lock().await.get_mut(name) {
                e.enabled = true;
            }
        }
        Ok(count)
    }

    /// Disable (disconnect) a server at runtime. When `make_default`, also
    /// persists `enabled: false` to the config.
    pub async fn disable(&self, name: &str, make_default: bool) -> Result<bool, Error> {
        {
            let boot = self.inner.boot.lock().await;
            if !boot.contains_key(name) {
                return Err(Error::Config(format!("unknown server: {name}")));
            }
        }
        let was = self.inner.upstreams.disconnect_one(name).await;
        self.regenerate_and_reload().await?;

        if make_default {
            config::set_enabled(&self.inner.config_path, name, false)?;
            if let Some(e) = self.inner.boot.lock().await.get_mut(name) {
                e.enabled = false;
            }
        }
        Ok(was)
    }

    /// Rebuild the SDK from currently-connected tools, hot-reload the worker, and
    /// notify MCP clients that the tool list changed.
    async fn regenerate_and_reload(&self) -> Result<(), Error> {
        let tools = self.inner.upstreams.all_tools().await;
        let registry = SdkRegistry::build(&tools);
        let sdk_py = registry.generate_sdk_py();
        let description = prompt::build_description(&registry, self.inner.isolation);

        // Hot-reload the worker's SDK module.
        self.inner.executor.reload_sdk(&sdk_py).await?;

        // Swap the shared SDK state.
        {
            let mut sdk = self.inner.sdk.lock().await;
            sdk.registry = registry;
            sdk.description = description;
        }

        // Notify clients; drop peers whose connection has gone away.
        let mut peers = self.inner.peers.lock().await;
        let mut alive = Vec::with_capacity(peers.len());
        for peer in peers.drain(..) {
            if peer.notify_tool_list_changed().await.is_ok() {
                alive.push(peer);
            }
        }
        *peers = alive;

        Ok(())
    }

    pub async fn shutdown(&self) {
        self.inner.executor.shutdown().await;
        self.inner.upstreams.shutdown().await;
    }
}

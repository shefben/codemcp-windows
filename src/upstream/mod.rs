//! Manages the set of connected upstream MCP servers.
//!
//! Connects to enabled servers at startup, lists their tools, and routes tool
//! calls to the owning server. Failed upstreams are logged and skipped (never
//! fatal). Upstreams can be connected/disconnected at runtime via the admin
//! interface, so the set lives behind an `RwLock`.

mod client;

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use tokio::sync::RwLock;

use crate::config::{ServerSpec, UpstreamConfig};
use crate::error::Error;

use client::UpstreamService;

/// One connected upstream: its live service plus the tools it exposes.
struct Upstream {
    service: UpstreamService,
    tools: Vec<Tool>,
}

/// A tool exposed by some upstream, tagged with its server.
#[derive(Debug, Clone)]
pub struct NamespacedTool {
    pub server: String,
    pub tool: Tool,
}

/// Holds all upstream connections and provides tool routing.
pub struct UpstreamManager {
    upstreams: RwLock<HashMap<String, Upstream>>,
}

impl UpstreamManager {
    /// Connect to all enabled upstreams concurrently. Servers that fail to
    /// connect are logged and omitted.
    pub async fn connect_all(configs: &[UpstreamConfig]) -> Self {
        let mut tasks = Vec::new();
        for cfg in configs {
            let name = cfg.name.clone();
            let spec = cfg.spec.clone();
            tasks.push(tokio::spawn(async move {
                let res = connect_and_list(&name, &spec).await;
                (name, res)
            }));
        }

        let mut upstreams = HashMap::new();
        for task in tasks {
            let (name, res) = match task.await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::error!(error = %e, "upstream connect task panicked");
                    continue;
                }
            };
            match res {
                Ok(up) => {
                    tracing::info!(server = %name, tools = up.tools.len(), "connected upstream");
                    upstreams.insert(name, up);
                }
                Err(e) => {
                    tracing::error!(server = %name, error = %e, "failed to connect upstream");
                }
            }
        }

        Self {
            upstreams: RwLock::new(upstreams),
        }
    }

    /// Connect a single upstream at runtime. Replaces any existing connection
    /// with the same name.
    pub async fn connect_one(&self, name: &str, spec: &ServerSpec) -> Result<usize, Error> {
        let up = connect_and_list(name, spec).await?;
        let count = up.tools.len();
        let mut guard = self.upstreams.write().await;
        if let Some(old) = guard.insert(name.to_string(), up) {
            // Drop the previous connection cleanly.
            let _ = old.service.cancel().await;
        }
        tracing::info!(server = %name, tools = count, "connected upstream (runtime)");
        Ok(count)
    }

    /// Disconnect a single upstream at runtime. Returns true if it was connected.
    pub async fn disconnect_one(&self, name: &str) -> bool {
        let removed = { self.upstreams.write().await.remove(name) };
        match removed {
            Some(up) => {
                if let Err(e) = up.service.cancel().await {
                    tracing::warn!(server = %name, error = %e, "error disconnecting upstream");
                }
                tracing::info!(server = %name, "disconnected upstream (runtime)");
                true
            }
            None => false,
        }
    }

    /// Whether the named upstream is currently connected.
    pub async fn is_connected(&self, name: &str) -> bool {
        self.upstreams.read().await.contains_key(name)
    }

    /// All tools across all connected upstreams, tagged with their server name.
    pub async fn all_tools(&self) -> Vec<NamespacedTool> {
        let guard = self.upstreams.read().await;
        let mut out = Vec::new();
        for (server, up) in guard.iter() {
            for tool in &up.tools {
                out.push(NamespacedTool {
                    server: server.clone(),
                    tool: tool.clone(),
                });
            }
        }
        out.sort_by(|a, b| {
            (a.server.as_str(), a.tool.name.as_ref())
                .cmp(&(b.server.as_str(), b.tool.name.as_ref()))
        });
        out
    }

    /// Route a tool call to `server`'s upstream.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, Error> {
        let guard = self.upstreams.read().await;
        let up = guard
            .get(server)
            .ok_or_else(|| Error::Upstream(format!("unknown upstream server: {server}")))?;

        let mut params = CallToolRequestParams::default();
        params.name = tool.to_string().into();
        params.arguments = arguments;

        up.service
            .call_tool(params)
            .await
            .map_err(|e| Error::Upstream(format!("{server}/{tool}: {e}")))
    }

    /// Gracefully disconnect every upstream (does not consume `self`).
    pub async fn shutdown(&self) {
        let drained: Vec<(String, Upstream)> =
            { self.upstreams.write().await.drain().collect() };
        for (name, up) in drained {
            if let Err(e) = up.service.cancel().await {
                tracing::warn!(server = %name, error = %e, "error during upstream shutdown");
            }
        }
    }
}

/// Connect to one upstream and list its tools.
async fn connect_and_list(name: &str, spec: &ServerSpec) -> Result<Upstream, Error> {
    let service = client::connect(name, spec).await?;
    let tools = match service.list_all_tools().await {
        Ok(tools) => tools,
        Err(e) => {
            let _ = service.cancel().await;
            return Err(Error::Upstream(format!("{name}: list_tools failed: {e}")));
        }
    };
    Ok(Upstream { service, tools })
}

/// Convenience wrapper so the manager can be shared across tasks.
pub type SharedUpstreams = Arc<UpstreamManager>;

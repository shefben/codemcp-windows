//! Admin interface: a Unix-domain-socket JSON-RPC channel for mutating the live
//! gateway's connected upstream set without restarting it.
//!
//! Line-delimited JSON: the client sends one request object, the server replies
//! with one response object, then the connection closes. Methods:
//!   - `list`    -> `{ servers: [ServerStatus] }`
//!   - `enable`  { name, make_default? } -> `{ name, connected, tools }`
//!   - `disable` { name, make_default? } -> `{ name, connected }`

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::error::Error;
use crate::runtime::Runtime;

/// Resolve the admin socket path (`CODEMCP_ADMIN_SOCKET` or XDG default).
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("CODEMCP_ADMIN_SOCKET") {
        return PathBuf::from(p);
    }
    crate::env::config_base().join("codemcp").join("admin.sock")
}

#[derive(Debug, Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EnableParams {
    pub name: String,
    #[serde(default)]
    pub make_default: bool,
}

/// Start the admin Unix-socket server. Binds the socket (removing any stale one),
/// sets 0600 perms, and serves requests until the process exits.
pub async fn serve(runtime: Runtime) -> Result<(), Error> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove a stale socket from a previous run.
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let listener = UnixListener::bind(&path)
        .map_err(|e| Error::Other(format!("admin socket bind {} failed: {e}", path.display())))?;
    set_perms(&path);
    tracing::info!(socket = %path.display(), "admin interface listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let rt = runtime.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, rt).await {
                        tracing::warn!(error = %e, "admin connection error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "admin accept failed");
            }
        }
    }
}

#[cfg(unix)]
fn set_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_perms(_path: &Path) {}

async fn handle_conn(stream: UnixStream, runtime: Runtime) -> Result<(), Error> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }

    let response = match serde_json::from_str::<Request>(&line) {
        Ok(req) => dispatch(&runtime, req).await,
        Err(e) => json!({ "error": format!("invalid request: {e}") }),
    };

    let mut out = serde_json::to_string(&response)?;
    out.push('\n');
    reader.get_mut().write_all(out.as_bytes()).await?;
    reader.get_mut().flush().await?;
    Ok(())
}

async fn dispatch(runtime: &Runtime, req: Request) -> Value {
    match req.method.as_str() {
        "list" => {
            let servers = runtime.list().await;
            json!({ "servers": servers })
        }
        "enable" => match serde_json::from_value::<EnableParams>(req.params) {
            Ok(p) => match runtime.enable(&p.name, p.make_default).await {
                Ok(tools) => json!({
                    "name": p.name,
                    "connected": true,
                    "tools": tools,
                    "made_default": p.make_default,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        "disable" => match serde_json::from_value::<EnableParams>(req.params) {
            Ok(p) => match runtime.disable(&p.name, p.make_default).await {
                Ok(was) => json!({
                    "name": p.name,
                    "connected": false,
                    "was_connected": was,
                    "made_default": p.make_default,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            },
            Err(e) => json!({ "error": format!("bad params: {e}") }),
        },
        other => json!({ "error": format!("unknown method: {other}") }),
    }
}

/// Client side: send one request to the admin socket and return the response.
pub async fn client_request(method: &str, params: Value) -> Result<Value, Error> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).await.map_err(|e| {
        Error::Other(format!(
            "cannot reach codemcp admin socket at {} ({e}). Is the gateway running?",
            path.display()
        ))
    })?;

    let mut reader = BufReader::new(stream);
    let mut req = serde_json::to_string(&json!({ "method": method, "params": params }))?;
    req.push('\n');
    reader.get_mut().write_all(req.as_bytes()).await?;
    reader.get_mut().flush().await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp: Value = serde_json::from_str(&line)
        .map_err(|e| Error::Other(format!("invalid admin response: {e}")))?;
    Ok(resp)
}

//! Loads the opencode-style `mcp.json` describing upstream MCP servers.
//!
//! Format (subset of opencode's `mcp` object):
//! ```json
//! {
//!   "mcp": {
//!     "github": { "type": "local", "command": ["npx","-y","..."], "environment": {"X":"y"} },
//!     "sentry": { "type": "remote", "url": "https://mcp.sentry.dev/mcp", "headers": {"Authorization":"Bearer {env:TOKEN}"} }
//!   }
//! }
//! ```
//! Values support `{env:VAR}` interpolation. Entries with `"enabled": false` are
//! skipped.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::Error;

/// Top-level config file shape. We only care about the `mcp` map.
#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub mcp: BTreeMap<String, ServerSpec>,
}

/// A single upstream server specification.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerSpec {
    Local {
        command: Vec<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        timeout: Option<u64>,
    },
    Remote {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

impl ServerSpec {
    pub fn enabled(&self) -> bool {
        match self {
            ServerSpec::Local { enabled, .. } | ServerSpec::Remote { enabled, .. } => {
                enabled.unwrap_or(true)
            }
        }
    }
}

/// A resolved (env-interpolated) upstream server plus its config `enabled` flag.
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub name: String,
    pub spec: ServerSpec,
    /// Whether this server is `enabled` in the config file (boot state).
    pub enabled: bool,
}

/// Load + parse the config file, interpolate `{env:VAR}`, and return only the
/// enabled servers (the boot-time set to connect at startup).
pub fn load(path: &Path) -> Result<Vec<UpstreamConfig>, Error> {
    Ok(load_all(path)?.into_iter().filter(|c| c.enabled).collect())
}

/// Load + parse every server in the config file (enabled and disabled), with
/// `{env:VAR}` interpolated. Used by the admin runtime so a currently-disabled
/// server can still be connected on demand.
pub fn load_all(path: &Path) -> Result<Vec<UpstreamConfig>, Error> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!("cannot read config {}: {e}", path.display()))
    })?;
    let mut file: ConfigFile = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("invalid config {}: {e}", path.display())))?;

    let mut out = Vec::new();
    for (name, mut spec) in std::mem::take(&mut file.mcp) {
        let enabled = spec.enabled();
        interpolate_spec(&mut spec)?;
        out.push(UpstreamConfig {
            name,
            spec,
            enabled,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Persist a server's `enabled` flag back to the config file, preserving all
/// other content verbatim. Used by admin commands with `--make-default`.
pub fn set_enabled(path: &Path, name: &str, enabled: bool) -> Result<(), Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("cannot read config {}: {e}", path.display())))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("invalid config {}: {e}", path.display())))?;

    let server = root
        .get_mut("mcp")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.get_mut(name))
        .and_then(|s| s.as_object_mut())
        .ok_or_else(|| Error::Config(format!("server {name:?} not found in {}", path.display())))?;
    server.insert("enabled".to_string(), serde_json::Value::Bool(enabled));

    let mut text = serde_json::to_string_pretty(&root)
        .map_err(|e| Error::Config(format!("serialize config failed: {e}")))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|e| Error::Config(format!("cannot write config {}: {e}", path.display())))?;
    Ok(())
}

fn interpolate_spec(spec: &mut ServerSpec) -> Result<(), Error> {
    match spec {
        ServerSpec::Local {
            command,
            environment,
            cwd,
            ..
        } => {
            for c in command.iter_mut() {
                *c = interpolate(c)?;
            }
            for v in environment.values_mut() {
                *v = interpolate(v)?;
            }
            if let Some(c) = cwd {
                *c = interpolate(c)?;
            }
        }
        ServerSpec::Remote { url, headers, .. } => {
            *url = interpolate(url)?;
            for v in headers.values_mut() {
                *v = interpolate(v)?;
            }
        }
    }
    Ok(())
}

/// Replace `{env:VAR}` occurrences with the value of `$VAR`. Missing vars are an
/// error so misconfiguration fails loudly at startup.
fn interpolate(s: &str) -> Result<String, Error> {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{env:") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        let end = after
            .find('}')
            .ok_or_else(|| Error::Config(format!("unterminated {{env:...}} in {s:?}")))?;
        let var = &after[..end];
        let val = std::env::var(var)
            .map_err(|_| Error::Config(format!("env var {var} referenced in config is not set")))?;
        result.push_str(&val);
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_env() {
        std::env::set_var("CODEMCP_TEST_TOKEN", "abc123");
        let out = interpolate("Bearer {env:CODEMCP_TEST_TOKEN}").unwrap();
        assert_eq!(out, "Bearer abc123");
    }

    #[test]
    fn missing_env_errors() {
        assert!(interpolate("{env:CODEMCP_DEFINITELY_UNSET_XYZ}").is_err());
    }

    #[test]
    fn no_placeholder_passthrough() {
        assert_eq!(interpolate("plain").unwrap(), "plain");
    }
}

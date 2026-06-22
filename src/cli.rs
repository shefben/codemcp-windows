//! Command-line interface (clap). With no subcommand, codemcp runs as the MCP
//! gateway. The `list`/`enable`/`disable` subcommands are a thin admin client
//! that talks to a running gateway over its Unix admin socket.

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::admin;
use crate::error::Error;
use crate::setup::{self, Harness};

#[derive(Parser)]
#[command(
    name = "codemcp",
    about = "Meta-MCP code-mode gateway. Run with no subcommand to start the server.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// List configured upstream servers and their live connection status.
    List,
    /// Connect an upstream in the running gateway (no restart).
    Enable {
        /// Server name as it appears in mcp.json.
        name: String,
        /// Also persist `enabled: true` to mcp.json (changes boot default).
        #[arg(long)]
        make_default: bool,
    },
    /// Disconnect an upstream in the running gateway (no restart).
    Disable {
        /// Server name as it appears in mcp.json.
        name: String,
        /// Also persist `enabled: false` to mcp.json (changes boot default).
        #[arg(long)]
        make_default: bool,
    },
    /// Wire codemcp into an agent harness: back up its config, move its MCP
    /// servers into codemcp's mcp.json, and point the harness at codemcp.
    Setup {
        /// Harness to set up. Supported: opencode.
        harness: Harness,
    },
}

impl Command {
    /// Whether this command is handled synchronously without the gateway/admin
    /// socket (i.e. `setup`).
    pub fn is_local(&self) -> bool {
        matches!(self, Command::Setup { .. })
    }
}

/// Run a local (non-admin) subcommand. Currently just `setup`.
pub fn run_local(cmd: Command) -> Result<(), Error> {
    match cmd {
        Command::Setup { harness } => setup::run(harness),
        _ => unreachable!("run_local called with a non-local command"),
    }
}

/// Run an admin subcommand against a live gateway. Prints human-readable output.
pub async fn run_admin(cmd: Command) -> Result<(), Error> {
    match cmd {
        Command::List => {
            let resp = admin::client_request("list", json!({})).await?;
            print_list(&resp);
        }
        Command::Enable { name, make_default } => {
            let resp = admin::client_request(
                "enable",
                json!({ "name": name, "make_default": make_default }),
            )
            .await?;
            print_action("enabled", &resp);
        }
        Command::Disable { name, make_default } => {
            let resp = admin::client_request(
                "disable",
                json!({ "name": name, "make_default": make_default }),
            )
            .await?;
            print_action("disabled", &resp);
        }
        Command::Setup { .. } => unreachable!("setup is handled by run_local"),
    }
    Ok(())
}

fn print_list(resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let servers = match resp.get("servers").and_then(Value::as_array) {
        Some(s) => s,
        None => {
            println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
            return;
        }
    };
    println!(
        "{:<22} {:<7} {:<9} {:<10} {}",
        "NAME", "TYPE", "DEFAULT", "CONNECTED", "TOOLS"
    );
    for s in servers {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = s.get("kind").and_then(Value::as_str).unwrap_or("?");
        let enabled = s
            .get("enabled_in_config")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let connected = s.get("connected").and_then(Value::as_bool).unwrap_or(false);
        let tools = s.get("tools").and_then(Value::as_u64).unwrap_or(0);
        println!(
            "{:<22} {:<7} {:<9} {:<10} {}",
            name,
            kind,
            if enabled { "yes" } else { "no" },
            if connected { "yes" } else { "no" },
            tools
        );
    }
}

fn print_action(verb: &str, resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let name = resp.get("name").and_then(Value::as_str).unwrap_or("?");
    let made_default = resp
        .get("made_default")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut msg = format!("{name} {verb}");
    if let Some(tools) = resp.get("tools").and_then(Value::as_u64) {
        msg.push_str(&format!(" ({tools} tools)"));
    }
    if made_default {
        msg.push_str(" [persisted to mcp.json]");
    }
    println!("{msg}");
}

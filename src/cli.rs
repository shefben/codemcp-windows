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

/// Selects which running gateway an admin command targets when more than one is
/// live. Matches a substring of the config path, or an exact PID.
#[derive(clap::Args, Clone, Default)]
pub struct InstanceSel {
    /// Target a specific gateway by launcher name, config-path substring, or
    /// PID (only needed when multiple gateways are running).
    #[arg(short = 'i', long, global = true)]
    pub instance: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a long-lived Streamable HTTP gateway on a fixed port, suitable for
    /// sharing one codemcp instance between multiple harnesses.
    Start {
        /// TCP port to bind the HTTP MCP endpoint on. Fails if already in use.
        #[arg(short = 'p', long, default_value_t = crate::env::DEFAULT_HTTP_PORT)]
        port: u16,
        /// Address to bind (default 127.0.0.1).
        #[arg(short = 'H', long, default_value = "127.0.0.1")]
        host: String,
    },
    /// List all running codemcp gateways (one per harness, plus any `start`ed).
    Instances,
    /// List configured upstream servers and their live connection status.
    List {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Print the `execute_python` tool definition (name + description +
    /// inputSchema) as the model sees it in `tools/list`.
    Tool {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Print the generated `sdk.py` (the typed Python SDK preloaded into the
    /// worker).
    Sdk {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Connect an upstream in the running gateway (no restart).
    Enable {
        /// Server name as it appears in mcp.json.
        name: String,
        /// Also persist `enabled: true` to mcp.json (changes boot default).
        #[arg(short = 'd', long)]
        make_default: bool,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Disconnect an upstream in the running gateway (no restart).
    Disable {
        /// Server name as it appears in mcp.json.
        name: String,
        /// Also persist `enabled: false` to mcp.json (changes boot default).
        #[arg(short = 'd', long)]
        make_default: bool,
        #[command(flatten)]
        instance: InstanceSel,
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

    /// Whether this command runs the gateway itself (i.e. `start`).
    pub fn is_gateway(&self) -> bool {
        matches!(self, Command::Start { .. })
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
        Command::Instances => {
            let instances = admin::live_instances().await;
            if instances.is_empty() {
                println!("no running codemcp gateways found");
            } else {
                println!("{:<14} {:<8} CONFIG", "LAUNCHER", "PID");
                for i in &instances {
                    println!("{:<14} {:<8} {}", i.launcher, i.pid, i.config);
                }
            }
        }
        Command::List { instance } => {
            let target = admin::select_instance(instance.instance.as_deref()).await?;
            println!("# gateway [{}] pid {}", target.launcher, target.pid);
            let resp = admin::client_request(instance.instance.as_deref(), "list", json!({})).await?;
            print_list(&resp);
        }
        Command::Tool { instance } => {
            let target = admin::select_instance(instance.instance.as_deref()).await?;
            let _ = target;
            let resp = admin::client_request(instance.instance.as_deref(), "tool", json!({}))
                .await?;
            if let Some(err) = resp.get("error").and_then(Value::as_str) {
                eprintln!("error: {err}");
            } else if let Some(tool) = resp.get("tool") {
                println!("{}", serde_json::to_string_pretty(tool).unwrap_or_default());
            } else {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
            }
        }
        Command::Sdk { instance } => {
            admin::select_instance(instance.instance.as_deref()).await?;
            let resp = admin::client_request(instance.instance.as_deref(), "sdk", json!({}))
                .await?;
            if let Some(err) = resp.get("error").and_then(Value::as_str) {
                eprintln!("error: {err}");
            } else if let Some(sdk) = resp.get("sdk").and_then(Value::as_str) {
                print!("{sdk}");
            } else {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
            }
        }
        Command::Enable {
            name,
            make_default,
            instance,
        } => {
            let resp = admin::client_request(
                instance.instance.as_deref(),
                "enable",
                json!({ "name": name, "make_default": make_default }),
            )
            .await?;
            print_action("enabled", &resp);
        }
        Command::Disable {
            name,
            make_default,
            instance,
        } => {
            let resp = admin::client_request(
                instance.instance.as_deref(),
                "disable",
                json!({ "name": name, "make_default": make_default }),
            )
            .await?;
            print_action("disabled", &resp);
        }
        Command::Start { .. } => unreachable!("start is handled by run_gateway"),
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

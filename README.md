# codemcp — Meta-MCP "Code-Mode" Gateway

A single MCP server that connects to many upstream MCP servers and exposes
**one tool: `execute_python`**. Agents write Python that calls every upstream
tool as a typed function and transform/combine results in-process — instead of
issuing many sequential MCP tool calls.

```
agent harness ──MCP──► codemcp gateway ──MCP clients──► upstream servers
   (stdio/HTTP)         (one tool: execute_python)       (github, sentry, …)
                              │                            (stdio / streamable-http)
                              ├─ generates a typed Python SDK (1 fn per upstream tool)
                              ├─ runs Python in a worker process
                              └─ SDK fns call back into the gateway → route to upstream
```

The agent's LLM sees **only** `execute_python`. Its description contains a short
intro plus **two lines per upstream tool**: the full typed Python signature and a
one-line summary. The SDK itself is preloaded into the Python runtime once — it is
**never** concatenated into the per-call code string.

## Why

A typical agent task ("find the open issue mentioning X, fetch its linked PR, and
summarize the diff") becomes three or more round-trips through the model, each
re-sending tool schemas and intermediate JSON. With codemcp the agent writes one
Python snippet that calls the three tools and returns just the summary:

```python
issue = github_search_issues(query="X", state="open")[0]
pr = github_get_pull_request(number=issue["linked_pr"])
result = {"title": pr["title"], "files_changed": len(pr["files"])}
```

One model turn, one tool call, only the final result returned.

## Status

Working vertical slice over **stdio** and **Streamable HTTP**:

- Connects to upstream MCP servers (stdio + Streamable HTTP), discovers tools.
- Generates a typed Python SDK from each tool's JSON Schema.
- Exposes a single `execute_python` MCP tool whose description carries the SDK.
- Runs user Python in a persistent host CPython worker; SDK calls round-trip back
  to the gateway over an authenticated WebSocket control channel and are routed to
  the right upstream server.

Isolation backends beyond the host process (Docker, Monty) and optional LLM tool
summaries are planned — see [TODO](#todo--planned-work).

## Install

### One-line install (prebuilt binary)

```sh
curl -fsSL https://raw.githubusercontent.com/basedatum/codemcp/main/install.sh | sh
```

This downloads a prebuilt binary for your OS/arch from
[GitHub Releases](https://github.com/basedatum/codemcp/releases), verifies its
SHA-256 checksum, and installs it to `~/.local/bin` (or `/usr/local/bin`).
Supported platforms: macOS (arm64, x86_64) and Linux (arm64, x86_64).

Useful overrides:

```sh
# pin a version and/or choose the install dir
curl -fsSL https://raw.githubusercontent.com/basedatum/codemcp/main/install.sh \
  | CODEMCP_VERSION=v0.1.0 CODEMCP_BIN_DIR="$HOME/bin" sh
```

| Variable          | Purpose                                            |
| ----------------- | -------------------------------------------------- |
| `CODEMCP_VERSION` | Release tag to install (default: latest)           |
| `CODEMCP_BIN_DIR` | Install directory                                  |
| `CODEMCP_REPO`    | `owner/repo` to download from (default `basedatum/codemcp`) |

> opencode launches `codemcp` by bare name, so the install dir must be on your
> `PATH`. The installer prints the exact line to add if it isn't.

### Build from source

Requires a Rust toolchain.

```sh
make install                 # release build, install onto PATH (/usr/local/bin)
make install PREFIX=~/.local # install somewhere else
make uninstall               # remove it
make help                    # list all targets
```

Or with cargo directly: `cargo install --path .`.

## Quick start

### Set up from an existing harness (opencode)

If you already have MCP servers configured in opencode, let codemcp adopt them:

```bash
codemcp setup opencode
```

This backs up `~/.config/opencode/opencode.json`, **moves its `mcp` section
verbatim** into codemcp's `mcp.json`, and rewrites opencode to launch a single
`codemcp` server instead of all the individual ones. Restart opencode afterward.
(`codemcp` must be on your `PATH`, since opencode launches it by bare name.) Only
`opencode` is supported today; more harnesses can be added later.

### Or configure manually

1. Write a config at `~/.config/codemcp/mcp.json` (XDG; override with
   `CODEMCP_CONFIG`). The format is a subset of opencode's `mcp` object:

   ```json
   {
     "mcp": {
       "everything": {
         "type": "local",
         "command": ["npx", "-y", "@modelcontextprotocol/server-everything"]
       },
       "sentry": {
         "type": "remote",
         "url": "https://mcp.sentry.dev/mcp",
         "headers": { "Authorization": "Bearer {env:SENTRY_TOKEN}" }
       }
     }
   }
   ```

   - `type: "local"` → stdio server launched via `command` (with optional
     `environment`, `cwd`).
   - `type: "remote"` → Streamable HTTP server at `url` (with optional `headers`).
   - Any string value supports `{env:VAR}` interpolation.
   - `"enabled": false` skips an entry.

2. Run the gateway:

   ```bash
   # stdio (default) — for an agent harness that launches codemcp as a subprocess
   codemcp

   # Streamable HTTP
   CODEMCP_TRANSPORT=http CODEMCP_HTTP_BIND=127.0.0.1:3388 codemcp
   ```

3. Point your MCP client at it. Inspect the generated SDK and tool description
   without serving:

   ```bash
   CODEMCP_DUMP=1 codemcp
   ```

## Enabling/disabling upstreams at runtime

`mcp.json` is the **boot-time** desired state. While the gateway is running you
can connect or disconnect upstreams **without restarting it** using the admin
subcommands, which talk to the running gateway over its Unix admin socket:

```bash
codemcp list                 # show every configured server + live status
codemcp enable github        # connect 'github' now (runtime only)
codemcp disable github       # disconnect 'github' now (runtime only)
```

```
NAME                   TYPE    DEFAULT   CONNECTED  TOOLS
github                 local   yes       yes        45
brave                  local   no        no         0
```

- `DEFAULT` = the `enabled` flag in `mcp.json` (what connects at boot).
- `CONNECTED` = whether it is connected in the running process right now.

By default admin commands change **only the live process** and do **not** touch
`mcp.json`. To also persist the change as the new boot default, pass
`--make-default`:

```bash
codemcp enable brave --make-default    # connect now AND set enabled:true in mcp.json
codemcp disable github --make-default  # disconnect now AND set enabled:false in mcp.json
```

When an upstream is enabled/disabled, codemcp regenerates the Python SDK,
hot-reloads it into the running worker (no worker restart, no lost state), and
sends a `notifications/tools/list_changed` to connected MCP clients so they
re-read the updated `execute_python` description.

> Note: `--make-default` rewrites `mcp.json` (preserving all values) and may
> reorder keys alphabetically.

## How it works

1. **Connect & discover.** On startup codemcp connects to every enabled upstream
   server and lists its tools.
2. **Generate the SDK.** Each tool's JSON Schema becomes a typed Python function
   (`server_tool_name(arg: type, ...)`). Tool names are sanitized to valid Python
   identifiers. The generated `sdk.py` is validated as parseable Python.
3. **Expose one tool.** The gateway serves a single `execute_python` tool. Its
   description is the intro + two lines per upstream tool (signature + summary).
4. **Execute.** A persistent Python worker process imports `sdk.py` once. Each
   `execute_python` call sends the user's code to the worker, which runs it and
   returns `{ result, stdout, stderr }`. Assign to `result` (or leave a final
   expression) to return a value.
5. **Route SDK calls.** When user code calls an SDK function, the worker sends a
   `call_tool` request back to the gateway over the WebSocket control channel; the
   gateway forwards it to the right upstream MCP server and returns the result.

### Control channel

The gateway runs a WebSocket server (loopback by default). The worker connects as
a client and, as its **first message**, sends a shared auth token
(`CODEMCP_CONTROL_TOKEN`, auto-generated per run if unset). JSON-RPC 2.0 messages
then flow both ways on the one connection:

- gateway → worker: `run { code }`
- worker → gateway: `call_tool { server, tool, args }`

One protocol covers host loopback, future Docker workers (Linux + macOS), and
future remote workers, and is natively bidirectional with built-in message
framing.

### Self-provisioning worker

`bootstrap.py` provisions its own `websockets` dependency (into a cache dir via
`pip install --target`) if it is missing, so the worker runs on any stock Python
host or container without a custom image. Controlled by `CODEMCP_WS_*`.

## Configuration

All settings are read once at startup from `CODEMCP_*` environment variables.

### Core

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_CONFIG` | `~/.config/codemcp/mcp.json` | Path to the upstream `mcp.json`. |
| `CODEMCP_ISOLATION` | `HOST_SYSTEM` | Execution isolation: `HOST_SYSTEM`, `DOCKER`, `MONTY`. Only `HOST_SYSTEM` is implemented today. |
| `CODEMCP_TRANSPORT` | `stdio` | Downstream MCP transport: `stdio` or `http`. |
| `CODEMCP_ADMIN_SOCKET` | `~/.config/codemcp/admin.sock` | Unix socket for the admin CLI (`list`/`enable`/`disable`). Both the gateway and the CLI honor it. |
| `CODEMCP_LOG` | `info` | Tracing filter (e.g. `info`, `debug`, `codemcp=debug`). |
| `CODEMCP_PYTHON` | _(auto)_ | Path to the Python interpreter (defaults to `python3`/`python` on `PATH`). |

### Streamable HTTP transport

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_HTTP_BIND` | `127.0.0.1:3388` | Address to bind the HTTP server. |
| `CODEMCP_HTTP_PATH` | `/mcp` | URL path the MCP endpoint is mounted at. |
| `CODEMCP_HTTP_JSON_RESPONSE` | `false` | `true` = stateless plain `application/json` replies; `false` = stateful SSE with session IDs. |

### Control channel

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_CONTROL_BIND` | `127.0.0.1:0` | Address for the WebSocket control server (`:0` = ephemeral port). |
| `CODEMCP_CONTROL_HOST_FOR_WORKER` | _(bind IP)_ | Host the worker uses to reach the control server (override for containers/remote). |
| `CODEMCP_CONTROL_TOKEN` | _(random per run)_ | Shared secret the worker must send as its first WS frame. |

### Worker dependency provisioning

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_WS_AUTO_INSTALL` | `true` | Self-install `websockets` into a cache dir if missing. |
| `CODEMCP_WS_VERSION` | _(unset)_ | Pin the `websockets` version. |
| `CODEMCP_WS_PIP_ARGS` | _(empty)_ | Extra args passed to `pip install` (whitespace-split). |

### Execution limits

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_EXEC_TIMEOUT_MS` | `30000` | Per-`run` execution timeout in milliseconds. |
| `CODEMCP_MAX_OUTPUT_BYTES` | `1048576` | Max captured stdout/stderr bytes. |

### Docker isolation (planned — see TODO)

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_DOCKER_IMAGE` | `python:3.14-slim` | Base image for the Docker executor. |
| `CODEMCP_DOCKER_EXTRA_ARGS` | _(empty)_ | Extra `docker run` args (whitespace-split). |

### Monty isolation (planned — see TODO)

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_MONTY_MEM_LIMIT` | `268435456` | Memory limit (bytes) for the Monty sandbox. |

### LLM tool summaries (planned — see TODO)

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_ENABLE_LLM_SUMMARIES` | `false` | Condense upstream tool descriptions via one cached LLM call per tool. |
| `CODEMCP_SUMMARY_MODEL` | _(unset)_ | Model to use for summaries. |
| `CODEMCP_SUMMARY_API_BASE` | _(unset)_ | API base URL for the summary model. |
| `CODEMCP_SUMMARY_API_KEY` | _(unset)_ | API key for the summary model. |
| `CODEMCP_SUMMARY_CACHE` | `~/.cache/codemcp/summaries.json` | Summary cache file. |

## Isolation modes & security boundaries

The `execute_python` tool runs arbitrary code **and** can call any connected
upstream MCP server. Choose isolation based on how much you trust the agent.

| Mode | Status | Isolation | Use when |
|---|---|---|---|
| `HOST_SYSTEM` | **implemented** | **None** — full host access with the gateway's privileges, full stdlib + installed packages. | Development / trusted agents only. |
| `DOCKER` | planned | OS-level container; only the authenticated WebSocket control channel bridges in. | Untrusted agents (recommended). |
| `MONTY` | planned | Strict in-process sandbox: no filesystem/network/env except the SDK callbacks the gateway grants. Limited Python subset (no classes, no third-party libs, partial stdlib). | Maximum safety; simple transform code. |

- **`HOST_SYSTEM` has no sandbox.** It executes with the gateway's privileges.
  Run it only with agents and tools you trust.
- **Control channel auth.** Because the control channel both executes arbitrary
  code and routes to authenticated upstreams, it is gated by a per-run shared
  token (`CODEMCP_CONTROL_TOKEN`, sent as the first WS frame). It binds loopback by
  default. **Never** expose the control port publicly without TLS and a strong
  token.
- **HTTP transport.** The Streamable HTTP server validates the `Host` header
  against a loopback allow-list by default to prevent DNS-rebinding attacks. Set
  appropriate hosts/origins before any non-loopback deployment, and front it with
  TLS + authentication.

## TODO / planned work

These phases are designed but not yet implemented. Configuration knobs already
exist (see tables above) but are inert until the backends land.

### Phase 7 — Docker isolation (`exec/docker.rs`)

Run the Python worker inside a Docker container instead of the host. The same
`Executor` trait and WebSocket control channel apply; the container reaches the
gateway via `CODEMCP_CONTROL_HOST_FOR_WORKER` (e.g. `host.docker.internal`). The
worker self-provisions `websockets`, so the stock `python:3.x-slim` image works
with no custom build. Honors `CODEMCP_DOCKER_IMAGE` and
`CODEMCP_DOCKER_EXTRA_ARGS`. This is the recommended isolation for untrusted
agents.

### Phase 8 — Monty isolation (`exec/monty.rs`, feature-gated)

An in-process, safe-by-construction sandbox using
[Monty](https://github.com/pydantic/monty) (pinned to `=0.0.18`). SDK calls are
exposed as Monty `external_functions` rather than over the WebSocket. Opt-in via
`CODEMCP_ISOLATION=MONTY` and the `monty` cargo feature (off by default; the crate
is pulled from git). Monty is a limited Python subset (no classes, no third-party
libraries, partial stdlib), so it suits simple transform code, not arbitrary
scripts. Memory bounded by `CODEMCP_MONTY_MEM_LIMIT`.

### Phase 9 — LLM tool summaries + cache

Optionally condense verbose upstream tool descriptions into a single tight summary
line per tool via one cached LLM call (`CODEMCP_ENABLE_LLM_SUMMARIES`,
`CODEMCP_SUMMARY_*`). Default behavior stays fully offline, using each tool's own
`description`.

## Development

```bash
cargo build
cargo test

# Inspect generated SDK + tool description for a given config
CODEMCP_CONFIG=/path/to/mcp.json CODEMCP_DUMP=1 cargo run

# One-shot smoke test: run a Python snippet against the worker and exit
CODEMCP_CONFIG=/path/to/mcp.json \
  CODEMCP_SMOKE='print(everything_get_sum(a=2, b=40))' cargo run
```

Requires a Python 3 interpreter on `PATH` (3.14 tested) and, for stdio upstreams,
whatever launcher their `command` needs (e.g. `npx`, `uvx`).

## License

MIT

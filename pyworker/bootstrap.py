#!/usr/bin/env python3
"""codemcp Python worker.

Self-provisions the `websockets` package if missing, connects to the gateway's
WebSocket control channel, authenticates with a shared token, then serves `run`
requests by executing user code. User code calls the generated SDK functions,
each of which dispatches a `call_tool` request back to the gateway over the same
WebSocket.

Environment (set by the gateway):
  CODEMCP_CONTROL_URL        ws://host:port  (required)
  CODEMCP_CONTROL_TOKEN      shared secret    (required)
  CODEMCP_SDK_DIR            dir containing generated sdk.py (added to sys.path)
  CODEMCP_WS_AUTO_INSTALL    "true"/"false"   (default true)
  CODEMCP_WS_VERSION         pin version
  CODEMCP_WS_PIP_ARGS        extra pip args (space separated)
  CODEMCP_WS_CACHE_DIR       dir for the self-installed websockets package
"""

import asyncio
import contextlib
import io
import json
import os
import subprocess
import sys
import threading
import traceback


def _ensure_websockets():
    """Import `websockets`, installing it into a private dir if necessary."""
    try:
        import websockets  # noqa: F401
        return
    except ImportError:
        pass

    if os.environ.get("CODEMCP_WS_AUTO_INSTALL", "true").lower() not in ("1", "true", "yes", "on"):
        sys.stderr.write("codemcp worker: websockets missing and auto-install disabled\n")
        sys.exit(3)

    cache_dir = os.environ.get("CODEMCP_WS_CACHE_DIR") or os.path.join(
        os.path.expanduser("~"), ".cache", "codemcp", "pylib"
    )
    os.makedirs(cache_dir, exist_ok=True)

    if cache_dir not in sys.path:
        sys.path.insert(0, cache_dir)
    try:
        import websockets  # noqa: F401
        return
    except ImportError:
        pass

    pkg = "websockets"
    version = os.environ.get("CODEMCP_WS_VERSION")
    if version:
        pkg = f"websockets=={version}"
    cmd = [sys.executable, "-m", "pip", "install", "--target", cache_dir, pkg]
    extra = os.environ.get("CODEMCP_WS_PIP_ARGS", "").split()
    cmd.extend(extra)

    sys.stderr.write(f"codemcp worker: installing {pkg} into {cache_dir}\n")
    result = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode != 0:
        sys.stderr.write(result.stdout.decode("utf-8", "replace"))
        sys.stderr.write("codemcp worker: failed to install websockets\n")
        sys.exit(3)

    import websockets  # noqa: F401


_ensure_websockets()

import websockets  # noqa: E402


# ── concurrent execution context ─────────────────────────────
#
# User code runs in a worker thread; the WebSocket lives on the asyncio event
# loop in the main thread. An SDK call *fires* its `call_tool` request onto the
# loop immediately (so the request is on the wire right away) and returns a
# `Pending` handle that resolves lazily — it blocks only when user code first
# reads its value (attribute, index, iteration, etc.).
#
# This gives concurrency for free: any calls issued before a result is read are
# already in flight by the time the first value is accessed, so their round-trips
# overlap. No special syntax is required.

# Per-worker-thread handle to the event loop, so a `Pending` can reach it to
# resolve even though it runs on the worker thread.
_thread_local = threading.local()


class ToolError(RuntimeError):
    """Raised when an upstream tool call fails."""


class Pending:
    """A handle to an in-flight `call_tool` request.

    The request is already on the wire. The result is fetched lazily and cached
    on first access. Most value-like operations (indexing, attribute access,
    iteration, truthiness, str/repr, equality) transparently resolve, so in the
    common case user code can treat a `Pending` exactly like the returned value.

    Call ``.result()`` to force resolution explicitly.
    """

    __slots__ = ("_fut", "_loop", "_server", "_tool", "_resolved", "_value")

    def __init__(self, fut, loop, server, tool):
        self._fut = fut
        self._loop = loop
        self._server = server
        self._tool = tool
        self._resolved = False
        self._value = None

    def result(self, timeout=None):
        """Block until the round-trip completes and return the unwrapped value."""
        if self._resolved:
            return self._value
        msg = asyncio.run_coroutine_threadsafe(
            _await_future(self._fut), self._loop
        ).result(timeout)
        if isinstance(msg, dict) and msg.get("error") is not None:
            raise ToolError(f"tool {self._server}/{self._tool} failed: {msg['error']}")
        value = _unwrap_tool_result(msg.get("result") if isinstance(msg, dict) else msg)
        self._value = value
        self._resolved = True
        return value

    # ── transparent resolution for ergonomic sequential use ──
    def __getitem__(self, key):
        return self.result()[key]

    def __getattr__(self, name):
        # Internal slot names are served by the descriptor protocol and never
        # reach here once set. Guard against recursion if accessed before init
        # completes (e.g. during unpickling) and shield dunder lookups so the
        # proxy doesn't accidentally claim to implement arbitrary protocols.
        if name.startswith("__") and name.endswith("__"):
            raise AttributeError(name)
        return getattr(self.result(), name)

    def __iter__(self):
        return iter(self.result())

    def __len__(self):
        return len(self.result())

    def __contains__(self, item):
        return item in self.result()

    def __bool__(self):
        return bool(self.result())

    def __eq__(self, other):
        return self.result() == other

    def __ne__(self, other):
        return self.result() != other

    def __hash__(self):
        return hash(self.result())

    def __repr__(self):
        if self._resolved:
            return repr(self._value)
        return f"<Pending {self._server}/{self._tool}>"

    def __str__(self):
        return str(self.result())


class Dispatcher:
    """Bridges synchronous user-code SDK calls to the async WebSocket.

    An SDK call schedules a `call_tool` request on the event loop (firing it
    immediately) and returns a `Pending` handle. The handle blocks only when its
    value is actually needed, which is what lets independent calls overlap.
    """

    def __init__(self, ws, loop):
        self._ws = ws
        self._loop = loop
        self._pending = {}
        self._counter = 0
        self._lock = threading.Lock()

    def _next_id(self):
        with self._lock:
            self._counter += 1
            return f"ct-{self._counter}"

    async def handle_response(self, msg):
        rid = msg.get("id")
        fut = self._pending.pop(rid, None)
        if fut is not None and not fut.done():
            fut.set_result(msg)

    def call_tool(self, server, tool, args):
        """Fire a `call_tool` request and return a lazily-resolved `Pending`."""
        rid = self._next_id()
        request = {
            "jsonrpc": "2.0",
            "id": rid,
            "method": "call_tool",
            "params": {"server": server, "tool": tool, "args": args},
        }

        fut = self._loop.create_future()
        self._pending[rid] = fut

        async def _send():
            await self._ws.send(json.dumps(request))

        # Fire now: the request is on the wire before we return.
        asyncio.run_coroutine_threadsafe(_send(), self._loop)
        return Pending(fut, self._loop, server, tool)


async def _await_future(fut):
    return await fut


def _unwrap_tool_result(result):
    """Turn an MCP CallToolResult into something ergonomic for Python.

    Prefer structuredContent; else join text content; else return raw.
    """
    if not isinstance(result, dict):
        return result
    if result.get("structuredContent") is not None:
        return result["structuredContent"]
    content = result.get("content")
    if isinstance(content, list):
        texts = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                texts.append(item.get("text", ""))
        if texts:
            joined = "\n".join(texts)
            # Try to parse JSON text payloads for convenience.
            try:
                return json.loads(joined)
            except (ValueError, TypeError):
                return joined
    return result


def _exec_user_code(code, sdk_module, dispatcher):
    """Execute user code, returning (result, stdout, stderr, error)."""
    namespace = {"__name__": "__codemcp__"}
    # Inject SDK functions directly — each returns a Pending when called.
    for name in dir(sdk_module):
        if not name.startswith("_"):
            namespace[name] = getattr(sdk_module, name)

    # Wire dispatch into the SDK module.
    sdk_module._codemcp_dispatch = dispatcher.call_tool

    namespace["Pending"] = Pending
    namespace["ToolError"] = ToolError
    # Remember the loop so Pending can reach it from this thread.
    _thread_local._loop = dispatcher._loop

    out, err = io.StringIO(), io.StringIO()
    result_value = None
    error = None
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            compiled = compile(code, "<codemcp>", "exec")
            exec(compiled, namespace)
            # Convention: `result` variable, else None.
            result_value = namespace.get("result")
            # If the result is still a Pending, resolve it before returning.
            if isinstance(result_value, Pending):
                result_value = result_value.result()
    except Exception:
        error = traceback.format_exc()

    return result_value, out.getvalue(), err.getvalue(), error


def _json_safe(value):
    if isinstance(value, Pending):
        value = value.result()
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError):
        return repr(value)


class SdkHolder:
    """Mutable container for the current SDK module so `reload` can swap it."""

    def __init__(self, module, sdk_dir):
        self.module = module
        self.sdk_dir = sdk_dir

    def reload(self, source):
        """Overwrite sdk.py with `source` and re-import the module.

        The write is best-effort: on a read-only filesystem (the Docker backend
        bind-mounts the workdir read-only), the gateway has already updated the
        mounted file, so we skip the write and just re-import it. On a writable
        filesystem (HOST backend) the write is what materializes the new SDK.
        """
        import importlib

        path = os.path.join(self.sdk_dir, "sdk.py")
        try:
            with open(path, "w") as f:
                f.write(source)
        except OSError:
            # Read-only mount: the gateway owns the file and has already
            # written the new source there. Re-import picks up that content.
            pass
        # Drop any cached bytecode so the new source is used.
        importlib.invalidate_caches()
        self.module = importlib.reload(self.module)


async def main():
    url = os.environ["CODEMCP_CONTROL_URL"]
    token = os.environ["CODEMCP_CONTROL_TOKEN"]
    sdk_dir = os.environ.get("CODEMCP_SDK_DIR", ".")

    if sdk_dir not in sys.path:
        sys.path.insert(0, sdk_dir)
    import sdk as sdk_module  # generated

    holder = SdkHolder(sdk_module, sdk_dir)
    loop = asyncio.get_running_loop()

    async with websockets.connect(url, max_size=None) as ws:
        # First frame: auth token.
        await ws.send(token)

        dispatcher = Dispatcher(ws, loop)

        async for raw in ws:
            try:
                msg = json.loads(raw)
            except ValueError:
                continue

            method = msg.get("method")
            if method == "run":
                # Run user code as a background task so the read loop keeps
                # servicing the SDK's call_tool round-trips while it executes.
                asyncio.create_task(_handle_run(ws, msg, holder, dispatcher))
            elif method == "reload":
                await _handle_reload(ws, msg, holder)
            elif method is None:
                # A response to one of our call_tool requests.
                await dispatcher.handle_response(msg)


async def _handle_reload(ws, msg, holder):
    rid = msg.get("id")
    source = msg.get("params", {}).get("sdk", "")
    error = None
    try:
        await asyncio.to_thread(holder.reload, source)
    except Exception:
        error = traceback.format_exc()
    response = {
        "jsonrpc": "2.0",
        "id": rid,
        "result": {"result": None, "stdout": "", "stderr": "", "error": error},
    }
    await ws.send(json.dumps(response))


async def _handle_run(ws, msg, holder, dispatcher):
    code = msg.get("params", {}).get("code", "")
    rid = msg.get("id")
    # Run user code off the event loop so SDK calls can round-trip.
    result_value, stdout, stderr, error = await asyncio.to_thread(
        _exec_user_code, code, holder.module, dispatcher
    )
    response = {
        "jsonrpc": "2.0",
        "id": rid,
        "result": {
            "result": _json_safe(result_value),
            "stdout": stdout,
            "stderr": stderr,
            "error": error,
        },
    }
    await ws.send(json.dumps(response))


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    except Exception as exc:  # pragma: no cover
        # The gateway closing the control channel is a normal shutdown signal;
        # exit quietly rather than dumping a traceback.
        import websockets.exceptions as _wse

        if isinstance(exc, (_wse.ConnectionClosed, ConnectionError, OSError)):
            sys.exit(0)
        raise

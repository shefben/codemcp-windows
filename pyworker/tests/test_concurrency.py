#!/usr/bin/env python3
"""Self-test for the worker's Pending / concurrent execution model.

Runs entirely offline: it stands up a real asyncio event loop in a background
thread (mirroring the worker's main loop) and a fake Dispatcher whose responses
resolve after a simulated network delay. It then drives `_exec_user_code` exactly
as the real worker does, asserting:

  1. Sequential calls return real values transparently (a Pending resolves on use).
  2. Calls issued before any result is read overlap (wall time ≈ one delay).
  3. Reading each result before the next call is made serializes (control case).
  4. Tool errors surface as ToolError.
  5. A top-level `result` that is still a Pending is resolved before return.
  6. List comprehensions that issue then read run concurrently.

Run:  python3 pyworker/tests/test_concurrency.py
Exits non-zero on failure.
"""

import asyncio
import os
import sys
import threading
import types

# Avoid the websockets self-install path during import.
os.environ.setdefault("CODEMCP_WS_AUTO_INSTALL", "false")
sys.modules.setdefault("websockets", types.ModuleType("websockets"))

HERE = os.path.dirname(os.path.abspath(__file__))
PYWORKER = os.path.dirname(HERE)
sys.path.insert(0, PYWORKER)

import bootstrap  # noqa: E402

DELAY = 0.20  # simulated per-call network latency


class FakeDispatcher:
    """Mimics Dispatcher.call_tool but resolves futures after DELAY seconds.

    Each call returns a real bootstrap.Pending wrapping a loop future; the future
    is completed by a delayed callback on the loop, so concurrency is genuine.
    """

    def __init__(self, loop):
        self._loop = loop

    def call_tool(self, server, tool, args):
        fut = self._loop.create_future()

        async def _resolve():
            await asyncio.sleep(DELAY)
            if tool == "boom":
                fut.set_result({"error": "simulated failure"})
            else:
                # Echo args back as the structured result.
                fut.set_result(
                    {"result": {"structuredContent": {"server": server, "tool": tool, "args": args}}}
                )

        asyncio.run_coroutine_threadsafe(_resolve(), self._loop)
        return bootstrap.Pending(fut, self._loop, server, tool)


def make_sdk_module(dispatcher):
    """Build a fake sdk module exposing t1, t2, t3 and a failing boom()."""
    mod = types.ModuleType("sdk")

    def _make(tool):
        def fn(**kwargs):
            return mod._codemcp_dispatch("srv", tool, kwargs)

        fn.__name__ = tool
        return fn

    mod.t1 = _make("t1")
    mod.t2 = _make("t2")
    mod.t3 = _make("t3")
    mod.boom = _make("boom")
    mod._codemcp_dispatch = dispatcher.call_tool
    return mod


def run_loop_in_thread():
    loop = asyncio.new_event_loop()
    ready = threading.Event()

    def _run():
        asyncio.set_event_loop(loop)
        loop.call_soon(ready.set)
        loop.run_forever()

    t = threading.Thread(target=_run, daemon=True)
    t.start()
    ready.wait()
    return loop, t


def exec_code(code, sdk_module, dispatcher):
    return bootstrap._exec_user_code(code, sdk_module, dispatcher)


FAILS = []


def check(name, cond, detail=""):
    status = "ok" if cond else "FAIL"
    print(f"  [{status}] {name}{(' — ' + detail) if detail else ''}")
    if not cond:
        FAILS.append(name)


def main():
    loop, _thread = run_loop_in_thread()
    dispatcher = FakeDispatcher(loop)
    sdk_module = make_sdk_module(dispatcher)

    print("1. sequential value is transparent")
    result, out, err, error = exec_code(
        "result = t1(x=1)['args']['x']", sdk_module, dispatcher
    )
    check("no error", error is None, error or "")
    check("indexed value resolves to 1", result == 1, repr(result))

    print("2. issue-then-read overlaps round-trips")
    code = """
import time
start = time.time()
a = t1(x=1)        # fired
b = t2(y=2)        # fired
c = t3(z=3)        # fired
# now read — all three already in flight
va, vb, vc = a["tool"], b["tool"], c["tool"]
elapsed = time.time() - start
result = {"vals": [va, vb, vc], "elapsed": elapsed}
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check("results in order", result and result["vals"] == ["t1", "t2", "t3"], repr(result))
    check(
        "elapsed ≈ one DELAY (concurrent, not 3x)",
        result and result["elapsed"] < DELAY * 2,
        f"elapsed={result['elapsed']:.3f}s vs sum={3 * DELAY:.3f}s",
    )

    print("3. read-before-next-call serializes (control)")
    code = """
import time
start = time.time()
a = t1(x=1)["tool"]   # blocks here
b = t2(y=2)["tool"]   # only fired after a resolved
elapsed = time.time() - start
result = elapsed
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check(
        "elapsed ≈ 2x DELAY (serialized as expected)",
        result and result >= DELAY * 1.8,
        f"elapsed={result:.3f}s",
    )

    print("4. tool errors surface as ToolError")
    code = """
try:
    boom()["x"]
    result = "no-error"
except ToolError as e:
    result = "caught: " + str(e)
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no uncaught error", error is None, error or "")
    check("ToolError caught", isinstance(result, str) and result.startswith("caught:"), repr(result))

    print("5. top-level Pending result is resolved before return")
    result, out, err, error = exec_code("result = t1(x=42)", sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check(
        "result resolved to dict, not Pending",
        isinstance(result, dict) and result.get("args", {}).get("x") == 42,
        repr(result),
    )

    print("6. comprehension issue-then-read runs concurrently")
    code = """
import time
start = time.time()
pages = [t1(x=i) for i in range(5)]   # all five fired
xs = [p["args"]["x"] for p in pages]  # then read
elapsed = time.time() - start
result = {"xs": xs, "elapsed": elapsed}
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check("five results in order", result and result["xs"] == [0, 1, 2, 3, 4], repr(result))
    check(
        "elapsed ≈ one DELAY (concurrent, not 5x)",
        result and result["elapsed"] < DELAY * 2,
        f"elapsed={result['elapsed']:.3f}s vs sum={5 * DELAY:.3f}s",
    )

    loop.call_soon_threadsafe(loop.stop)

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)} check(s): {', '.join(FAILS)}")
        sys.exit(1)
    print("All checks passed.")


if __name__ == "__main__":
    main()

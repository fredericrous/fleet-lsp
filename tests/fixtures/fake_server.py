#!/usr/bin/env python3
"""A fake language server for fleet-lsp's integration tests.

Installed as a fixture repository's `.venv/bin/pyright-langserver`, so the
real python resolver reaches it and fleet-lsp's binary carries no test mode.
It behaves like pyright 1.1.411 where fleet-lsp cares: the readiness line is
`Found <n> source files`. Behaviour comes from FAKE_* variables:

FAKE_EVENTS       file to append "<monotonic> <kind> <detail>" lines to
FAKE_READY_DELAY  seconds after `initialized` before the readiness line
FAKE_NEVER_READY  never send the readiness line
FAKE_CONFIG_FIRST send workspace/configuration and wait for its reply
                  before becoming ready (the reply must not be held)
FAKE_STOP_READING stop reading stdin after `initialized`
FAKE_SLOW_READ    read stdin at this many bytes per second
FAKE_FLOOD_MIB    after `initialized`, send this many MiB of diagnostics
FAKE_FLOOD_FRAME  size of each diagnostics frame in bytes (default 1 MiB)
FAKE_EXIT_AFTER   exit on its own this many seconds after `initialized`
FAKE_FLAVOR=ra    behave like rust-analyzer instead: readiness is
                  `experimental/serverStatus` quiescent, with
FAKE_RA_HEALTH    ok | warning | error (default ok) and
FAKE_RA_MESSAGE   the status message
"""
import json, os, sys, time, threading

EVENTS = os.environ.get("FAKE_EVENTS")
out_lock = threading.Lock()


def event(kind, detail=""):
    if EVENTS:
        with open(EVENTS, "a") as f:
            f.write(f"{time.monotonic():.3f} {kind} {detail}\n")


def send(obj):
    body = json.dumps(obj, separators=(",", ":")).encode()
    with out_lock:
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        sys.stdout.buffer.flush()


class Reader:
    def __init__(self, rate=None):
        self.buf = b""
        self.rate = rate

    def fill(self):
        want = 65536 if not self.rate else max(1, int(self.rate) // 20)
        chunk = os.read(0, want)
        if not chunk:
            return False
        self.buf += chunk
        if self.rate:
            time.sleep(len(chunk) / float(self.rate))
        return True

    def message(self):
        while b"\r\n\r\n" not in self.buf:
            if not self.fill():
                return None
        head, rest = self.buf.split(b"\r\n\r\n", 1)
        n = int([l for l in head.split(b"\r\n") if l.lower().startswith(b"content-length")][0].split(b":")[1])
        self.buf = rest
        while len(self.buf) < n:
            if not self.fill():
                return None
        body, self.buf = self.buf[:n], self.buf[n:]
        return json.loads(body)


def flood():
    total = int(float(os.environ["FAKE_FLOOD_MIB"]) * (1 << 20))
    size = int(os.environ.get("FAKE_FLOOD_FRAME", 1 << 20))
    sent = 0
    i = 0
    while sent < total:
        text = "d" * max(0, size - 120)
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": f"file:///f{i}", "diagnostics": [], "pad": text}})
        sent += size
        i += 1
    event("flooded", str(i))


def become_ready(delay):
    if os.environ.get("FAKE_FLAVOR") == "ra":
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": False}})
    time.sleep(delay)
    event("ready")
    if os.environ.get("FAKE_FLAVOR") == "ra":
        params = {"health": os.environ.get("FAKE_RA_HEALTH", "ok"), "quiescent": True}
        if os.environ.get("FAKE_RA_MESSAGE"):
            params["message"] = os.environ["FAKE_RA_MESSAGE"]
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": params})
    else:
        send({"jsonrpc": "2.0", "method": "window/logMessage",
              "params": {"type": 3, "message": "Found 3 source files"}})


def main():
    event("pid", str(os.getpid()))
    rate = os.environ.get("FAKE_SLOW_READ")
    r = Reader(float(rate) if rate else None)
    shutdown = False
    config_id = 900
    while True:
        m = r.message()
        if m is None:
            event("eof")
            sys.exit(0 if shutdown else 1)
        method = m.get("method")
        mid = m.get("id")
        if method:
            event("recv", f"{method} {json.dumps(mid)}")
        else:
            event("reply", json.dumps(mid))
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"referencesProvider": True}}})
        elif method == "initialized":
            if os.environ.get("FAKE_EXIT_AFTER"):
                t = float(os.environ["FAKE_EXIT_AFTER"])
                threading.Thread(target=lambda: (time.sleep(t), event("self-exit"), os._exit(3)), daemon=True).start()
            if os.environ.get("FAKE_FLOOD_MIB"):
                threading.Thread(target=flood, daemon=True).start()
            if os.environ.get("FAKE_CONFIG_FIRST"):
                send({"jsonrpc": "2.0", "id": config_id, "method": "workspace/configuration",
                      "params": {"items": [{"section": "python.analysis"}]}})
            elif not os.environ.get("FAKE_NEVER_READY"):
                threading.Thread(target=become_ready,
                                 args=(float(os.environ.get("FAKE_READY_DELAY", "0")),), daemon=True).start()
            if os.environ.get("FAKE_STOP_READING"):
                event("stopped-reading")
                while True:
                    time.sleep(3600)
        elif mid == config_id and method is None:
            event("config-reply", json.dumps(m.get("result")))
            if not os.environ.get("FAKE_NEVER_READY"):
                threading.Thread(target=become_ready,
                                 args=(float(os.environ.get("FAKE_READY_DELAY", "0")),), daemon=True).start()
        elif method == "shutdown":
            shutdown = True
            send({"jsonrpc": "2.0", "id": mid, "result": None})
        elif method == "exit":
            event("exit")
            sys.exit(0 if shutdown else 1)
        elif mid is not None and method:
            send({"jsonrpc": "2.0", "id": mid, "result": [{"uri": "file:///answer", "range": {
                "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]})


if "--version" in sys.argv:
    print("rust-analyzer 1.94.1 (fake)" if os.environ.get("FAKE_FLAVOR") == "ra" else "fake 0")
    sys.exit(0)
main()

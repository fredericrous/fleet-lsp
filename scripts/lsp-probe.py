#!/usr/bin/env python3
"""Phase 0 probe: drive one language server cold and observe readiness.

Records, with timestamps relative to process start:
- every server notification/request (progress, serverStatus, log/showMessage);
- a "first" query sent right after didOpen, and when/what it answered;
- polled re-sends of the same query until it returns the known answer.

Writes a JSONL transcript and prints a JSON summary.
"""
import argparse, json, os, queue, subprocess, sys, threading, time
from pathlib import Path

def frame(obj):
    body = json.dumps(obj).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body

def reader(stream, q):
    buf = b""
    while True:
        while b"\r\n\r\n" not in buf:
            chunk = stream.read1(65536) if hasattr(stream, "read1") else stream.read(65536)
            if not chunk:
                q.put(None); return
            buf += chunk
        head, buf = buf.split(b"\r\n\r\n", 1)
        n = 0
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"):
                n = int(line.split(b":")[1])
        while len(buf) < n:
            chunk = stream.read1(65536) if hasattr(stream, "read1") else stream.read(65536)
            if not chunk:
                q.put(None); return
            buf += chunk
        body, buf = buf[:n], buf[n:]
        q.put(json.loads(body))

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cmd", required=True, help="JSON list")
    ap.add_argument("--cwd", required=True)
    ap.add_argument("--root", required=True)
    ap.add_argument("--file", required=True)
    ap.add_argument("--lang", required=True)
    ap.add_argument("--line", type=int, required=True, help="0-based")
    ap.add_argument("--char", type=int, required=True, help="0-based")
    ap.add_argument("--expect", required=True, help="substring a correct answer's uris contain")
    ap.add_argument("--open-delay", type=float, default=0.0)
    ap.add_argument("--config-delay", type=float, default=0.0)
    ap.add_argument("--init-options", default="null")
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--unset-env", action="append", default=[])
    ap.add_argument("--poll", type=float, default=1.0)
    ap.add_argument("--timeout", type=float, default=300.0)
    ap.add_argument("--eof-test", action="store_true")
    ap.add_argument("--until-quiescent", action="store_true")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    env = dict(os.environ)
    for k in a.unset_env:
        env.pop(k, None)
    for kv in a.env:
        k, v = kv.split("=", 1); env[k] = v
    t0 = time.monotonic()
    rel = lambda: round(time.monotonic() - t0, 3)
    p = subprocess.Popen(json.loads(a.cmd), cwd=a.cwd, env=env, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=open(a.out + ".stderr", "wb"))
    q = queue.Queue()
    threading.Thread(target=reader, args=(p.stdout, q), daemon=True).start()
    out = open(a.out, "w")
    wlock = threading.Lock()
    def log(kind, obj):
        out.write(json.dumps({"t": rel(), "kind": kind, "msg": obj}) + "\n"); out.flush()
    def send(obj):
        with wlock:
            p.stdin.write(frame(obj)); p.stdin.flush()
        log("send", obj)

    root_uri = Path(a.root).resolve().as_uri()
    file_uri = Path(a.file).resolve().as_uri()
    caps = {
        "workspace": {"configuration": True, "workspaceFolders": True,
                      "didChangeConfiguration": {"dynamicRegistration": True}},
        "window": {"workDoneProgress": True, "showMessage": {}},
        "textDocument": {"references": {}, "callHierarchy": {}, "definition": {},
                         "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                         "synchronization": {"didSave": True}},
        "experimental": {"serverStatusNotification": True},
    }
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "processId": os.getpid(), "rootUri": root_uri, "rootPath": a.root,
        "workspaceFolders": [{"uri": root_uri, "name": Path(a.root).name}],
        "capabilities": caps, "initializationOptions": json.loads(a.init_options)}})

    next_id = [100]
    pending = {}      # id -> (label, t_sent)
    summary = {"lang": a.lang, "cmd": a.cmd, "file": a.file, "expect": a.expect,
               "progress": [], "status": [], "logmsgs": [], "showmsgs": [],
               "first": None, "polls": [], "ready_at": None, "initialized_at": None,
               "opened_at": None, "exit_after_eof_s": None, "server_requests": {}}
    state = {"initialized": False, "opened": False, "first_sent": False, "ready": False,
             "last_poll": -1e9, "poll_inflight": False}

    def query(label):
        i = next_id[0]; next_id[0] += 1
        pending[i] = (label, rel())
        send({"jsonrpc": "2.0", "id": i, "method": "textDocument/references", "params": {
            "textDocument": {"uri": file_uri}, "position": {"line": a.line, "character": a.char},
            "context": {"includeDeclaration": False}}})

    def correct(result):
        if not isinstance(result, list):
            return False
        return any(a.expect in (loc.get("uri", "")) for loc in result)

    def reply(mid, result):
        send({"jsonrpc": "2.0", "id": mid, "result": result})

    deadline = t0 + a.timeout
    while time.monotonic() < deadline:
        now = rel()
        if state["initialized"] and not state["opened"] and now >= summary["initialized_at"] + a.open_delay:
            send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": {
                "uri": file_uri, "languageId": a.lang, "version": 1,
                "text": Path(a.file).read_text()}}})
            state["opened"] = True; summary["opened_at"] = now
            query("first"); state["first_sent"] = True
        if state["opened"] and not state["ready"] and not state["poll_inflight"] and now - state["last_poll"] >= a.poll:
            query("poll"); state["poll_inflight"] = True; state["last_poll"] = now
        try:
            m = q.get(timeout=0.05)
        except queue.Empty:
            continue
        if m is None:
            summary["died_at"] = rel(); break
        log("recv", m)
        if "method" in m and "id" in m:  # server request
            meth = m["method"]
            summary["server_requests"][meth] = summary["server_requests"].get(meth, 0) + 1
            if meth == "workspace/configuration":
                n = len(m["params"].get("items", []))
                if a.config_delay:
                    mid = m["id"]
                    threading.Timer(a.config_delay, reply, args=(mid, [None] * n)).start()
                else:
                    reply(m["id"], [None] * n)
            else:
                reply(m["id"], None)
        elif "method" in m:
            meth, prm = m["method"], m.get("params", {})
            if meth == "$/progress":
                v = prm.get("value", {})
                summary["progress"].append([rel(), str(prm.get("token")), v.get("kind"),
                                            v.get("title"), v.get("message")])
            elif meth == "experimental/serverStatus":
                summary["status"].append([rel(), prm.get("health"), prm.get("quiescent"), prm.get("message")])
            elif meth == "window/logMessage" and len(summary["logmsgs"]) < 40:
                summary["logmsgs"].append([rel(), prm.get("message", "")[:300]])
            elif meth == "window/showMessage":
                summary["showmsgs"].append([rel(), prm.get("message", "")[:300]])
        elif "id" in m:
            if m["id"] == 1:
                send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
                state["initialized"] = True; summary["initialized_at"] = rel()
                continue
            label, ts = pending.pop(m["id"], (None, None))
            res = m.get("result"); err = m.get("error")
            shape = ("error:" + str(err.get("message"))[:120]) if err else \
                    ("null" if res is None else f"list[{len(res)}]" if isinstance(res, list) else type(res).__name__)
            ok = correct(res)
            rec = [ts, rel(), shape, ok]
            if label == "first":
                summary["first"] = rec
            elif label == "poll":
                summary["polls"].append(rec); state["poll_inflight"] = False
            if ok and not state["ready"]:
                state["ready"] = True; summary["ready_at"] = rel()
            if state["ready"] and summary["first"] is not None and (not a.until_quiescent or any(x[2] for x in summary["status"])):
                break
        if a.until_quiescent and state["ready"] and any(x[2] for x in summary["status"]):
            break
    if a.eof_test and p.poll() is None:
        te = time.monotonic()
        p.stdin.close()
        try:
            p.wait(timeout=10); summary["exit_after_eof_s"] = round(time.monotonic() - te, 3)
        except subprocess.TimeoutExpired:
            summary["exit_after_eof_s"] = "still alive after 10s"
    if p.poll() is None:
        p.kill(); p.wait()
    # compress polls: keep first 5, last 3
    if len(summary["polls"]) > 8:
        summary["polls"] = summary["polls"][:5] + [["…", len(summary["polls"]) - 8]] + summary["polls"][-3:]
    print(json.dumps(summary, indent=1))

main()

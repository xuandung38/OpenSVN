#!/usr/bin/env python3
"""Message-level logging HTTP proxy: records every request/response exchanged
between `svn` and the capture container as JSON lines (decoded bodies, gzip off).

usage: proxy.py LISTEN_PORT UPSTREAM_PORT OUT.jsonl
"""
import base64, http.client, json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LISTEN, UPSTREAM, OUT = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
LOCK = threading.Lock()
SEQ = [0]

def read_body(rfile, headers):
    if headers.get("Transfer-Encoding", "").lower() == "chunked":
        out = b""
        while True:
            size = int(rfile.readline().split(b";")[0].strip() or b"0", 16)
            if size == 0:
                while rfile.readline().strip():
                    pass
                return out
            out += rfile.read(size)
            rfile.readline()
    n = int(headers.get("Content-Length", 0) or 0)
    return rfile.read(n) if n else b""

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def handle_any(self):
        body = read_body(self.rfile, self.headers)
        hdrs = {k: v for k, v in self.headers.items()
                if k.lower() not in ("accept-encoding", "transfer-encoding", "connection", "content-length", "proxy-connection")}
        hdrs["Accept-Encoding"] = "identity"
        c = http.client.HTTPConnection("127.0.0.1", UPSTREAM, timeout=60)
        c.request(self.command, self.path, body=body if body or self.command in ("POST","PUT","PROPFIND","REPORT","LOCK","MERGE","PROPPATCH") else None, headers=hdrs)
        r = c.getresponse()
        rbody = r.read()
        rh = [(k, v) for k, v in r.getheaders()]
        with LOCK:
            SEQ[0] += 1
            rec = {"seq": SEQ[0], "method": self.command, "path": self.path,
                   "req_headers": list(self.headers.items()),
                   "req_body": base64.b64encode(body).decode(),
                   "status": r.status, "reason": r.reason, "resp_headers": rh,
                   "resp_body": base64.b64encode(rbody).decode()}
            with open(OUT, "a") as f:
                f.write(json.dumps(rec) + "\n")
        self.send_response(r.status, r.reason)
        for k, v in rh:
            if k.lower() in ("transfer-encoding", "connection", "content-length", "keep-alive", "content-encoding"):
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(rbody)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(rbody)
    do_GET = do_POST = do_PUT = do_DELETE = do_OPTIONS = do_PROPFIND = do_REPORT = handle_any
    do_MKCOL = do_COPY = do_MERGE = do_LOCK = do_UNLOCK = do_PROPPATCH = do_HEAD = do_CHECKOUT = handle_any

ThreadingHTTPServer(("127.0.0.1", LISTEN), H).serve_forever()

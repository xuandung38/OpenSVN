#!/usr/bin/env python3
"""Turns raw proxy captures (*.jsonl) into anonymized golden fixtures.

usage: extract.py RAW_DIR DEST_DIR   (DEST_DIR = crates/svn-protocol/tests/data)

Each exchange becomes three files: <name>.meta (request line, status, headers),
<name>.req and <name>.resp (raw bodies, omitted when empty). Selection is the
MANIFEST below: (step, method, status, nth-match, destination).
"""
import base64, json, os, re, shutil, sys

RAW, DEST = sys.argv[1], sys.argv[2]
DROP = {"authorization", "date", "keep-alive", "content-length", "transfer-encoding",
        "connection", "accept-encoding", "vary", "ms-author-via", "etag-unused"}
UUID = re.compile(rb"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
FAKE_REPO_UUID = b"4c9a806c-49e3-4175-9b8a-de5e8a74be25"  # repository uuid is kept stable below

def norm_bytes(b: bytes) -> bytes:
    b = b.replace(b"127.0.0.1:18081", b"svn.test.local").replace(b"127.0.0.1", b"svn.test.local")
    return b

def norm_text(s: str) -> str:
    return s.replace("127.0.0.1:18081", "svn.test.local").replace("127.0.0.1", "svn.test.local")

def load(step):
    with open(os.path.join(RAW, step + ".jsonl")) as f:
        return [json.loads(l) for l in f]

def pick(step, method=None, status=None, path=None, nth=0, header=None, has_body=None):
    out = []
    for r in load(step):
        if method and r["method"] != method: continue
        if status and r["status"] != status: continue
        if path and not re.search(path, r["path"]): continue
        if header and not any(k.lower() == header.lower() for k, _ in r["resp_headers"]): continue
        out.append(r)
    return out[nth]

def write(r, dest):
    d = os.path.join(DEST, os.path.dirname(dest)); os.makedirs(d, exist_ok=True)
    base = os.path.join(DEST, dest)
    lines = [f"REQUEST {r['method']} {norm_text(r['path'])}"]
    for k, v in r["req_headers"]:
        if k.lower() in DROP or k.lower() == "host": continue
        lines.append(f"> {k}: {norm_text(v)}")
    lines.append(f"RESPONSE {r['status']}")
    for k, v in r["resp_headers"]:
        if k.lower() in DROP or k.lower() == "server": continue
        lines.append(f"< {k}: {norm_text(v)}")
    open(base + ".meta", "w").write("\n".join(lines) + "\n")
    for key, ext in (("req_body", ".req"), ("resp_body", ".resp")):
        b = base64.b64decode(r[key])
        if b: open(base + ext, "wb").write(norm_bytes(b))

M = [
 ("options-info", dict(method="OPTIONS", status=401), "caps/options-401"),
 ("options-info", dict(method="OPTIONS", status=200, header="SVN-Me-Resource"), "caps/options-200"),
 ("options-info", dict(method="OPTIONS", status=200, nth=1), "caps/options-200-no-v2-headers"),
 ("stat-dir", dict(method="PROPFIND", status=207), "propfind/stat-dir"),
 ("lock-info", dict(method="PROPFIND", status=207, path="rvr"), "propfind/stat-file"),
 ("stat-missing", dict(method="PROPFIND", status=404), "propfind/stat-missing-404"),
 ("list-propfind-depth1", dict(method="PROPFIND", status=207), "propfind/list-depth1"),
 ("proplist", dict(method="PROPFIND", status=207, nth=1), "propfind/allprop-file"),
 ("lock-info", dict(method="PROPFIND", status=207, path=r"^/svn/repo/trunk"), "propfind/get-lock"),
 ("unlock", dict(method="PROPFIND", status=207), "propfind/get-lock-no-comment"),
 ("cat", dict(method="GET", status=200), "get/file"),
 ("log-verbose", dict(method="REPORT", status=200), "log/head5-verbose"),
 ("log-xml-verbose", dict(method="REPORT", status=200), "log/head-verbose"),
 ("log-xml-root", dict(method="REPORT", status=200), "log/root-with-copy"),
 ("log-limit", dict(method="REPORT", status=200, path="rvr"), "log/limit2-all-revprops"),
 ("log-merged", dict(method="REPORT", status=200, path="rvr"), "log/merged-revisions"),
 ("log-empty-range", dict(method="REPORT", status=200), "log/single-rev-no-paths"),
 ("lock", dict(method="LOCK", status=200), "locks/lock-ok"),
 ("lock-steal", dict(method="LOCK", status=200), "locks/lock-steal"),
 ("lock-wc", dict(method="LOCK", status=200), "locks/lock-from-wc"),
 ("unlock-force-unlocked", dict(method="PROPFIND", status=207), "propfind/get-lock-none"),
 ("lock-conflict", dict(method="LOCK", status=423), "locks/lock-conflict-423"),
 ("unlock", dict(method="UNLOCK", status=204), "locks/unlock-204"),
 ("unlock-break", dict(method="UNLOCK", status=204), "locks/unlock-break"),
 ("list-locks", dict(method="REPORT", status=200, path=r"^/svn/repo/trunk"), "locks/get-locks-report"),
 ("list-trunk", dict(method="REPORT", status=200, path=r"^/svn/repo/trunk"), "locks/get-locks-report-empty"),
 ("commit-out-of-date", dict(method="PUT", status=409), "errors/out-of-date-409"),
 ("stat-missing", dict(method="OPTIONS", status=401), "errors/auth-401-html"),
 ("stat-missing", dict(method="PROPFIND", status=404), "errors/not-found-404-html"),
]
# Raw transcripts kept for later work (update / commit goldens).
M += [("checkout", dict(method="REPORT", status=200, path="!svn/me"), "update/checkout-send-all"),
      ("checkout-skelta", dict(method="REPORT", status=200, path="!svn/me"), "update/checkout-skelta"),
      ("update", dict(method="REPORT", status=200, path="!svn/me"), "update/update-send-all"),
      ("status-u", dict(method="REPORT", status=200, path="!svn/me"), "update/status-u"),
      ("commit", dict(method="POST", status=201), "commit/create-txn"),
      ("commit", dict(method="MKCOL", status=201), "commit/mkcol"),
      ("commit", dict(method="PUT", status=201, nth=0), "commit/put-new"),
      ("commit", dict(method="MERGE", status=200), "commit/merge")]

M += [("log-control-chars", dict(method="REPORT", status=200, path="rvr"), "log/control-chars")]

if os.path.isdir(DEST): shutil.rmtree(DEST)
for step, sel, dest in M:
    try:
        write(pick(step, **sel), dest)
    except IndexError:
        print("MISSING", step, sel, dest)
# Expected `svn ... --xml` outputs, for semantic comparison tests.
for step, dest in (("log-xml-verbose", "log/head-verbose"), ("log-xml-root", "log/root-with-copy"),
                   ("log-limit", "log/limit2-all-revprops"), ("log-merged", "log/merged-revisions"),
                   ("list-xml", "propfind/list-depth1"), ("info-xml", "propfind/stat-file")):
    t = open(os.path.join(RAW, step + ".svn-out.txt"), "rb").read()
    open(os.path.join(DEST, dest + ".svn.xml"), "wb").write(norm_bytes(t))
print("ok")

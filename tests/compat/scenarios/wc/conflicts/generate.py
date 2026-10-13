#!/usr/bin/env python3
"""Records ACTUAL_NODE.conflict_data blobs written by the local svn client.

Usage: python3 generate.py (needs svn, svnadmin; recorded against svn 1.15.0).
Creates an update conflict (text+prop on one file, tree on another) and a merge text conflict in
/tmp/opensvn-golden-conflicts and stores each blob as `<case>.skel` plus its relpath context.
"""
import os, shutil, sqlite3, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = "/tmp/opensvn-golden-conflicts"
ENV = dict(os.environ, LC_ALL="en_US.UTF-8", TZ="UTC")

def svn(*a, cwd=None):
    r = subprocess.run(["svn", *a], cwd=cwd, env=ENV, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(f"svn {a}: {r.stderr.decode()}")
    return r.stdout.decode()

def blobs(wc):
    db = sqlite3.connect(os.path.join(wc, ".svn", "wc.db"))
    return {rel: bytes(b) for rel, b in db.execute("select local_relpath, conflict_data from actual_node where conflict_data is not null")}

def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    subprocess.run(["svnadmin", "create", WORK + "/repo"], check=True)
    url = "file://" + WORK + "/repo"
    a, b = WORK + "/a", WORK + "/b"
    svn("checkout", "-q", url, a)
    for name, body in (("t.txt", "one\ntwo\nthree\n"), ("d.txt", "doomed\n"), ("m.txt", "m1\nm2\nm3\n")):
        open(f"{a}/{name}", "w").write(body)
    svn("add", "-q", f"{a}/t.txt", f"{a}/d.txt", f"{a}/m.txt")
    svn("propset", "-q", "color", "red", f"{a}/t.txt")
    svn("commit", "-q", "--username", "alice", "-m", "base", a)
    svn("checkout", "-q", url, b)
    # wc a commits the incoming change.
    open(f"{a}/t.txt", "w").write("one\nTWO-A\nthree\n")
    svn("propset", "-q", "color", "blue", f"{a}/t.txt")
    svn("delete", "-q", f"{a}/d.txt")
    svn("commit", "-q", "--username", "alice", "-m", "incoming", a)
    # wc b has conflicting local changes.
    open(f"{b}/t.txt", "w").write("one\nTWO-B\nthree\n")
    svn("propset", "-q", "color", "green", f"{b}/t.txt")
    open(f"{b}/d.txt", "w").write("doomed\nedited\n")
    svn("update", "-q", "--accept", "postpone", b)
    out = {}
    for rel, blob in blobs(b).items():
        out["update_" + rel.replace(".", "_")] = (rel, blob)
    # Merge conflict: merge r3 (a change to m.txt) into a locally modified copy.
    open(f"{a}/m.txt", "w").write("m1\nMERGE-A\nm3\n")
    svn("commit", "-q", "--username", "alice", "-m", "m-change", a)
    open(f"{b}/m.txt", "w").write("m1\nMERGE-B\nm3\n")
    svn("merge", "-q", "--accept", "postpone", "-c", "3", url + "/m.txt", f"{b}/m.txt")
    for rel, blob in blobs(b).items():
        if rel == "m.txt":
            out["merge_m_txt"] = (rel, blob)
    for case, (rel, blob) in sorted(out.items()):
        open(os.path.join(HERE, case + ".skel"), "wb").write(blob)
        print(case, rel, blob[:100])
    shutil.rmtree(WORK, ignore_errors=True)

if __name__ == "__main__":
    sys.exit(main())

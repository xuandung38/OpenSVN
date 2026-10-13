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

def extra_cases(url, a, b, out):
    """Props absent on one side, several property names, a local move, and a switch."""
    # Property added on both sides (no common ancestor value) plus two conflicting names.
    open(f"{a}/p.txt", "w").write("p\n")
    open(f"{a}/mv.txt", "w").write("mv\n")
    svn("add", "-q", f"{a}/p.txt", f"{a}/mv.txt")
    svn("propset", "-q", "one", "1", f"{a}/p.txt")
    svn("propset", "-q", "two", "2", f"{a}/p.txt")
    svn("commit", "-q", "--username", "alice", "-m", "p", a)
    svn("update", "-q", b)
    svn("propset", "-q", "one", "mine1", f"{b}/p.txt")
    svn("propset", "-q", "two", "mine2", f"{b}/p.txt")
    svn("propset", "-q", "added", "mine", f"{b}/p.txt")
    svn("propset", "-q", "one", "theirs1", f"{a}/p.txt")
    svn("propset", "-q", "two", "theirs2", f"{a}/p.txt")
    svn("propset", "-q", "added", "theirs", f"{a}/p.txt")
    svn("commit", "-q", "--username", "alice", "-m", "p2", a)
    # Local move in b, incoming edit in a: tree conflict on the move source.
    svn("move", "-q", f"{b}/mv.txt", f"{b}/moved.txt")
    open(f"{a}/mv.txt", "w").write("mv edited\n")
    svn("commit", "-q", "--username", "alice", "-m", "edit mv", a)
    svn("update", "-q", "--accept", "postpone", b)
    for rel, blob in blobs(b).items():
        if rel in ("p.txt", "mv.txt"):
            out["update2_" + rel.replace(".", "_")] = (rel, blob)
    # Switch with a text conflict: the branch copy differs from the locally edited trunk file.
    open(f"{a}/s.txt", "w").write("s1\ns2\ns3\n")
    svn("add", "-q", f"{a}/s.txt")
    svn("commit", "-q", "--username", "alice", "-m", "s", a)
    svn("mkdir", "-q", "--username", "alice", "-m", "branches", url + "/br")
    svn("copy", "-q", "--username", "alice", "-m", "branch", url + "/s.txt", url + "/br/s.txt")
    svn("update", "-q", a)
    open(f"{a}/br/s.txt", "w").write("s1\nBRANCH\ns3\n")
    svn("commit", "-q", "--username", "alice", "-m", "branch change", a)
    svn("update", "-q", b)
    open(f"{b}/s.txt", "w").write("s1\nLOCAL\ns3\n")
    svn("switch", "-q", "--accept", "postpone", url + "/br/s.txt", f"{b}/s.txt")
    for rel, blob in blobs(b).items():
        if rel == "s.txt":
            out["switch_s_txt"] = (rel, blob)

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
    extra_cases(url, a, b, out)
    for case, (rel, blob) in sorted(out.items()):
        open(os.path.join(HERE, case + ".skel"), "wb").write(blob)
        print(case, rel, blob[:100])
    shutil.rmtree(WORK, ignore_errors=True)

if __name__ == "__main__":
    sys.exit(main())

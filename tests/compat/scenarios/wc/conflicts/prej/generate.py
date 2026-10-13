#!/usr/bin/env python3
"""Records property reject (`.prej`) files and the matching `conflict_data` written by svn 1.15.0.

Usage: python3 generate.py (needs svn, svnadmin). One property per file so the output does not
depend on APR hash order. Per case it stores:
  <case>.skel     ACTUAL_NODE.conflict_data
  <case>.prej     the reject file svn wrote
  <case>.pristine NODES.properties of the node (proplist skel), used for merge conflicts
"""
import os, shutil, sqlite3, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = "/tmp/opensvn-golden-prej"
ENV = dict(os.environ, LC_ALL="en_US.UTF-8", TZ="UTC")


def svn(*a, check=True):
    r = subprocess.run(["svn", *a], env=ENV, capture_output=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"svn {a}: {r.stderr.decode()}")
    return r


def setprop(path, name, value):
    """Sets (bytes) or deletes (None) a property."""
    if value is None:
        svn("propdel", "-q", name, path)
    else:
        with tempfile.NamedTemporaryFile(delete=False) as f:
            f.write(value)
        svn("propset", "-q", "-F", f.name, name, path)
        os.unlink(f.name)


CASES = {
    # name: (base, incoming, local); None means "property absent"
    "multiline": (b"line1\nline2\nline3\n", b"line1\nTHEIRS\nline3\n", b"line1\nMINE\nline3\n"),
    "both_add": (None, b"theirs", b"mine"),
    "delete_vs_modify": (b"v", None, b"w"),
    "modify_vs_delete": (b"v", b"w", None),
    "binary": (b"\x00\x01", b"\x00\x03", b"\x00\x02"),
    "non_ascii": ("café\n".encode(), "cafü theirs\n".encode(), "café mine\n".encode()),
    "single_line": (b"base", b"theirs", b"mine"),
    # Values that differ only in characters that render to the same ASCII still conflict.
    "fuzzy_equal_sides": (b"x\n", b"cafe\n", "café\n".encode()),
    # Controls are kept (NEL becomes {U+0085}); U+2019 and U+2013 and NBSP are lumped to ASCII;
    # double quotes are not.
    "controls_and_lump": (
        b"x\n",
        "t\u2019s \u2013 x\u00a0y \u201cq\u201d\n".encode(),
        b"m\tb\x0cc\x0bd\xc2\x85e\n",
    ),
}


def blob(wc, rel, sql):
    db = sqlite3.connect(os.path.join(wc, ".svn", "wc.db"))
    row = db.execute(sql, (rel,)).fetchone()
    return bytes(row[0]) if row and row[0] is not None else None


def record(name, wc, rel):
    skel = blob(wc, rel, "select conflict_data from actual_node where local_relpath=?")
    prej = None
    for entry in os.listdir(wc):
        if entry.startswith(rel) and entry.endswith(".prej"):
            prej = open(os.path.join(wc, entry), "rb").read()
    pristine = blob(wc, rel, "select properties from nodes where local_relpath=? and op_depth=0")
    open(os.path.join(HERE, name + ".skel"), "wb").write(skel)
    open(os.path.join(HERE, name + ".prej"), "wb").write(prej)
    if pristine is not None:
        open(os.path.join(HERE, name + ".pristine"), "wb").write(pristine)


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    subprocess.run(["svnadmin", "create", WORK + "/repo"], check=True)
    url = "file://" + WORK + "/repo"
    a, b = WORK + "/a", WORK + "/b"
    svn("checkout", "-q", url, a)
    for case in CASES:
        open(f"{a}/{case}", "w").write("x\n")
        svn("add", "-q", f"{a}/{case}")
    open(f"{a}/merged", "w").write("m\n")
    svn("add", "-q", f"{a}/merged")
    svn("commit", "-q", "--username", "alice", "-m", "files", a)
    for case, (base, _, _) in CASES.items():
        if base is not None:
            setprop(f"{a}/{case}", "k", base)
    setprop(f"{a}/merged", "k", b"base")
    svn("commit", "-q", "--username", "alice", "-m", "base props", a)
    svn("checkout", "-q", url, b)
    for case, (_, incoming, local) in CASES.items():
        setprop(f"{a}/{case}", "k", incoming)
        setprop(f"{b}/{case}", "k", local)
    svn("commit", "-q", "--username", "alice", "-m", "incoming", a)
    svn("update", "-q", "--accept", "postpone", b)
    for case in CASES:
        record("update_" + case, b, case)
    # Merge conflict: merge the change of revision 4 into a locally changed property.
    setprop(f"{a}/merged", "k", b"theirs-merge")
    svn("commit", "-q", "--username", "alice", "-m", "merge change", a)
    setprop(f"{b}/merged", "k", b"local-merge")
    svn("merge", "-q", "--accept", "postpone", "-c", "4", url + "/merged", f"{b}/merged")
    record("merge_single_line", b, "merged")
    shutil.rmtree(WORK, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())

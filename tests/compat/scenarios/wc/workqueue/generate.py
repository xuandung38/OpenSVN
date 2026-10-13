#!/usr/bin/env python3
"""Records WORK_QUEUE items that svn 1.15.0 leaves behind when an operation is interrupted.

Usage: python3 generate.py (POSIX only; needs svn, svnadmin). The interruption is forced by
making the working copy directory read-only so the filesystem step of the operation fails after
the queue was written. Output: queue.txt, one skel per line (bytes as latin-1 escaped text).
"""
import os, shutil, sqlite3, stat, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = "/tmp/opensvn-golden-wq"
ENV = dict(os.environ, LC_ALL="en_US.UTF-8", TZ="UTC")

def svn(*a, cwd=None, check=True):
    r = subprocess.run(["svn", *a], cwd=cwd, env=ENV, capture_output=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"svn {a}: {r.stderr.decode()}")
    return r

def queue(wc):
    db = sqlite3.connect(os.path.join(wc, ".svn", "wc.db"))
    return [bytes(w) for (w,) in db.execute("select work from work_queue order by id")]

def chmod_dir(path, writable):
    os.chmod(path, 0o755 if writable else 0o555)

def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    subprocess.run(["svnadmin", "create", WORK + "/repo"], check=True)
    url = "file://" + WORK + "/repo"
    a, b = WORK + "/a", WORK + "/b"
    svn("checkout", "-q", url, a)
    os.makedirs(a + "/d")
    for f, body in (("b.txt", "base\n"), ("a.txt", "a\n"), ("c.txt", "c\n"), ("d/f", "f\n")):
        open(f"{a}/{f}", "w").write(body)
    svn("add", "-q", a + "/b.txt", a + "/a.txt", a + "/c.txt", a + "/d")
    svn("commit", "-q", "--username", "alice", "-m", "base", a)
    cases = {}

    # Interrupted revert: file modified, directory read-only so the install cannot replace it.
    svn("checkout", "-q", url, b)
    open(b + "/b.txt", "w").write("modified\n")
    chmod_dir(b, False)
    svn("revert", b + "/b.txt", check=False)
    chmod_dir(b, True)
    cases["interrupted_revert"] = queue(b)
    svn("cleanup", b, check=False)

    # Interrupted update that deletes a directory and a file.
    svn("delete", "-q", a + "/d", a + "/c.txt")
    svn("commit", "-q", "--username", "alice", "-m", "delete", a)
    chmod_dir(b, False)
    svn("update", "-q", b, check=False)
    chmod_dir(b, True)
    cases["interrupted_update_delete"] = queue(b)

    with open(os.path.join(HERE, "queue.txt"), "w") as out:
        for name, items in cases.items():
            for item in items:
                out.write(f"{name}\t{item.decode('latin-1')}\n")
            print(name, [i.decode('latin-1') for i in items])
    shutil.rmtree(WORK, ignore_errors=True)

if __name__ == "__main__":
    sys.exit(main())

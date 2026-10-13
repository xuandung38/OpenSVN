#!/usr/bin/env python3
"""Regenerates the golden translation fixtures with the locally installed svn client.

Usage: python3 generate.py   (requires svn, svnadmin, sqlite3; recorded against svn 1.15.0)

For every case a file is committed with the given properties, checked out again, and the
repo-normal bytes (pristine), the working bytes and the keyword inputs are stored in
`<case>/`. Cases that svn rejects store `expect_error.txt` instead.
"""
import os, shutil, subprocess, sys, tempfile, sqlite3, xml.etree.ElementTree as ET
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))

KW_ALL = ("$Id$ $Rev$ $Revision$ $LastChangedRevision$ $Date$ $LastChangedDate$ $Author$ "
          "$LastChangedBy$ $HeadURL$ $URL$ $Header$\n"
          "stale: $Id: old 1 2000-01-01 00:00:00Z bob $ $Rev: 99 $ $Date: junk $ $Author:$\n"
          "fixed: $Rev::            $ $Rev:: 12345678901234567890 $ $Author:: x $ $Date::   $ $Id::  $\n"
          "unknown: $Foo$ $foo: bar$ $id$ $REV$ $Rev $ $$ $$$ $Id\n"
          "nested: $Id: $Rev$ x$ and $Rev: $Date$\n"
          "tail $Rev$")
LONG = "$Id: " + "x" * 300 + " $\n$Rev: " + "y" * 249 + "$ $Rev: " + "z" * 250 + "$\n"

# Content written after the properties were set (before commit), to get mixed line endings
# past `svn propset`'s consistency check.
LATER = {
    "eol_lf_mixed_after_prop": b"one\ntwo\r\nthree\rfour\n",
    "eol_crlf_mixed_after_prop": b"one\ntwo\r\nthree\rfour\n",
    "eol_cr_mixed_after_prop": b"one\ntwo\r\nthree\rfour\n",
    "eol_native_mixed_after_prop": b"one\ntwo\r\nthree\n",
}

# Cases whose repository path differs from the case name (percent-encoding in URLs).
PATHS = {"kw_encoded_url": "sp ace/\u00e9/a b%41.txt"}

# (name, content, props, timezones)
CASES = [
    ("plain_no_props", b"a\nb\r\nc\n", {}, ["UTC"]),
    ("eol_native_lf", b"one\ntwo\nthree\n", {"svn:eol-style": "native"}, ["UTC"]),
    ("eol_native_crlf_source", b"one\r\ntwo\r\nthree\r\n", {"svn:eol-style": "native"}, ["UTC"]),
    ("eol_native_no_trailing", b"one\ntwo", {"svn:eol-style": "native"}, ["UTC"]),
    ("eol_native_mixed", b"one\ntwo\r\nthree\n", {"svn:eol-style": "native"}, ["UTC"]),
    ("eol_lf_lf_source", b"one\ntwo\n", {"svn:eol-style": "LF"}, ["UTC"]),
    ("eol_lf_crlf_source", b"one\r\ntwo\r\n", {"svn:eol-style": "LF"}, ["UTC"]),
    ("eol_lf_mixed", b"one\ntwo\r\nthree\rfour\n", {"svn:eol-style": "LF"}, ["UTC"]),
    ("eol_crlf_lf_source", b"one\ntwo\n", {"svn:eol-style": "CRLF"}, ["UTC"]),
    ("eol_crlf_crlf_source", b"one\r\ntwo\r\n", {"svn:eol-style": "CRLF"}, ["UTC"]),
    ("eol_crlf_mixed", b"one\ntwo\r\nthree\rfour\n", {"svn:eol-style": "CRLF"}, ["UTC"]),
    ("eol_cr_lf_source", b"one\ntwo\n", {"svn:eol-style": "CR"}, ["UTC"]),
    ("eol_cr_cr_source", b"one\rtwo\r", {"svn:eol-style": "CR"}, ["UTC"]),
    ("eol_crlf_lone_cr", b"a\rb\n\r\n\n\rc", {"svn:eol-style": "CRLF"}, ["UTC"]),
    ("eol_unknown_value", b"one\r\ntwo\n", {"svn:eol-style": "bogus"}, ["UTC"]),
    ("eol_lf_mixed_after_prop", b"one\ntwo\n", {"svn:eol-style": "LF"}, ["UTC"]),
    ("eol_crlf_mixed_after_prop", b"one\r\ntwo\r\n", {"svn:eol-style": "CRLF"}, ["UTC"]),
    ("eol_cr_mixed_after_prop", b"one\rtwo\r", {"svn:eol-style": "CR"}, ["UTC"]),
    ("eol_native_mixed_after_prop", b"one\ntwo\n", {"svn:eol-style": "native"}, ["UTC"]),
    ("kw_all", KW_ALL.encode(), {"svn:keywords": "Id Rev Revision LastChangedRevision Date LastChangedDate Author LastChangedBy HeadURL URL Header"}, ["UTC", "Asia/Ho_Chi_Minh", "America/St_Johns", "America/Los_Angeles"]),
    ("kw_fixed_too_short_lossy", b"fixed: $Rev:: $ $Rev::            $\n$Id:: $\n", {"svn:keywords": "Rev Id"}, ["UTC"]),
    ("kw_subset", KW_ALL.encode(), {"svn:keywords": "Rev Author"}, ["UTC"]),
    ("kw_short_only", KW_ALL.encode(), {"svn:keywords": "Id"}, ["UTC"]),
    ("kw_case_insensitive_prop", KW_ALL.encode(), {"svn:keywords": "id REV date author url header"}, ["UTC"]),
    ("kw_whitespace_prop", KW_ALL.encode(), {"svn:keywords": "  Id\tRev\x0bDate \x0c"}, ["UTC"]),
    ("kw_custom", b"$Foo$ $Bar: x$ $Id$ $Baz$\n", {"svn:keywords": "Foo=%a-%r-%b Bar=%H Id Baz=%P|%R|%u|%d|%%|%_|%x|%"}, ["UTC"]),
    ("kw_long", LONG.encode(), {"svn:keywords": "Id Rev"}, ["UTC"]),
    ("kw_with_crlf", b"$Id$\r\n$Rev$\r\nx\r\n", {"svn:keywords": "Id Rev", "svn:eol-style": "CRLF"}, ["UTC"]),
    ("kw_with_native_crlf_source", b"$Id$\r\n$Rev$\r\nx\r\n", {"svn:keywords": "Id Rev", "svn:eol-style": "native"}, ["UTC"]),
    ("kw_empty_prop_value", b"$Id$\n", {"svn:keywords": ""}, ["UTC"]),
    ("kw_fixed_interior_dollar_expand_lossy", b"$Rev:: a$b $\n", {"svn:keywords": "Rev Id"}, ["UTC"]),
    ("kw_fixed_interior_dollar_contract_lossy", b"$Rev:: 5$b $\n", {"svn:keywords": "Rev Id"}, ["UTC"]),
    ("kw_chunk_boundary_normalize_lossy", b"x" * 16377 + b"$Rev:a$Id: x $\n", {"svn:keywords": "Rev Id"}, ["UTC"]),
    ("kw_encoded_url", b"$Id$ $HeadURL$ $URL$ $Header$ $Cb$\n", {"svn:keywords": "Id HeadURL URL Header Cb=%b|%P"}, ["UTC"]),
    ("kw_utf8_name", "Tên $Author$ $Id$ é\n".encode(), {"svn:keywords": "Author Id"}, ["UTC"]),
    ("special_symlink", None, {"svn:special": "*"}, ["UTC"]),
    ("executable_flag", b"#!/bin/sh\n", {"svn:executable": "*"}, ["UTC"]),
]

def run(args, env=None, check=True, cwd=None):
    r = subprocess.run(args, capture_output=True, env=env, cwd=cwd)
    if check and r.returncode != 0:
        raise RuntimeError(f"{args}: {r.stderr.decode(errors='replace')}")
    return r

def main():
    # A fixed, neutral location: repository URLs and svn error messages end up in the fixtures.
    work = "/tmp/opensvn-golden"
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    repo = os.path.join(work, "repo"); run(["svnadmin", "create", repo])
    url = "file://" + repo
    wc1 = os.path.join(work, "wc1"); run(["svn", "checkout", "-q", url, wc1])
    env = dict(os.environ, TZ="UTC", LC_ALL="en_US.UTF-8")
    version = run(["svn", "--version", "--quiet"]).stdout.decode().strip()
    committed = {}
    commit_inputs = {}
    for name, content, props, _ in CASES:
        rel = PATHS.get(name, name)
        p = os.path.join(wc1, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        if content is None:
            os.symlink("target/for link", p)
        else:
            open(p, "wb").write(content)
        run(["svn", "add", "-q", "--parents", p], env=env)
        err = None
        for k, v in props.items():
            r = run(["svn", "propset", "-q", k, v, p], env=env, check=False)
            if r.returncode != 0:
                err = r.stderr.decode(errors="replace").strip()
                break
        if err is None and name in LATER:
            open(p, "wb").write(LATER[name])
        pre_commit = None if (err or os.path.islink(p)) else open(p, "rb").read()
        if err is None:
            r = run(["svn", "commit", "-q", "--username", "alice", "-m", "add " + name, wc1], env=env, check=False)
            if r.returncode != 0:
                err = r.stderr.decode(errors="replace").strip()
        if err:
            run(["svn", "revert", "-q", p], env=env, check=False)
            os.remove(p)
            committed[name] = ("error", err)
        else:
            committed[name] = ("ok", None)
            # Exactly what `svn commit` read from the working file (input of the normaliser); svn
            # rewrites the file with expanded keywords right after the commit, so it is captured before.
            commit_inputs[name] = pre_commit
    for name, content, props, tzs in CASES:
        state, err = committed[name]
        rel = PATHS.get(name, name)
        out = os.path.join(HERE, name)
        shutil.rmtree(out, ignore_errors=True)
        os.makedirs(out)
        meta = [f"svn_version={version}"] + [f"prop.{k}={v}" for k, v in props.items()]
        if state == "error":
            open(os.path.join(out, "expect_error.txt"), "w").write(err + "\n")
            if content is not None:
                open(os.path.join(out, "working.bin"), "wb").write(LATER.get(name, content))
            open(os.path.join(out, "meta.txt"), "w").write("\n".join(meta) + "\n")
            continue
        for tz in tzs:
            wc2 = os.path.join(work, "co-" + name + "-" + tz.replace("/", "_"))
            env2 = dict(os.environ, TZ=tz, LC_ALL="en_US.UTF-8")
            run(["svn", "checkout", "-q", url, wc2], env=env2)
            f = os.path.join(wc2, rel)
            info = run(["svn", "info", "--xml", f], env=env2).stdout
            e = ET.fromstring(info).find("entry")
            rev = e.find("commit").get("revision")
            author = e.find("commit/author").text
            dt = e.find("commit/date").text  # 2026-10-13T08:11:12.123456Z
            d = datetime.strptime(dt, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)
            micros = int(d.timestamp()) * 1_000_000 + d.microsecond
            off = run(["date", "-r", str(int(d.timestamp())), "+%z"], env=env2).stdout.decode().strip()
            sign = -1 if off[0] == "-" else 1
            off_secs = sign * (int(off[1:3]) * 3600 + int(off[3:5]) * 60)
            root = e.find("repository/root").text
            furl = e.find("url").text
            db = sqlite3.connect(os.path.join(wc2, ".svn", "wc.db"))
            (chk,) = db.execute("select checksum from nodes where local_relpath=? and op_depth=0", (rel,)).fetchone()
            hexsum = chk.split("$")[-1]
            pristine = open(os.path.join(wc2, ".svn", "pristine", hexsum[:2], hexsum + ".svn-base"), "rb").read()
            sub = out if len(tzs) == 1 else os.path.join(out, tz.replace("/", "_"))
            os.makedirs(sub, exist_ok=True)
            open(os.path.join(sub, "pristine.bin"), "wb").write(pristine)
            if commit_inputs.get(name) is not None:
                open(os.path.join(sub, "commit.bin"), "wb").write(commit_inputs[name])
            if os.path.islink(f):
                open(os.path.join(sub, "working.link"), "w").write(os.readlink(f))
            else:
                open(os.path.join(sub, "working.bin"), "wb").write(open(f, "rb").read())
                open(os.path.join(sub, "working.mode"), "w").write("%o\n" % (os.stat(f).st_mode & 0o777))
            m = meta + [f"rev={rev}", f"author={author}", f"date_micros={micros}", f"utc_offset_secs={off_secs}",
                        f"url={furl}", f"root={root}", f"tz={tz}", f"path={rel}"]
            open(os.path.join(sub, "meta.txt"), "w").write("\n".join(m) + "\n")
    shutil.rmtree(work, ignore_errors=True)

if __name__ == "__main__":
    sys.exit(main())

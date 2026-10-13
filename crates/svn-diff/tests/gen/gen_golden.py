#!/usr/bin/env python3
"""Regenerates tests/golden/{unified,merge3} from a real Subversion client.

Usage: python3 -I gen_golden.py [svn-binary]

Requires `svn` and `svnadmin` (developed against 1.15.0).  Scratch repositories live in a
temporary directory; nothing outside tests/golden is touched.  Env: GOLDEN_OUT (output dir), GOLDEN_RANDOM (extra random cases per kind).  Inputs are deterministic
(seeded PRNG), the expected output is whatever `svn diff` / `svn update` produced.
"""
import os
import random
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
GOLDEN = os.environ.get("GOLDEN_OUT") or os.path.join(HERE, "..", "golden")
EXTRA_RANDOM = int(os.environ.get("GOLDEN_RANDOM", "0"))  # extra fuzz cases for local soak runs
SVN = sys.argv[1] if len(sys.argv) > 1 else "svn"
ENV = dict(os.environ, LC_ALL="C", LANG="C")


def run(args, cwd=None, check=True):
    p = subprocess.run(args, cwd=cwd, env=ENV, capture_output=True)
    if check and p.returncode != 0:
        raise RuntimeError("%s failed: %s" % (args, p.stderr.decode(errors="replace")))
    return p.stdout


def svn(cfg, args, cwd=None, check=True):
    return run([SVN, "--config-dir", cfg, "--non-interactive"] + args, cwd, check)


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def lines(n, prefix="line", start=0):
    return [("%s %d\n" % (prefix, i)).encode() for i in range(start, start + n)]


def j(ls):
    return b"".join(ls)


def rand_doc(rng, n, vocab=6, eol=b"\n", final_eol=True):
    words = [("w%d" % i).encode() for i in range(vocab)]
    ls = [rng.choice(words) + b" " + rng.choice(words) + eol for _ in range(n)]
    out = b"".join(ls)
    if not final_eol and out.endswith(eol):
        out = out[: -len(eol)]
    return out


def split(data):
    out, cur, i = [], b"", 0
    while i < len(data):
        c = data[i : i + 1]
        cur += c
        i += 1
        if c == b"\r" and data[i : i + 1] == b"\n":
            cur += b"\n"
            i += 1
            c = b"\n"
        if c in (b"\n", b"\r"):
            out.append(cur)
            cur = b""
    if cur:
        out.append(cur)
    return out


def mutate(rng, data, edits, vocab=6, tag=b"x"):
    ls = split(data)
    eol = b"\r\n" if b"\r\n" in data else b"\n"
    for _ in range(edits):
        op = rng.choice(["rep", "ins", "del"])
        if not ls:
            op = "ins"
        pos = rng.randrange(len(ls) + 1) if op == "ins" else rng.randrange(max(len(ls), 1))
        new = tag + b"%d" % rng.randrange(vocab) + b" " + b"v%d" % rng.randrange(vocab) + eol
        if op == "ins":
            ls.insert(pos, new)
        elif op == "del" and ls:
            ls.pop(pos)
        elif ls:
            ls[pos] = new
    return b"".join(ls)


def unified_cases():
    rng = random.Random(1013)
    base = j(lines(20))
    c = {}
    c["change-middle"] = (base, base.replace(b"line 10\n", b"LINE TEN\n"))
    c["add-first"] = (base, b"new first\n" + base)
    c["add-last"] = (base, base + b"new last\n")
    c["delete-first"] = (base, base.split(b"\n", 1)[1])
    c["delete-last"] = (base, b"\n".join(base.split(b"\n")[:-2]) + b"\n")
    c["replace-all"] = (base, j(lines(20, "other")))
    c["empty-to-content"] = (b"", base)
    c["content-to-empty"] = (base, b"")
    c["single-line-change"] = (b"hello\n", b"world\n")
    c["single-line-no-eol"] = (b"hello", b"world")
    c["eol-added-at-end"] = (b"a\nb", b"a\nb\n")
    c["eol-removed-at-end"] = (b"a\nb\n", b"a\nb")
    c["no-eol-both-last-changed"] = (b"a\nb\nc", b"a\nb\nC")
    c["no-eol-both-middle-changed"] = (b"a\nb\nc", b"a\nB\nc")
    c["no-eol-append-line"] = (b"a\nb", b"a\nb\nc")
    c["crlf-change"] = (b"a\r\nb\r\nc\r\nd\r\n", b"a\r\nB\r\nc\r\nd\r\n")
    c["crlf-to-lf"] = (b"a\r\nb\r\nc\r\n", b"a\nb\nc\n")
    c["crlf-no-final-eol"] = (b"a\r\nb\r\nc", b"a\r\nb\r\nC")
    c["cr-only"] = (b"a\rb\rc\rd\r", b"a\rB\rc\rd\r")
    c["mixed-eol"] = (b"a\nb\r\nc\rd\n", b"a\nb\r\nC\rd\n")
    c["adjacent-edits"] = (base, base.replace(b"line 5\n", b"A\n").replace(b"line 6\n", b"B\n"))
    gap = lambda g: base.replace(b"line 3\n", b"X\n").replace(b"line %d\n" % (3 + g), b"Y\n")
    c["hunks-gap-6"] = (base, gap(7))   # exactly 6 unchanged lines between changes
    c["hunks-gap-7"] = (base, gap(8))   # 7 unchanged lines: separate hunks
    c["hunks-gap-5"] = (base, gap(6))
    c["blank-line-insert"] = (base, base.replace(b"line 7\n", b"line 7\n\n"))
    c["blank-lines-only"] = (b"a\n\n\n\nb\n", b"a\n\nb\n")
    c["whitespace-change"] = (b"a b\n  c\n", b"a  b\n\tc\n")
    c["trailing-space"] = (b"a\nb\n", b"a \nb\n")
    c["latin1-bytes"] = (b"caf\xe9 one\nplain\n", b"caf\xe9 two\nplain\n")
    c["utf8-text"] = ("héllo\nwörld\n".encode(), "héllo\nwörld!\n".encode())
    c["repeated-lines"] = (b"a\na\na\nb\na\na\n", b"a\na\nb\na\na\na\n")
    c["duplicate-block-move"] = (j(lines(6) + lines(6, "blk") + lines(3, "end")), j(lines(6, "blk") + lines(6) + lines(3, "end")))
    c["long-common-prefix-suffix"] = (j(lines(300)), j(lines(150) + [b"changed\n"] + lines(149, start=151)))
    c["large-scattered"] = (j(lines(3000)), mutate(rng, j(lines(3000)), 40, 1000))
    c["large-delete-block"] = (j(lines(2000)), j(lines(800) + lines(1000, start=1000)))
    c["large-insert-block"] = (j(lines(1000)), j(lines(500) + lines(300, "ins") + lines(500, start=500)))
    c["tiny-vs-big"] = (b"x\n", j(lines(50)))
    for i in range(14 + EXTRA_RANDOM):
        n = rng.randrange(5, 60)
        d = rand_doc(rng, n, vocab=rng.choice([3, 5, 12]),
                     eol=rng.choice([b"\n", b"\n", b"\r\n"]), final_eol=rng.random() < 0.8)
        c["random-%02d" % i] = (d, mutate(rng, d, rng.randrange(1, 8), vocab=5))
    return c


# Cases run with whitespace/EOL comparison options: name -> (orig, mod, option code).
OPT_FLAGS = {"w": ["-x", "-w"], "b": ["-x", "-b"], "e": ["-x", "--ignore-eol-style"]}


def perturb_ws(rng, data):
    out = []
    for l in split(data):
        body = l.rstrip(b"\r\n")
        eol = l[len(body):]
        r = rng.random()
        if r < 0.3:
            body = b"  " + body
        elif r < 0.5:
            body = body + b" \t"
        elif r < 0.7:
            body = body.replace(b" ", b"   ", 1)
        out.append(body + eol)
    return b"".join(out)


def option_cases():
    rng = random.Random(4242)
    base = j(lines(20))
    c = {}
    c["w-inner-and-real"] = (b"a b c\nx\nlast\n", b"ab c\ny\nlast\n", "w")
    c["w-indent-plus-real"] = (base, perturb_ws(rng, base).replace(b"line 9", b"CHANGED 9"), "w")
    c["w-tabs"] = (b"\tint x;\nfoo bar\n", b"    int  x ;\nfoo bar!\n", "w")
    c["b-runs-and-real"] = (b"a  b\nc\nd\n", b"a b\nC\nd\n", "b")
    c["b-trailing-plus-real"] = (base, perturb_ws(rng, base).replace(b"line 3", b"CHANGED 3"), "b")
    c["b-leading-vs-none"] = (b"x\n  y\nz\n", b"x\ny\nZ\n", "b")
    c["e-crlf-to-lf-plus-real"] = (b"a\r\nb\r\nc\r\nd\r\n", b"a\nB\nc\nd\n", "e")
    c["e-cr-to-lf"] = (b"a\rb\rc\r", b"a\nb\nX\n", "e")
    c["e-mixed-to-crlf"] = (b"a\nb\r\nc\rd\n", b"a\r\nb\r\nc\r\nD\r\n", "e")
    c["e-no-final-eol"] = (b"a\r\nb", b"a\nb\nc", "e")
    for i in range(6):
        d = rand_doc(rng, rng.randrange(8, 40), vocab=5, eol=rng.choice([b"\n", b"\r\n"]))
        m = mutate(rng, perturb_ws(rng, d), 2, vocab=5)
        if rng.random() < 0.5:
            m = m.replace(b"\r\n", b"\n")
        c["random-%d-%s" % (i, "web"[i % 3])] = (d, m, "web"[i % 3])
    return c


def gen_unified(cfg, tmp):
    cases = unified_cases()
    opt_cases = option_cases()
    for n, (o, m, code) in opt_cases.items():
        cases["opt-" + n] = (o, m)
    repo = os.path.join(tmp, "urepo")
    run(["svnadmin", "create", repo])
    url = "file://" + repo
    wc = os.path.join(tmp, "uwc")
    svn(cfg, ["checkout", url, wc])
    names = sorted(cases)
    for n in names:
        write(os.path.join(wc, n, "f"), cases[n][0])
    svn(cfg, ["add", "--force", "-q", "."], cwd=wc)
    svn(cfg, ["commit", "-q", "-m", "base"], cwd=wc)
    out_root = os.path.join(GOLDEN, "unified")
    shutil.rmtree(out_root, ignore_errors=True)
    kept = 0
    for n in names:
        orig, mod = cases[n]
        if orig == mod:
            continue
        d = os.path.join(wc, n)
        write(os.path.join(d, "f"), mod)
        code = opt_cases.get(n[4:], (None, None, None))[2] if n.startswith("opt-") else None
        out = svn(cfg, ["diff"] + (OPT_FLAGS[code] if code else []) + ["f"], cwd=d)
        if b"Cannot display" in out or b"@@" not in out:
            print("skip", n)
            continue
        dest = os.path.join(out_root, n)
        write(os.path.join(dest, "orig"), orig)
        write(os.path.join(dest, "mod"), mod)
        write(os.path.join(dest, "expected.diff"), out)
        if code:
            write(os.path.join(dest, "opts"), code.encode())
        kept += 1
    # Added and deleted files: the header labels differ.
    for n, data in (("added-file", j(lines(8))), ("deleted-file", j(lines(8)))):
        d = os.path.join(wc, n)
        if n == "added-file":
            write(os.path.join(d, "f"), data)
            svn(cfg, ["add", "-q", d])
            out = svn(cfg, ["diff", "f"], cwd=d)
            orig, mod = b"", data
        else:
            write(os.path.join(d, "f"), data)
            svn(cfg, ["add", "-q", d])
            svn(cfg, ["commit", "-q", "-m", "case " + n], cwd=wc)
            svn(cfg, ["rm", "-q", os.path.join(d, "f")])
            out = svn(cfg, ["diff", "f"], cwd=d)
            orig, mod = data, b""
        dest = os.path.join(out_root, n)
        write(os.path.join(dest, "orig"), orig)
        write(os.path.join(dest, "mod"), mod)
        write(os.path.join(dest, "expected.diff"), out)
        kept += 1
    print("unified cases:", kept)


def merge_cases():
    rng = random.Random(7013)
    base = j(lines(12))
    ed = lambda d, *pairs: d if not pairs else ed(d.replace(pairs[0][0], pairs[0][1]), *pairs[1:])
    c = {}
    c["clean-distant"] = (base, ed(base, (b"line 1\n", b"M1\n")), ed(base, (b"line 10\n", b"T10\n")))
    c["conflict-same-line"] = (base, ed(base, (b"line 5\n", b"mine\n")), ed(base, (b"line 5\n", b"theirs\n")))
    c["adjacent-lines"] = (base, ed(base, (b"line 5\n", b"mine\n")), ed(base, (b"line 6\n", b"theirs\n")))
    c["identical-edit"] = (base, ed(base, (b"line 5\n", b"same\n")), ed(base, (b"line 5\n", b"same\n")))
    c["identical-edit-plus-mine"] = (base, ed(base, (b"line 5\n", b"same\n"), (b"line 1\n", b"m\n")), ed(base, (b"line 5\n", b"same\n")))
    c["identical-insert"] = (base, base + b"new\n", base + b"new\n")
    c["both-delete-same"] = (base, ed(base, (b"line 4\n", b"")), ed(base, (b"line 4\n", b"")))
    c["mine-delete-theirs-modify"] = (base, ed(base, (b"line 4\n", b"")), ed(base, (b"line 4\n", b"T\n")))
    c["mine-modify-theirs-delete"] = (base, ed(base, (b"line 4\n", b"M\n")), ed(base, (b"line 4\n", b"")))
    c["mine-delete-theirs-elsewhere"] = (base, ed(base, (b"line 2\n", b"")), ed(base, (b"line 9\n", b"T\n")))
    c["append-different"] = (base, base + b"mine tail\n", base + b"theirs tail\n")
    c["prepend-different"] = (base, b"mine head\n" + base, b"theirs head\n" + base)
    c["insert-same-spot"] = (base, ed(base, (b"line 6\n", b"mine ins\nline 6\n")), ed(base, (b"line 6\n", b"theirs ins\nline 6\n")))
    c["insert-different-spots"] = (base, ed(base, (b"line 2\n", b"mine ins\nline 2\n")), ed(base, (b"line 9\n", b"theirs ins\nline 9\n")))
    c["mine-empties-file"] = (base, b"", ed(base, (b"line 3\n", b"T\n")))
    c["theirs-empties-file"] = (base, ed(base, (b"line 3\n", b"M\n")), b"")
    c["both-empty"] = (base, b"", b"")
    c["mine-empty-theirs-appends"] = (base, b"", base + b"more\n")
    c["from-empty-base"] = (b"", b"mine\n", b"theirs\n")
    c["from-empty-base-same"] = (b"", b"same\n", b"same\n")
    c["crlf-clean"] = (b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\n", b"A\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\n", b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nH\r\n")
    c["crlf-conflict"] = (b"a\r\nb\r\nc\r\n", b"a\r\nM\r\nc\r\n", b"a\r\nT\r\nc\r\n")
    c["cr-only-conflict"] = (b"a\rb\rc\r", b"a\rM\rc\r", b"a\rT\rc\r")
    c["mine-lf-theirs-crlf"] = (b"a\nb\nc\n", b"a\nM\nc\n", b"a\r\nb\r\nC\r\n")
    c["no-eol-conflict-last"] = (b"a\nb\nc", b"a\nb\nM", b"a\nb\nT")
    c["no-eol-theirs-adds-eol"] = (b"a\nb", b"A\nb", b"a\nb\n")
    c["no-eol-mine-adds-eol"] = (b"a\nb", b"a\nb\n", b"a\nB")
    c["no-eol-clean"] = (b"a\nb\nc\nd\ne\nf\ng", b"A\nb\nc\nd\ne\nf\ng", b"a\nb\nc\nd\ne\nf\nG")
    c["multi-conflict"] = (j(lines(30)), ed(j(lines(30)), (b"line 3\n", b"m3\n"), (b"line 15\n", b"m15\n"), (b"line 27\n", b"m27\n")), ed(j(lines(30)), (b"line 3\n", b"t3\n"), (b"line 15\n", b"t15\n"), (b"line 27\n", b"t27\n")))
    c["mixed-conflict-and-clean"] = (j(lines(30)), ed(j(lines(30)), (b"line 3\n", b"m3\n"), (b"line 20\n", b"m20\n")), ed(j(lines(30)), (b"line 3\n", b"t3\n"), (b"line 10\n", b"t10\n")))
    c["multi-line-overlap"] = (j(lines(20)), ed(j(lines(20)), (b"line 5\nline 6\nline 7\n", b"m\nm\n")), ed(j(lines(20)), (b"line 6\nline 7\nline 8\n", b"t\nt\nt\nt\n")))
    c["partial-overlap-common-tail"] = (j(lines(20)), ed(j(lines(20)), (b"line 5\nline 6\n", b"m5\nline 6x\nshared\n")), ed(j(lines(20)), (b"line 5\nline 6\n", b"t5\nline 6y\nshared\n")))
    c["conflict-common-middle"] = (b"a\nb\nc\nd\ne\n", b"a\nM1\nc\nM2\ne\n", b"a\nT1\nc\nT2\ne\n")
    c["conflict-same-prefix"] = (b"a\nb\nc\nd\ne\n", b"a\nS\nM\ne\n", b"a\nS\nT\ne\n")
    c["conflict-at-start"] = (base, ed(base, (b"line 0\n", b"M\n")), ed(base, (b"line 0\n", b"T\n")))
    c["conflict-at-end"] = (base, ed(base, (b"line 11\n", b"M\n")), ed(base, (b"line 11\n", b"T\n")))
    c["conflict-whole-file"] = (b"x\n", b"m\n", b"t\n")
    c["repeated-lines"] = (b"a\na\na\nb\na\na\n", b"a\nX\na\nb\na\na\n", b"a\na\na\nb\nY\na\n")
    c["repeated-lines-conflict"] = (b"a\na\na\na\n", b"a\nM\na\na\n", b"a\nT\na\na\n")
    c["blank-lines"] = (b"a\n\n\nb\n\nc\n", b"a\n\nM\n\nb\n\nc\n", b"a\n\n\nb\n\nT\n")
    c["long-prefix-suffix"] = (j(lines(400)), ed(j(lines(400)), (b"line 200\n", b"M\n")), ed(j(lines(400)), (b"line 200\n", b"T\n")))
    c["large-clean"] = (j(lines(3000)), mutate(rng, j(lines(3000)), 10, 1000, b"m"), j(lines(3000)))
    c["large-many-conflicts"] = (j(lines(2000)), ed(j(lines(2000)), *[(b"line %d\n" % k, b"m%d\n" % k) for k in range(50, 2000, 100)]), ed(j(lines(2000)), *[(b"line %d\n" % k, b"t%d\n" % k) for k in range(50, 2000, 100)] + [(b"line 1000\n", b"extra\n")]))
    c["large-mixed"] = (j(lines(2000)), ed(j(lines(2000)), *[(b"line %d\n" % k, b"m%d\n" % k) for k in range(10, 2000, 50)]), ed(j(lines(2000)), *[(b"line %d\n" % k, b"t%d\n" % k) for k in range(35, 2000, 50)] + [(b"line 10\n", b"t10\n")]))
    for i in range(22 + EXTRA_RANDOM):
        n = rng.randrange(6, 50)
        d = rand_doc(rng, n, vocab=rng.choice([3, 6, 15]),
                     eol=rng.choice([b"\n", b"\n", b"\r\n"]), final_eol=rng.random() < 0.8)
        m = mutate(rng, d, rng.randrange(1, 6), vocab=5, tag=b"m")
        t = mutate(rng, d, rng.randrange(1, 6), vocab=5, tag=b"t")
        c["random-%02d" % i] = (d, m, t)
    return c


def gen_merge3(cfg, tmp):
    cases = merge_cases()
    repo = os.path.join(tmp, "mrepo")
    run(["svnadmin", "create", repo])
    url = "file://" + repo
    wc1 = os.path.join(tmp, "mwc1")
    wc2 = os.path.join(tmp, "mwc2")
    svn(cfg, ["checkout", "-q", url, wc1])
    names = sorted(n for n, (b, m, t) in cases.items() if m != b and t != b)
    for n in names:
        write(os.path.join(wc1, n, "f"), cases[n][0])
    svn(cfg, ["add", "--force", "-q", "."], cwd=wc1)
    svn(cfg, ["commit", "-q", "-m", "base"], cwd=wc1)
    svn(cfg, ["checkout", "-q", url, wc2])
    for n in names:
        write(os.path.join(wc2, n, "f"), cases[n][2])
        write(os.path.join(wc1, n, "f"), cases[n][1])
    svn(cfg, ["commit", "-q", "-m", "theirs"], cwd=wc2)
    modes = {"postpone": "postpone", "mine-conflict": "mine-conflict", "theirs-conflict": "theirs-conflict"}
    copies = {}
    for mode in modes:
        copies[mode] = os.path.join(tmp, "mwc-" + mode)
        shutil.copytree(wc1, copies[mode], symlinks=True)
    out_root = os.path.join(GOLDEN, "merge3")
    shutil.rmtree(out_root, ignore_errors=True)
    for mode, accept in modes.items():
        svn(cfg, ["update", "-q", "--accept", "postpone"], cwd=copies[mode])
        if accept != "postpone":
            svn(cfg, ["resolve", "-q", "-R", "--accept", accept, "."], cwd=copies[mode])
    for n in names:
        dest = os.path.join(out_root, n)
        base, mine, theirs = cases[n]
        write(os.path.join(dest, "base"), base)
        write(os.path.join(dest, "mine"), mine)
        write(os.path.join(dest, "theirs"), theirs)
        for mode in modes:
            with open(os.path.join(copies[mode], n, "f"), "rb") as f:
                write(os.path.join(dest, "expected." + mode), f.read())
    print("merge3 cases:", len(names))


def main():
    tmp = tempfile.mkdtemp(prefix="svn-diff-golden-")
    cfg = os.path.join(tmp, "cfg")
    os.makedirs(cfg)
    try:
        gen_unified(cfg, tmp)
        gen_merge3(cfg, tmp)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


main()

#!/usr/bin/env python3
"""Generates the hostile-server fixture set in this directory (deterministic, stdlib only).

usage: python3 fixtures/evil/generate.py

Writes `<category>/<case>.<ext>` and `cases.tsv` (id, parser, file, expect, description).
The parser column names the engine entry point a test must feed the file to; `expect` is the
required outcome (see README.md). Tests: crates/svn-protocol/tests/evil_server.rs,
crates/svn-delta/tests/evil_svndiff.rs, crates/svn-ra/tests/evil_transport.rs.
Session URL the documents assume: http://svn.test.local/svn/repo (repository root /svn/repo).
"""
import base64
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
CASES = []


def write(category, name, data, parser, expect, desc):
    ext = {"svndiff": "svndiff", "svndiff-apply": "svndiff", "options": "headers"}.get(parser, "xml")
    rel = f"{category}/{name}.{ext}"
    os.makedirs(os.path.join(HERE, category), exist_ok=True)
    with open(os.path.join(HERE, rel), "wb") as f:
        f.write(data if isinstance(data, bytes) else data.encode("utf-8"))
    CASES.append((f"{category}/{name}", parser, rel, expect, desc))


# ---------------------------------------------------------------- update-report (send-all) ---
def update(body, rev=5):
    return f"""<?xml version="1.0" encoding="utf-8"?>
<S:update-report xmlns:S="svn:" xmlns:V="http://subversion.tigris.org/xmlns/dav/" xmlns:D="DAV:" send-all="true" inline-props="true">
<S:target-revision rev="{rev}"/>
<S:open-directory rev="4">
<D:checked-in><D:href>/svn/repo/!svn/rvr/5/trunk</D:href></D:checked-in>
{body}
</S:open-directory>
</S:update-report>
"""


def add_file(name, href=None):
    href = href or f"/svn/repo/!svn/rvr/5/trunk/x"
    return (f'<S:add-file name="{name}">\n<D:checked-in><D:href>{href}</D:href></D:checked-in>\n'
            f'<S:txdelta>{base64.b64encode(b"SVN" + bytes([0])).decode()}</S:txdelta>\n</S:add-file>')


def add_dir(name, inner=""):
    return (f'<S:add-directory name="{name}">\n<D:checked-in><D:href>/svn/repo/!svn/rvr/5/trunk/d'
            f'</D:href></D:checked-in>\n{inner}\n</S:add-directory>')


U = "update"
write("update", "name-dotdot", update(add_file("../escape.txt")), U, "unsafe",
      "add-file name climbs out of the working copy (../)")
write("update", "dir-dotdot", update(add_dir("..", add_file("pwned.txt"))), U, "unsafe",
      "add-directory named .. with a file inside")
write("update", "name-dot", update(add_dir(".")), U, "unsafe", "add-directory named .")
write("update", "name-absolute", update(add_file("/etc/passwd")), U, "unsafe",
      "absolute POSIX path as a node name")
write("update", "name-slash-traversal", update(add_file("sub/../../escape.txt")), U, "unsafe",
      "node name containing separators and ..")
write("update", "name-windows-drive", update(add_file("C:\\Windows\\evil.dll")), U, "unsafe",
      "Windows drive-absolute path as a node name")
write("update", "name-drive-relative", update(add_file("C:evil.txt")), U, "unsafe",
      "Windows drive-relative name (C:evil.txt)")
write("update", "name-unc", update(add_file("\\\\server\\share\\evil.txt")), U, "unsafe",
      "UNC path as a node name")
write("update", "name-backslash-dotdot", update(add_file("..\\..\\escape.txt")), U, "unsafe",
      "backslash traversal (..\\..\\)")
write("update", "admin-dir", update(add_dir(".svn", add_file("wc.db"))), U, "unsafe",
      "add-directory .svn, then wc.db inside it (admin area overwrite)")
write("update", "admin-dir-case", update(add_dir(".SVN")), U, "unsafe",
      "admin directory in another case (.SVN, case-insensitive file systems)")
write("update", "dos-device", update(add_file("CON.txt")), U, "unsafe",
      "reserved DOS device name")
write("update", "ads-stream", update(add_file("a:b")), U, "unsafe",
      "NTFS alternate data stream name (a:b)")
write("update", "trailing-dot-space", update(add_file("evil. ")), U, "unsafe",
      "trailing dot/space (Windows strips them, aliasing another name)")
write("update", "delete-dotdot", update('<S:delete-entry name="../victim"/>'), U, "unsafe",
      "delete-entry outside the working copy")
write("update", "href-other-host",
      update(add_file("a.txt", "http://evil.test.local/svn/repo/!svn/rvr/5/trunk/a.txt")), U,
      "unsafe", "checked-in href pointing at another host")
write("update", "href-traversal",
      update(add_file("a.txt", "/svn/repo/!svn/rvr/5/../../../etc/passwd")), U, "unsafe",
      "checked-in href with .. segments")
write("update", "href-encoded-traversal",
      update(add_file("a.txt", "/svn/repo/!svn/rvr/5/trunk/%2e%2e/%2e%2e/x")), U, "unsafe",
      "checked-in href with percent-encoded .. segments")
write("update", "href-outside-repo",
      update(add_file("a.txt", "/other/!svn/rvr/5/trunk/a.txt")), U, "unsafe",
      "checked-in href outside the repository root")
# The whitespace cases below (and propfind/child-dotdot-space) document intent only: the XML
# parsers trim href text before vetting it, so these pass even without the whitespace check in
# check_server_url_text. The unit test the_whitespace_fixture_hrefs_are_refused_untrimmed
# (crates/svn-protocol/src/caps/url_builder.rs) guards that check.
write("update", "href-trailing-space-dotdot",
      update(add_file("a.txt", "/svn/repo/!svn/rvr/5/trunk/.. ")), U, "unsafe",
      "checked-in href ending in '.. ' (the URL parser strips the space, leaving ..)")
write("update", "href-encoded-dotdot-space",
      update(add_file("a.txt", "/svn/repo/!svn/rvr/5/trunk/%2e%2e ")), U, "unsafe",
      "checked-in href ending in an encoded .. plus a space")
write("update", "href-scheme-in-path",
      update(add_file("a.txt", "/svn/repo/!svn/rvr/5/../../../etc/x://y")), U, "unsafe",
      "checked-in href with .. segments before a '://' inside the path (not a scheme)")
write("update", "truncated", update(add_file("a.txt"))[:-60], U, "corrupt",
      "document cut off in the middle")
write("update", "mismatched-tags", update("<S:add-file name=\"a\"></S:add-directory>"), U, "corrupt",
      "end tag does not match start tag")
write("update", "billion-laughs", """<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
<!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">]>
<S:update-report xmlns:S="svn:">&lol3;</S:update-report>
""", U, "corrupt", "DOCTYPE with nested entity expansion")
write("update", "external-entity", """<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
<S:update-report xmlns:S="svn:" send-all="true"><S:target-revision rev="1"/>&xxe;</S:update-report>
""", U, "corrupt", "external entity (XXE)")
write("update", "invalid-utf8", update(add_file("a.txt")).encode().replace(b'name="a.txt"', b'name="a\xff\xfe.txt"'),
      U, "corrupt", "invalid UTF-8 in a node name")
write("update", "nul-char-ref", update(add_file("a&#0;b")), U, "corrupt",
      "NUL character reference in a node name")
write("update", "bad-revision", update(add_file("a.txt"), rev="99999999999999999999999"), U,
      "corrupt", "target revision overflows")
write("update", "negative-revision", update(add_file("a.txt"), rev="-7"), U, "corrupt",
      "negative target revision")
write("update", "not-xml", b"\x00\x01\x02 this is not XML \xff", U, "corrupt",
      "binary garbage instead of XML")

# ------------------------------------------------------------------ PROPFIND depth 1 (list) ---
def multistatus(hrefs):
    parts = []
    for h in hrefs:
        parts.append(f"""<D:response xmlns:lp1="DAV:" xmlns:lp2="http://subversion.tigris.org/xmlns/dav/">
<D:href>{h}</D:href>
<D:propstat><D:prop><lp1:resourcetype/><lp1:getcontentlength>4</lp1:getcontentlength>
<lp1:version-name>3</lp1:version-name><lp2:deadprop-count>0</lp2:deadprop-count></D:prop>
<D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>""")
    return ('<?xml version="1.0" encoding="utf-8"?>\n<D:multistatus xmlns:D="DAV:">\n'
            '<D:response><D:href>/svn/repo/!svn/rvr/5/trunk/</D:href><D:propstat><D:prop>'
            '<D:resourcetype><D:collection/></D:resourcetype><D:version-name>4</D:version-name>'
            '</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n'
            + "\n".join(parts) + "\n</D:multistatus>\n")


L = "propfind-list"
# Listings quarantine a bad child (ListItem::Rejected) instead of failing the whole listing.
write("propfind", "child-dotdot", multistatus(["/svn/repo/!svn/rvr/5/trunk/../../etc/passwd"]), L,
      "quarantine", "list entry href climbs out of the listed directory")
write("propfind", "child-encoded-dotdot", multistatus(["/svn/repo/!svn/rvr/5/trunk/%2e%2e"]), L,
      "quarantine", "list entry href is a percent-encoded ..")
write("propfind", "child-encoded-slash", multistatus(["/svn/repo/!svn/rvr/5/trunk/a%2f..%2f..%2fb"]),
      L, "quarantine", "percent-encoded separators inside one entry name")
write("propfind", "child-other-host",
      multistatus(["http://evil.test.local/svn/repo/!svn/rvr/5/trunk/a.txt"]), L, "quarantine",
      "list entry href on another host")
write("propfind", "child-outside-target", multistatus(["/svn/repo/!svn/rvr/5/branches/x.txt"]), L,
      "quarantine", "list entry that is not a child of the listed directory")
write("propfind", "child-admin", multistatus(["/svn/repo/!svn/rvr/5/trunk/.svn"]), L, "quarantine",
      "list entry named .svn")
write("propfind", "child-backslash", multistatus(["/svn/repo/!svn/rvr/5/trunk/..%5c..%5cx"]), L,
      "quarantine", "list entry with encoded backslash traversal")
write("propfind", "child-nul", multistatus(["/svn/repo/!svn/rvr/5/trunk/a%00b"]), L, "quarantine",
      "list entry with an encoded NUL")
write("propfind", "child-dotdot-space", multistatus(["/svn/repo/!svn/rvr/5/trunk/.. "]), L,
      "quarantine", "list entry href ending in '.. ' (the URL parser strips the space)")
write("propfind", "child-scheme-in-path",
      multistatus(["/svn/repo/!svn/rvr/5/trunk/../../../etc/x://y"]), L, "quarantine",
      "list entry href with .. before a '://' inside the path (not a scheme)")

# ------------------------------------------------------------------------------- log-report ---
def log(path_xml, msg="m"):
    return f"""<?xml version="1.0" encoding="utf-8"?>
<S:log-report xmlns:S="svn:" xmlns:D="DAV:">
<S:log-item>
{path_xml}
<D:version-name>3</D:version-name>
<S:date>2024-01-01T03:00:00.000000Z</S:date>
<D:comment>{msg}</D:comment>
<D:creator-displayname>fixture</D:creator-displayname>
</S:log-item>
</S:log-report>
"""


G = "log"
write("log", "changed-path-dotdot",
      log('<S:modified-path node-kind="file" text-mods="true" prop-mods="false">/trunk/../../etc/passwd</S:modified-path>'),
      G, "corrupt", "changed path with .. segments")
write("log", "changed-path-relative",
      log('<S:added-path node-kind="file" text-mods="true" prop-mods="false">trunk/a.txt</S:added-path>'),
      G, "corrupt", "changed path that is not absolute in the repository")
write("log", "copyfrom-dotdot",
      log('<S:added-path node-kind="file" copyfrom-path="/../secret" copyfrom-rev="1" text-mods="true" prop-mods="false">/trunk/a.txt</S:added-path>'),
      G, "corrupt", "copyfrom path with ..")
write("log", "html-message", log("", "&lt;script&gt;alert(1)&lt;/script&gt;&lt;img src=x onerror=alert(1)&gt;"), G,
      "ok-verbatim", "commit message with HTML/script: kept as text, never interpreted")
write("log", "bad-revision",
      log("").replace("<D:version-name>3</D:version-name>", "<D:version-name>3x</D:version-name>"),
      G, "corrupt", "non-numeric revision")
write("log", "truncated", log("")[:-40], G, "corrupt", "document cut off")

# ----------------------------------------------------------------------------------- merge ---
def merge(href, ver="6"):
    return f"""<?xml version="1.0" encoding="utf-8"?>
<D:merge-response xmlns:D="DAV:"><D:updated-set><D:response>
<D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype><D:baseline/></D:resourcetype>
<D:version-name>{ver}</D:version-name><D:creationdate>2024-01-01T06:00:00.000000Z</D:creationdate>
<D:creator-displayname>fixture</D:creator-displayname></D:prop>
<D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:updated-set></D:merge-response>
"""


write("merge", "bad-new-revision", merge("/svn/repo/!svn/vcc/default", "-1"), "merge", "corrupt",
      "MERGE answers a negative new revision")
write("merge", "garbage-revision", merge("/svn/repo/!svn/vcc/default", "six"), "merge", "corrupt",
      "MERGE answers a non-numeric new revision")

# ------------------------------------------------------------------------------- get-locks ---
def locks(path):
    return f"""<?xml version="1.0" encoding="utf-8"?>
<S:get-locks-report xmlns:S="svn:" xmlns:D="DAV:"><S:lock><S:path>{path}</S:path>
<S:token>opaquelocktoken:a51c8c59-0dda-4d35-bb7f-b4ab1f3d2ee9</S:token>
<S:creationdate>2024-01-01T01:00:00.000000Z</S:creationdate><S:owner>fixture</S:owner>
</S:lock></S:get-locks-report>
"""


write("locks", "path-dotdot", locks("/trunk/../../etc/passwd"), "get-locks", "corrupt",
      "lock reported on a path with ..")
write("locks", "path-relative", locks("trunk/a.txt"), "get-locks", "corrupt",
      "lock path not absolute in the repository")

# ---------------------------------------------------------------------- OPTIONS (headers) ---
BASE_OPTS = [
    ("DAV", "1,2"), ("DAV", "version-control,checkout,working-resource"),
    ("SVN-Youngest-Rev", "5"), ("SVN-Repository-UUID", "7462d98e-4071-4508-b797-d2922b5cda8f"),
    ("SVN-Repository-Root", "/svn/repo"), ("SVN-Me-Resource", "/svn/repo/!svn/me"),
    ("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr"), ("SVN-Rev-Stub", "/svn/repo/!svn/rev"),
    ("SVN-Txn-Root-Stub", "/svn/repo/!svn/txr"), ("SVN-Txn-Stub", "/svn/repo/!svn/txn"),
]


def options(**override):
    out = []
    for k, v in BASE_OPTS:
        out.append(f"{k}: {override.get(k, v)}")
    return "\n".join(out) + "\n"


O = "options"
write("options", "root-other-host", options(**{"SVN-Repository-Root": "http://evil.test.local/svn/repo"}),
      O, "reject", "repository root on another host")
write("options", "root-not-prefix", options(**{"SVN-Repository-Root": "/elsewhere"}), O,
      "reject", "repository root that is not a prefix of the session URL")
write("options", "root-dotdot", options(**{"SVN-Repository-Root": "/svn/repo/.."}), O,
      "reject", "repository root with a .. segment")
write("options", "root-backslash-dotdot", options(**{"SVN-Repository-Root": "/svn/repo\\.."}), O,
      "reject", "repository root with a backslash .. segment (the URL parser reads \\ as /)")
write("options", "root-abs-backslash-dotdot",
      options(**{"SVN-Repository-Root": "http://svn.test.local\\svn\\repo\\.."}), O, "reject",
      "absolute repository root with a backslash right after the host and a .. segment")
write("options", "root-tab-dotdot", options(**{"SVN-Repository-Root": "/svn/repo/.\t."}), O,
      "reject", "repository root with a tab between dots (the URL parser deletes tabs)")
write("options", "root-encoded-tab-dotdot", options(**{"SVN-Repository-Root": "/svn/repo/%2e%09%2e"}),
      O, "reject", "repository root with an encoded tab between encoded dots")
write("options", "root-encoded-backslash", options(**{"SVN-Repository-Root": "/svn/repo%5C.."}), O,
      "reject", "repository root with an encoded backslash before ..")
write("options", "me-tab", options(**{"SVN-Me-Resource": "/svn/repo/!svn/.\t./me"}), O,
      "reject", "me resource with a tab between dots")
write("options", "me-other-host", options(**{"SVN-Me-Resource": "http://evil.test.local/!svn/me"}),
      O, "reject", "me resource on another host (commits would go there)")
write("options", "stub-other-host", options(**{"SVN-Rev-Root-Stub": "//evil.test.local/svn/repo/!svn/rvr"}),
      O, "reject", "protocol-relative stub URL on another host")
write("options", "stub-scheme-in-path",
      options(**{"SVN-Rev-Root-Stub": "/svn/repo/!svn/../../x://y"}), O, "reject",
      "stub with .. segments before a '://' inside the path (not a scheme)")
write("options", "root-scheme-in-path",
      options(**{"SVN-Repository-Root": "/svn/repo/trunk/..://x"}), O, "reject",
      "repository root whose '..' segment is followed by '://' (hidden from a scheme split)")
write("options", "youngest-garbage", options(**{"SVN-Youngest-Rev": "99999999999999999999999"}), O,
      "reject", "youngest revision overflows")

# -------------------------------------------------------------------------------- svndiff ---
def varint(n):
    out = [n & 0x7F]
    n >>= 7
    while n:
        out.append(0x80 | (n & 0x7F))
        n >>= 7
    return bytes(reversed(out))


def window(sv_off, sv_len, tv_len, ins, new):
    return varint(sv_off) + varint(sv_len) + varint(tv_len) + varint(len(ins)) + varint(len(new)) + ins + new


def op(kind, length, offset=None):
    # kind: 0 source copy, 1 target copy, 2 new data; lengths < 64 inline.
    assert length < 64
    b = bytes([(kind << 6) | length])
    return b + (varint(offset) if offset is not None else b"")


V0 = b"SVN\x00"
V1 = b"SVN\x01"
D = "svndiff"
A = "svndiff-apply"
write("svndiff", "bad-magic", b"SVX\x00" + window(0, 0, 1, op(2, 1), b"a"), D, "corrupt",
      "wrong magic bytes")
write("svndiff", "unknown-version", b"SVN\x07" + window(0, 0, 1, op(2, 1), b"a"), D, "corrupt",
      "unsupported svndiff version 7")
write("svndiff", "truncated-header", b"SV", D, "corrupt", "stream ends inside the header")
write("svndiff", "truncated-window", V0 + window(0, 0, 5, op(2, 5), b"hello")[:-2], D, "corrupt",
      "stream ends inside a window")
write("svndiff", "varint-overflow", V0 + b"\xff" * 11 + b"\x00", D, "corrupt",
      "varint longer than 64 bits")
write("svndiff", "huge-target-view", V0 + varint(0) + varint(0) + varint(1 << 40) + varint(1) + varint(1) + op(2, 1) + b"a",
      D, "corrupt", "window declares a 1 TiB target view (allocation bomb)")
write("svndiff", "huge-new-data", V0 + varint(0) + varint(0) + varint(4) + varint(1) + varint(1 << 40) + op(2, 4),
      D, "corrupt", "window declares 1 TiB of new data")
write("svndiff", "sview-overflow", V0 + window((1 << 64) - 2, 16, 1, op(0, 1, 0), b""), D, "corrupt",
      "source view offset + length overflows u64")
write("svndiff", "new-data-beyond", V0 + window(0, 0, 8, op(2, 8), b"abc"), D, "corrupt",
      "new-data instruction reads past the new-data section")
write("svndiff", "source-copy-beyond", V0 + window(0, 4, 8, op(0, 8, 0), b""), D, "corrupt",
      "source copy reads past the source view")
write("svndiff", "target-copy-forward", V0 + window(0, 0, 4, op(1, 4, 2), b""), D, "corrupt",
      "target copy from an offset that is not yet written")
write("svndiff", "short-target", V0 + window(0, 0, 10, op(2, 3), b"abc"), D, "corrupt",
      "instructions produce fewer bytes than the target view")
write("svndiff", "long-target", V0 + window(0, 0, 2, op(2, 3), b"abc"), D, "corrupt",
      "instructions produce more bytes than the target view")
write("svndiff", "bad-opcode", V0 + window(0, 0, 1, bytes([0xC1]), b"a"), D, "corrupt",
      "instruction with the reserved opcode 3")
zbomb = zlib.compress(b"\x00" * (1 << 20), 9)
write("svndiff", "zlib-length-lie",
      V1 + varint(0) + varint(0) + varint(5) + varint(2) + varint(len(zbomb) + len(varint(5)))
      + varint(1) + op(2, 5) + varint(5) + zbomb, D, "corrupt",
      "svndiff1 new-data claims 5 bytes but inflates to 1 MiB (zip bomb)")
write("svndiff", "zlib-garbage", V1 + varint(0) + varint(0) + varint(1) + varint(2) + varint(4) + varint(1) + op(2, 1) + varint(1) + b"\xde\xad\xbe",
      D, "corrupt", "svndiff1 compressed section that is not zlib")
write("svndiff", "lz4-garbage", b"SVN\x02" + varint(0) + varint(0) + varint(1) + varint(2) + varint(4) + varint(1) + op(2, 1) + varint(9) + b"\xff\xff\xff",
      D, "corrupt", "svndiff2 compressed section that is not lz4")
write("svndiff", "source-past-base", V0 + window(100, 4, 4, op(0, 4, 0), b""), A, "corrupt",
      "window references source bytes beyond the end of the base text (apply-time)")

# ------------------------------------------------------------------------------------ index ---
with open(os.path.join(HERE, "cases.tsv"), "w", encoding="utf-8") as f:
    f.write("# id\tparser\tfile\texpect\tdescription  (generated by generate.py)\n")
    for row in CASES:
        f.write("\t".join(row) + "\n")
print(f"{len(CASES)} cases")

import { expect, test } from "vitest";
import { describeLayout, folderLines, layoutProblem, normalizeFolder } from "./layout-model";
import {
  externalsProblem,
  isBinaryMimeType,
  knownPropsFor,
  mergeinfoProblem,
  mimeTypeProblem,
  propProblem,
  propRows,
} from "./props-model";

test("names: svn's rule for property names, unknown svn: names, where a property applies", () => {
  expect(propProblem("", "x", "file")?.key).toBe("propNameEmpty");
  expect(propProblem("1abc", "x", "file")?.key).toBe("propNameInvalid");
  expect(propProblem("has space", "x", "file")?.key).toBe("propNameInvalid");
  expect(propProblem("my:prop-1.x_y", "anything", "file")).toBeNull();
  expect(propProblem("svn:foo", "x", "file")).toEqual({ key: "propUnknownSvn", name: "svn:foo" });
  expect(propProblem("svn:eol-style", "LF", "dir")?.key).toBe("propFileOnly");
  expect(propProblem("svn:needs-lock", "*", "dir")?.key).toBe("propFileOnly");
  expect(propProblem("svn:ignore", "*.o", "file")?.key).toBe("propDirOnly");
  expect(propProblem("svn:externals", "^/a b", "symlink")?.key).toBe("propDirOnly");
  expect(propProblem("svn:mergeinfo", "/trunk:1-2", "file")).toBeNull();
  expect(propProblem("svn:mergeinfo", "/trunk:1-2", "dir")).toBeNull();
  expect(knownPropsFor("dir")).toContain("svn:externals");
  expect(knownPropsFor("file")).toContain("svn:needs-lock");
  expect(knownPropsFor("file")).not.toContain("svn:special");
});

test("svn:eol-style: the four styles, and never on a binary file", () => {
  for (const v of ["native", "LF", "CRLF", "CR", " LF "])
    expect(propProblem("svn:eol-style", v, "file")).toBeNull();
  for (const v of ["lf", "unix", ""])
    expect(propProblem("svn:eol-style", v, "file")?.key).toBe("propEolInvalid");
  const binary = [{ name: "svn:mime-type", value: "application/octet-stream", binary: false }];
  expect(propProblem("svn:eol-style", "LF", "file", binary)?.key).toBe("propEolBinary");
  const text = [{ name: "svn:mime-type", value: "text/x-c", binary: false }];
  expect(propProblem("svn:eol-style", "LF", "file", text)).toBeNull();
});

test("svn:mime-type: svn_mime_type_validate", () => {
  expect(mimeTypeProblem("text/plain")).toBeNull();
  expect(mimeTypeProblem("image/svg+xml; charset=utf-8")).toBeNull();
  expect(mimeTypeProblem("")).not.toBeNull();
  expect(mimeTypeProblem("textplain")).not.toBeNull();
  expect(mimeTypeProblem("text/pl(ain")).not.toBeNull();
  expect(mimeTypeProblem("text/plé")).not.toBeNull();
  expect(mimeTypeProblem("text/plain;\u0001")).not.toBeNull();
  expect(propProblem("svn:mime-type", "nope", "file")?.key).toBe("propMimeInvalid");
  expect(isBinaryMimeType("text/html")).toBe(false);
  expect(isBinaryMimeType("image/x-xpixmap")).toBe(false);
  expect(isBinaryMimeType("image/png")).toBe(true);
});

test("svn:externals: the shapes svn's parser accepts and refuses", () => {
  expect(
    externalsProblem("^/lib lib\n# comment\n\nhttp://h.example.com/r/x@12 third/x"),
  ).toBeNull();
  expect(externalsProblem("-r 5 ^/lib lib")).toBeNull();
  expect(externalsProblem("-r5 ^/lib lib")).toBeNull();
  expect(externalsProblem("lib -r 5 http://h.example.com/r/lib")).toBeNull();
  expect(externalsProblem('"my lib" http://h.example.com/r/lib')).toBeNull();
  expect(externalsProblem("onlyone")).toBe("onlyone");
  expect(externalsProblem("a b c d e")).toBe("a b c d e");
  expect(externalsProblem("http://h.example.com/a http://h.example.com/b")).not.toBeNull();
  expect(externalsProblem("^/lib ../up")).toBe("^/lib ../up");
  expect(externalsProblem("^/lib /abs")).toBe("^/lib /abs");
  expect(externalsProblem("-r HEAD ^/lib lib")).not.toBeNull();
  expect(externalsProblem("^/a x\n^/b x")).toBe("^/b x");
  expect(propProblem("svn:externals", "a b c d e", "dir")).toEqual({
    key: "propExternalsInvalid",
    line: "a b c d e",
  });
});

test("svn:mergeinfo: paths from the root, increasing ranges from r1", () => {
  expect(mergeinfoProblem("/trunk:1-3,5*\n/branches/x:7")).toBeNull();
  expect(mergeinfoProblem("trunk:1")).toBe("trunk:1");
  expect(mergeinfoProblem("/trunk:3-1")).toBe("/trunk:3-1");
  expect(mergeinfoProblem("/trunk:0")).toBe("/trunk:0");
  expect(mergeinfoProblem("/trunk")).toBe("/trunk");
  expect(mergeinfoProblem("/trunk:")).toBe("/trunk:");
});

test("rows: one value when the items agree, none when they differ, and how many have it", () => {
  const rows = propRows([
    {
      path: "a",
      kind: "file",
      props: [
        { name: "svn:needs-lock", value: "*", binary: false },
        { name: "z", value: "1", binary: false },
      ],
    },
    {
      path: "b",
      kind: "file",
      props: [
        { name: "svn:needs-lock", value: "*", binary: false },
        { name: "z", value: "2", binary: false },
        { name: "blob", value: "�", binary: true },
      ],
    },
  ]);
  expect(rows).toEqual([
    { name: "blob", value: "�", count: 1, binary: true },
    { name: "svn:needs-lock", value: "*", count: 2, binary: false },
    { name: "z", value: null, count: 2, binary: false },
  ]);
});

test("layout: folders are normalized and checked like layout_set does", () => {
  expect(normalizeFolder("trunk/")).toBe("/trunk");
  expect(folderLines(" branches \n\n/{project}/branches\n")).toEqual([
    "/branches",
    "/{project}/branches",
  ]);
  const custom = (trunk: string, branches: string[] = [], tags: string[] = []) =>
    ({ mode: "custom", trunk, branches, tags }) as const;
  expect(layoutProblem({ mode: "auto" })).toBeNull();
  expect(layoutProblem(custom("/trunk", ["/branches"], ["/tags"]))).toBeNull();
  expect(layoutProblem(custom("/"))).toBe("layoutTrunkRequired");
  expect(layoutProblem(custom("/a/../b"))).toBe("layoutDots");
  expect(
    layoutProblem(
      custom(
        "/t",
        Array.from({ length: 21 }, (_, i) => `/b${i}`),
      ),
    ),
  ).toBe("layoutTooMany");
  expect(describeLayout(custom("/trunk", ["/branches"], ["/tags"]))).toBe(
    "/trunk, /branches, /tags",
  );
});

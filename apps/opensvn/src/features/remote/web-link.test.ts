import { expect, test } from "vitest";
import { hasQueryOrFragment, isVisualSvnWebLink } from "./web-link";

test("recognises the VisualSVN web-interface forms", () => {
  for (const s of [
    "https://svn.example.com/!/#Repo",
    "https://svn.example.com/!/#Repo/view/head/trunk/src",
    "https://svn.example.com/!/#Repo/view/r1234/branches/x",
    "https://svn.example.com:8443/!/#Repo",
    "https://svn.example.com/vsvn/!/#Repo",
    "http://svn.example.com/!#Repo",
    "  https://svn.example.com/!/#Dự án  ",
    "https://svn.example.com/!/#",
  ]) {
    expect(isVisualSvnWebLink(s), s).toBe(true);
  }
});

test("leaves other URLs alone", () => {
  for (const s of [
    "https://svn.example.com/svn/Repo",
    "https://svn.example.com/svn/Repo#frag",
    "https://svn.example.com/docs/#section",
    "https://svn.example.com/a!/#Repo",
    "https://svn.example.com/!/",
    "svn://svn.example.com/!/#Repo",
    "not a url",
    "",
  ]) {
    expect(isVisualSvnWebLink(s), s).toBe(false);
  }
});

test("query and fragment detection is for http(s) URLs only", () => {
  expect(hasQueryOrFragment("https://svn.example.com/svn/Repo?p=1")).toBe(true);
  expect(hasQueryOrFragment("https://svn.example.com/svn/Repo#x")).toBe(true);
  expect(hasQueryOrFragment("https://svn.example.com/svn/Repo")).toBe(false);
  expect(hasQueryOrFragment("file:///tmp/a#b")).toBe(false);
});

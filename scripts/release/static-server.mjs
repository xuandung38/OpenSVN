#!/usr/bin/env node
import { readFile, writeFile } from "node:fs/promises";
// Tiny static file server for updater tests: serves a directory on 127.0.0.1, optionally
// demanding "Authorization: Bearer <token>" like a private GitHub repo would.
//   static-server.mjs --dir DIR --port-file FILE [--token TOKEN]
import { createServer } from "node:http";
import { join, normalize, sep } from "node:path";

const args = process.argv.slice(2);
const get = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const dir = get("--dir");
const portFile = get("--port-file");
const token = get("--token");
if (!dir || !portFile) {
  console.error("usage: static-server.mjs --dir DIR --port-file FILE [--token TOKEN]");
  process.exit(2);
}

const server = createServer(async (req, res) => {
  if (token && req.headers.authorization !== `Bearer ${token}`) {
    res.writeHead(401).end("unauthorized");
    return;
  }
  const path = normalize(join(dir, decodeURIComponent(new URL(req.url, "http://x").pathname)));
  if (path !== dir && !path.startsWith(dir + sep)) {
    res.writeHead(403).end("forbidden");
    return;
  }
  try {
    const body = await readFile(path);
    res.writeHead(200, { "content-type": "application/octet-stream" }).end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});
server.listen(0, "127.0.0.1", async () => {
  await writeFile(portFile, String(server.address().port));
});

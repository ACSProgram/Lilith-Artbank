import { createReadStream, statSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(fileURLToPath(new URL(".", import.meta.url)));
const port = Number(process.env.PORT || 18767);
const types = {
  ".html": "text/html; charset=utf-8",
  ".jpg": "image/jpeg",
  ".webp": "image/webp",
};

const server = createServer((request, response) => {
  const url = new URL(request.url || "/", `http://127.0.0.1:${port}`);
  const relative = normalize(decodeURIComponent(url.pathname)).replace(/^([/\\])+/, "");
  const target = join(root, relative || "index.html");
  if (!target.startsWith(root)) {
    response.writeHead(403).end("forbidden");
    return;
  }
  let stats;
  try {
    stats = statSync(target);
  } catch {
    response.writeHead(404).end("not found");
    return;
  }
  if (!stats.isFile()) {
    response.writeHead(404).end("not found");
    return;
  }
  const extension = extname(target).toLowerCase();
  response.writeHead(200, {
    "Content-Type": types[extension] || "application/octet-stream",
    "Content-Length": stats.size,
    "Cache-Control": extension === ".html" ? "no-store" : "public, max-age=31536000, immutable",
  });
  createReadStream(target).pipe(response);
});

server.listen(port, "127.0.0.1", () => {
  console.log(`thumb bench: http://127.0.0.1:${port}/?format=jpg`);
});

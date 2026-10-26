// A tiny stand-in for the vertical slice's server. Ticket 09 replaces it with a
// real app: a Postgres-backed page, a migration run through `exec`, and a
// /health endpoint the daemon can probe.
import { createServer } from "node:http";

const port = Number(process.env.PORT ?? 3000);

createServer((request, response) => {
  if (request.url === "/health") {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ status: "ok" }));
    return;
  }
  response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
  response.end(`<!doctype html><title>hello-pg</title><p>${process.env.GREETING ?? "hello"}</p>`);
}).listen(port, "0.0.0.0", () => {
  console.log(`hello-pg listening on ${port}`);
});

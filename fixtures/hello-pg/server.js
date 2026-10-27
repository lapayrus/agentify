// The vertical slice's app: a page read from Postgres, and a health endpoint
// that answers without the database.
//
// The database is allowed to be slow: the app starts before Postgres is ready on
// a cold boot, and compose can restart either container independently, so every
// read retries with a bounded backoff instead of failing the first request.
import { createServer } from "node:http";
import pg from "pg";

const port = Number(process.env.PORT ?? 3000);
const pool = new pg.Pool({
  connectionString: process.env.DATABASE_URL,
  max: 5,
  connectionTimeoutMillis: 2_000,
  // A page must not sit on a dead socket for the linger time of the default.
  idleTimeoutMillis: 10_000,
});

const RETRY_ATTEMPTS = 20;
const RETRY_DELAY_MS = 500;

async function query(text) {
  let last;
  for (let attempt = 0; attempt < RETRY_ATTEMPTS; attempt += 1) {
    try {
      return await pool.query(text);
    } catch (error) {
      last = error;
      await new Promise((resolve) => setTimeout(resolve, RETRY_DELAY_MS));
    }
  }
  throw last;
}

function page(rows) {
  const items = rows
    .map((row) => `    <li data-id="${row.id}">${escape(row.message)}</li>`)
    .join("\n");
  return [
    "<!doctype html>",
    '<html lang="en">',
    "  <head><meta charset=\"utf-8\"><title>hello-pg</title></head>",
    "  <body>",
    "    <h1>hello-pg</h1>",
    "    <p id=\"source\">rows read from Postgres through a generated password</p>",
    "    <ul id=\"greetings\">",
    items,
    "    </ul>",
    "  </body>",
    "</html>",
  ].join("\n");
}

function escape(text) {
  return String(text)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

const server = createServer((request, response) => {
  if (request.url === "/health") {
    // Deliberately does not touch the database: this is the liveness of the
    // process, which is what an orchestrator should restart on.
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ status: "ok" }));
    return;
  }

  query("select id, message from greetings order by id")
    .then((result) => {
      response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      response.end(page(result.rows));
    })
    .catch((error) => {
      // No connection string, no password: the message names the failure only.
      response.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
      response.end(`the database did not answer: ${error.message}\n`);
    });
});

server.listen(port, "0.0.0.0", () => {
  console.log(`hello-pg listening on ${port}`);
});

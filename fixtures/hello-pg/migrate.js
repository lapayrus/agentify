// The migration an Agent runs through `exec`:
//
//     agentify exec hello-pg web -- node migrate.js
//
// It creates the table and inserts one row, and running it twice changes
// nothing: a migration that is not idempotent is a migration an Agent cannot
// safely retry.
import pg from "pg";

const MESSAGE = process.env.GREETING ?? "hello from postgres";
const RETRY_ATTEMPTS = 60;
const RETRY_DELAY_MS = 1_000;

async function connect() {
  let last;
  for (let attempt = 0; attempt < RETRY_ATTEMPTS; attempt += 1) {
    const client = new pg.Client({
      connectionString: process.env.DATABASE_URL,
      connectionTimeoutMillis: 2_000,
    });
    try {
      await client.connect();
      return client;
    } catch (error) {
      last = error;
      await client.end().catch(() => {});
      await new Promise((resolve) => setTimeout(resolve, RETRY_DELAY_MS));
    }
  }
  throw last;
}

async function main() {
  const client = await connect();
  try {
    await client.query(
      "create table if not exists greetings (id serial primary key, message text not null)"
    );
    await client.query(
      "insert into greetings (message) select $1 where not exists (select 1 from greetings where message = $1)",
      [MESSAGE]
    );
    const { rows } = await client.query(
      "select id, message from greetings order by id"
    );
    for (const row of rows) {
      console.log(`${row.id}: ${row.message}`);
    }
    console.log(`migrate: ${rows.length} row(s) in greetings`);
  } finally {
    await client.end().catch(() => {});
  }
}

main().catch((error) => {
  console.error(`migrate failed: ${error.message}`);
  process.exit(1);
});

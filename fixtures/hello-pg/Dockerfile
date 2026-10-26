# The hello-pg fixture's build context. Ticket 09 makes this a real image
# (multi-stage build, migrations, and a health endpoint that queries Postgres);
# ticket 02 only needs the context to exist so a `build` Resource has a source.
FROM node:22-alpine

WORKDIR /app
COPY server.js ./server.js

EXPOSE 3000
CMD ["node", "server.js"]

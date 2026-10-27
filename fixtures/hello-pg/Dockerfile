# The hello-pg build context: Node 22, the `pg` client, the server, and the
# migration an Agent runs through `exec`.
FROM node:22-alpine

WORKDIR /app

# Dependencies first, so a source change does not re-run the install.
COPY package.json ./package.json
RUN npm install --omit=dev

COPY server.js migrate.js ./

EXPOSE 3000

# The Resource's `command` from the Manifest replaces this, and is kept here so
# the image is runnable on its own.
CMD ["node", "server.js"]

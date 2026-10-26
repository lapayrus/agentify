# Drop Team and Server from the domain model

Coolify models Team → Project → Environment → Resource, with Server and Destination (a Docker network on a server) as deployment targets. This product is single-user and single-machine, so we keep Project → Environment → Resource, remove Team and Server, and replace Destination with Backend, a runtime handle. Keeping the remote-server aggregates would import SSH, provisioning, and multi-tenancy concerns that do not exist locally.

Consequences: no `team_id` scoping, no SSH transport, no server capability checks; the Backend contract carries whatever platform differences remain.

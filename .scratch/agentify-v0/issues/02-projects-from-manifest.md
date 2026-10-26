# 02: Projects from a Manifest

**What to build:** Projects can be created from the bundled `hello-pg` fixture or an inline Manifest and inspected through the API, MCP, and UI. Nothing starts yet. The strict parser rejects unknown fields, bad references, duplicate routes, and dependency cycles, producing typed errors.

**Blocked by:** 01 Walking skeleton.

**Status:** ready-for-agent

- [ ] `create_project` accepts exactly one of the fixture or an inline Manifest; `list_projects` and `get_project` return the Project with its one `dev` Environment and Resources.
- [ ] The parser rejects unknown fields at every level, missing required keys, app Resources without exactly one of image or build, undefined `${secret:name}` references, duplicate hostnames, and dependency cycles.
- [ ] A duplicate Project name returns `AlreadyExists`; a build context without a source returns `SourceMissing`; malformed input returns `ValidationFailed` with a path.
- [ ] The UI rail lists the Project with state and cost placeholders; the detail pane shows Resources, the generated URL, and the Environment badge.
- [ ] MCP `create_project`, `list_projects`, and `get_project` work against the same API.
- [ ] The test suite covers the vertical-slice Manifest and each failure case above.

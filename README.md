# overleaf-mcp

MCP server that lets agents work with an Overleaf Community Edition (ShareLaTeX) instance: create projects,
read, write and edit files, compile to PDF and read the LaTeX errors, and share projects. It is one static
binary (Rust, built on [rmcp](https://github.com/modelcontextprotocol/rust-sdk), the official MCP Rust SDK)
serving MCP's streamable HTTP transport, optionally over HTTPS.

Overleaf CE has no public API. The server logs in as one ordinary Overleaf user and calls the same web
endpoints as the browser editor, so Overleaf's permissions apply: the agents see that user's projects and
projects shared with it. Tested against Overleaf CE 6.3.1.

This is an independent project, not affiliated with or endorsed by Overleaf.

## Tools

| Tool | What it does |
|---|---|
| `list_projects` | projects the user owns or can access (`search`, `include_archived`) |
| `create_project` | new project with a minimal `main.tex` (or Overleaf's example project) |
| `list_files` | file tree with types (`doc` = editable text, `file` = binary) and the main document |
| `read_file` | text files as text, images as images |
| `write_file` | create or overwrite a file; creates folders; `base64` for binary files |
| `edit_file` | replace an exact string (once, or `replace_all`) |
| `delete_file` | delete a file or folder |
| `set_main_file` | choose the document compiled by default |
| `compile` | compile; returns status, errors with source lines, warnings, bad-box count, log tail |
| `get_compile_output` | `output.pdf` (default) or another output file of the last compile; text files (`output.log`, `output.blg`, ...) come with line numbers and can be filtered by regex `pattern` with `context` lines and paged with `offset`/`limit` (negative offset = tail; at most 400 lines unless `limit` is raised) |
| `invite_collaborator` | invite a person by e-mail (edit or read-only) |
| `accept_invites` | accept projects that people shared with the agent user |
| `transfer_ownership` | make a collaborator (who has accepted an invite) the owner; the agent user stays a collaborator |

Writes go through Overleaf's upload endpoint, which updates an existing document in place (same id, history
kept); people who have the project open see the change immediately.

## Configuration

| Variable | Meaning |
|---|---|
| `OVERLEAF_URL` | Overleaf base URL (default `http://sharelatex`, the compose service name) |
| `OVERLEAF_EMAIL` | the Overleaf account the server acts as |
| `OVERLEAF_PASSWORD` or `OVERLEAF_PASSWORD_FILE` | its password |
| `MCP_TOKENS` or `MCP_TOKENS_FILE` | bearer tokens accepted from clients, comma- or line-separated, `#` comments |
| `MCP_BIND` | listen address (default `0.0.0.0:8080`) |
| `MCP_ALLOWED_HOSTS` | accepted `Host` headers, comma-separated (default loopback only; `*` = any) |
| `MCP_TLS_CERT`, `MCP_TLS_KEY` | PEM files; when both are set the server speaks HTTPS and reloads them every 12 h. Only for running without a reverse proxy; leave unset behind one |
| `RUST_LOG` | log filter (default `info,rmcp=warn`) |

The MCP endpoint is `/mcp`; every request needs `Authorization: Bearer <token>`. `/healthz` is unauthenticated.

## Build

```sh
cargo build --release              # local binary
docker build -t overleaf-mcp:0.1.0 .   # static musl binary in a FROM scratch image (~10 MB)
```

## Client setup (Claude Code)

```sh
claude mcp add --transport http overleaf https://overleaf.example.com/mcp \
  --header "Authorization: Bearer <token>"
```

## Agent skill

`skills/overleaf/SKILL.md` tells agents how to use the tools well (finding projects, the compile-and-fix loop,
log filtering, shell-escape limits, sharing and handing over). Install it for Claude Code with
`ln -s "$PWD/skills/overleaf" ~/.claude/skills/overleaf`, or copy the folder into a project's `.claude/skills/`.

## Deployment next to Overleaf

Run it as another service of the Overleaf compose project, so it reaches Overleaf at `http://sharelatex`
over the compose network, and publish it on loopback behind the reverse proxy that already serves Overleaf
over HTTPS. The proxy terminates TLS with its own certificate and forwards `/mcp` over plain HTTP, so leave
`MCP_TLS_CERT`/`MCP_TLS_KEY` unset there. `deploy/` has the compose service, the Apache lines and the install
steps.

Without a reverse proxy, set `MCP_TLS_CERT` and `MCP_TLS_KEY` and publish the port directly. The files must be
readable by the container user (uid/gid 65534); renewed certificates are picked up within 12 hours.

Create a dedicated Overleaf user for the agents; do not use a person's account. People give the agents
access to a project by sharing it with that user's e-mail address and asking an agent to run
`accept_invites`.

## Testing

`test/docker-compose.yml` starts a throwaway Overleaf CE 6.3.1 (with mongo and redis) on
`127.0.0.1:18080` and the MCP container on `https://localhost:18444` (it expects a test certificate,
password and token in `test/secrets/`, which is git-ignored, and `OVERLEAF_INVITE_TOKEN_SECRET` in
`test/.env`). `cargo test` runs the unit tests.

## Limits

- One Overleaf identity for all clients; give each client its own token so one can be revoked.
- Edits are whole-file uploads, not real-time operational transforms: if a person edits the same file at the
  same moment, the later write wins.
- The file tree is read over Overleaf's real-time socket (socket.io 0.9 long-polling), so `/socket.io` must
  be reachable at `OVERLEAF_URL`.
- No track changes or comments: Overleaf CE has the storage for them (the chat service keeps comment threads,
  documents keep tracked-change ranges) but not the web routes, which come with Server Pro's review module
  (on CE 6.3.1 `/project/:id/threads`, `/ranges` and `/track_changes` answer 404). Without those routes the
  editor cannot load comment threads, so comments added some other way would not reach the people editing.

## License

Copyright 2026 Wojciech Gradkowski. Licensed under the Apache License, Version 2.0; see [LICENSE](LICENSE).

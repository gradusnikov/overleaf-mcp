---
name: overleaf
description: "Work on LaTeX documents in Overleaf (ShareLaTeX) through the overleaf MCP server (mcp__overleaf__* tools): create projects, read and edit .tex/.bib files, compile to PDF, read LaTeX errors from the log, share projects with people and hand them over. Use when the user asks to write, fix, build or share a document on Overleaf/ShareLaTeX, or mentions an Overleaf project."
---

# Overleaf via the overleaf MCP server

The `mcp__overleaf__*` tools drive an Overleaf Community Edition server as one dedicated Overleaf user (the
"agent user"). Overleaf's own permissions apply: you see only projects that user owns or that were shared
with it. Load the tool schemas with ToolSearch (`select:mcp__overleaf__list_projects,...`) if they are deferred.

## Finding the project

- `list_projects` (`search` = part of the name) returns `id`, `name`, `owner` and `access`
  (`owner`, `readWrite`, `readOnly`, ...). Every other tool takes that 24-hex-digit `project_id`.
- A project the user mentions but that is not listed has not been shared with the agent user yet. Ask the
  user to share it (Share menu, the agent user's e-mail, "Can edit"), then call `accept_invites`. Do not
  look for other ways in.
- `create_project` makes a new project with a minimal `main.tex` (`example: true` for Overleaf's example
  project). The agent user owns it; nobody else can see it until you invite them (see Sharing).

## Files

- `list_files` gives the tree with types: `doc` = editable text with history (.tex, .bib, .cls, .sty, ...),
  `file` = everything else, including text formats Overleaf does not treat as documents (.dot, .puml).
  It also names the main document. Files (`file`) carry `hash`, the SHA-1 `git hash-object` gives for their
  content, so a local copy can skip unchanged ones.
- `read_file`: text as text, images as images, other binaries as a blob.
- `download_project`: the whole project as one zip; use it to copy a project rather than reading every file.
- `edit_file` replaces an exact string; `old_string` must occur exactly once unless `replace_all`. Read the
  file first and copy the text exactly, including whitespace. Prefer it over `write_file` for changes to
  existing files: it keeps the diff small and fails loudly when the text moved.
- `write_file` creates or overwrites a whole file and creates missing folders (`figures/plot.png`). For
  binary files pass base64 and `base64: true`.
- `delete_file` removes a file or a folder with its content. `set_main_file` picks the document compiled by
  default.
- Writes are uploads, not live collaborative edits: people who have the project open see the change at
  once, but if someone edits the same file at the same moment the later write wins. When the user says they
  are editing, re-read the file right before each `edit_file`.

## Compile and fix loop

1. `compile` (optional `main_file`, `draft`, `stop_on_first_error`). It returns `status` (`success`,
   `failure`, `error`, `timedout`, ...), `errors` (with the `l.N` source line), `warnings` (first 30),
   `bad_boxes` and a `log_tail`. Rapid recompiles are throttled by Overleaf; the tool retries for you.
2. Error entries hold the first line of each TeX message. For the lines TeX prints after it (the input
   context, `<argument>`, help text), query the log: `get_compile_output` with `file: "output.log"`,
   `pattern: "^!"` and `context: 8`. Other useful patterns: `"Warning"`, `"Undefined"`,
   `"\\.tex:\\d+:"`, `"(?i)overfull|underfull"`; `offset: -60` gives the last 60 lines.
3. Fix with `edit_file`, compile again, until `status` is `success` and there are no errors. Then look at
   the warnings: undefined references and citations usually need one more compile (or BibTeX ran with
   errors: check `output.blg`).
4. `get_compile_output` with no `file` returns the PDF. Fetch it only when you need to inspect or hand it
   over; the file list of the last compile is in `output_files`.

`get_compile_output` reads the last compile done through this server; after a server restart, compile
again first. Text output is paged (400 lines by default, `limit` up to 5000), so filter instead of
dumping whole logs.

## Shell escape, diagrams, TeX version

Compiles run with restricted `\write18`: only commands on the server's allow list run. Packages that need
unrestricted shell escape (`minted`, `graphviz.sty`'s `\digraph`, `svg` with Inkscape, `--shell-escape`
tricks) fail. Servers whose TeX image adds Graphviz and PlantUML to the allow list can generate figures
like this:

```latex
\immediate\write18{dot -Tpdf -o graph.pdf graph.dot}    % Graphviz
\includegraphics{graph.pdf}
\immediate\write18{plantuml -tpng seq.puml}             % PlantUML: use PNG; pdfTeX cannot read its PDF output
\includegraphics[width=8cm]{seq.png}
```

When a generated figure is missing, check the log for `runsystem(...)`: `executed safely (allowed)` means
the command ran, `disabled (restricted)` means it is not on the allow list. The first log line names the
TeX Live version; an old one lacks newer packages and fixes, so when a package is missing prefer an older
equivalent or ship the `.sty` in the project rather than asking for a server change.

## Sharing and handing over

- `invite_collaborator` (`email`, `read_only`) e-mails an invitation; the person must already have an
  account on that Overleaf server (Community Edition has no self-registration, an admin creates accounts).
- `transfer_ownership` (`email`) makes a collaborator the owner of a project the agent user owns. It works
  only after they accepted the invitation; the agent user stays an editor. It cannot be undone by you, so
  do it only when the user asked for it. Order: `invite_collaborator`, wait for the user to confirm they
  accepted, then `transfer_ownership`.
- Prefer the reverse when the user starts the work: they create the project and share it with the agent
  user, so it stays theirs from the start.

## Not available

No deleting, renaming or archiving whole projects, no renaming or moving files (writing a copy and deleting
the original loses the file's history), no comments or track changes (Community Edition does not
serve them), no history browsing or restore, no project-wide search (read the files), no admin functions.
Say so instead of improvising; the user can do these in the Overleaf web editor.

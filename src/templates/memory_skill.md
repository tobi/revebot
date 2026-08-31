---
name: memory
description: >
  Remember or forget enduring facts, dated history and short-lived notes with
  update_state. Agent-local by default; share user/project memory explicitly.
---

# Memory

Your home is `/workspace/agents/<your-id>/`. Your `SOUL.md` is your identity and
standing remit; `profile.json` is your metadata. Memory is not another persona.
Do not assume another agent's work is your assignment.

Use the Rust house tool `update_state`:

```json
{"target":"memory","action":"write","fact":"The user prefers concise answers.","tier":"profile"}
```

- `action`: `write` or `forget`; required.
- `fact`: exact nonblank text, at most 4096 bytes; required. Whitespace is meaningful.
- `tier` (write only): `profile` (enduring), `log` (default, recent 30 days),
  `note` (recent 48 hours). Expired facts stay on disk; they just leave the prompt.
- `scope`: `agent` (default, only your context), `user` (explicitly shared by all
  agents), or `project` (explicitly associated project).
- `project`: required only for project scope. It must already be in your
  `profile.json` `projects` list. Use a single name, not a path.

```json
{"target":"memory","action":"forget","fact":"The exact fact as recorded"}
```

Forget matches exact recorded text in the selected scope across all tiers. An
unknown fact is a no-op. Corrections are exact forget followed by write. Repeating
a write does not create duplicates or refresh its timestamp; forget/rewrite if a
short-lived fact needs renewal.

## Files and reading

There is no read-memory tool. Use `read`, `ls`, `grep` or the VM shell:

- Agent: `$HOME/memory/profile.md`, `log/YYYY-MM.md`, `notes/YYYY-MM.md`.
- Shared user: `/workspace/memory/user/` with the same tier layout.
- Project: `/workspace/projects/<project>/memory/agents/<writer-id>/`.
  Only agents explicitly associated with that project get its shards in context.

API facts are readable Markdown blocks with timestamp/tier comment delimiters.
Keep those delimiters intact; editing the text changes the exact fact to forget.
Ordinary prose outside managed blocks is preserved. Unmanaged `profile.md` prose
is included in enduring context; unmanaged log/notes prose is read on demand.

Prompts contain bounded private memory, then explicitly shared user/project memory.
They indicate truncation and older facts on disk. Prompt projection considers at
most four associated projects and fifty writer shards per project. No sibling private memory or
legacy global `KNOWLEDGE.md` is automatically loaded. Private means context
separation, not filesystem secrecy: all bots still share one VM.

Writes are serialized, compare existing bytes before atomic guest replacement,
and acknowledge only successful publication. A conflict asks you to retry rather
than overwrite a changed file. A file is limited to 64 KiB and an active memory
scope to 256 files: archive older files outside the injected tier directories.
Do not store secrets in memory. Use `AskUserForSecret`.

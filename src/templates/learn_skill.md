---
name: learn
description: >
  Turn a workflow, URL, directory or conversation into a reusable skill via
  skill_manage. Use when the user runs /learn or asks to save a skill.
---

# Learn a skill

Turn something you can already see — this conversation, a directory, a URL, or
pasted notes — into a class-level skill. There is no separate ingestion engine.
Gather with `read` / `ls` / `grep` / the VM shell (and web_fetch if present),
then save with the house tool `skill_manage`.

## 1. Gather

- A directory or file: `ls` / `read` / `grep`.
- A URL: fetch it (do not invent APIs).
- "What we just did": use this conversation. Do not ask them to repeat it.
- Pasted notes: treat the user text as the source.

## 2. Shape

Class-level, not one-session. `deploy-k8s` not `fix-pr-1421`. Prefer patching
an existing umbrella (`skills_list` then `skill_view`, then `skill_manage`
patch) over creating a near-duplicate.

Small source → one tight `SKILL.md`. Large source (book, spec, doc corpus) →
lean `SKILL.md` index plus `references/<topic>.md` via
`skill_manage action=write_file`. Do not cram a book into one file. Do not
reproduce long passages.

## 3. SKILL.md

```
---
name: lowercase-hyphen-name
description: One sentence of what it does and when to use it.
---

# Title

What it does, what it does not, key dependency.

## When to Use
- trigger phrases

## Procedure
1. numbered steps with copy-paste commands (guest paths under /workspace)

## Pitfalls
- known failure modes

## Verification
How to confirm it worked.
```

`name` must match the `skill_manage` `name`. Body is required. Frame commands
as tools the bot already has (`bash`, `read`, `write`), never invented host
CLIs. Do not put secrets in the skill.

## 4. Save

```json
{"action":"create","name":"deploy-k8s","scope":"house","content":"---\nname: deploy-k8s\n..."}
```

House: `/workspace/skills/<name>/SKILL.md` (`scope=house`, default). This bot:
`scope=bot`. Support files: `action=write_file`, `file_path` starting
`references/`, `templates/`, or `scripts/`.

If a matching skill exists, patch it. Bundled skills are off-limits. Creates
are curator-managed (stale/archive later). Pin anything load-bearing:
`revebot curator pin <name>` (host; you cannot run it from the VM).

Show them the name, slash (`/<name>`), and house vs bot path.

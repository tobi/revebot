---
name: curator
description: >
  Maintain the house skill library: usage, pin, adopt, stale, archive, restore.
  Use when skills pile up, something went missing, or the user runs /curator.
---

# Curator

The curator is host maintenance for **agent-created skills**. It never deletes.
The worst outcome is a move into `skills/.archive/`, which `revebot curator restore`
reverses.

It does **not** run inside the microVM. You cannot shell out to it. Tell the user
the `revebot curator …` command, or do the markdown edits yourself with `read` /
`write` / `edit`.

## What it touches

- **Managed** — `created_by: agent` in `.reve/curator/usage.json` (after `adopt`, or a `skill_manage` create).
- **Bundled** — `create-skill`, `vm`, `routines`, `plugins`, `memory`, `secrets`,
  `browser`, `computer`, `curator`. Off-limits unless `curator.prune_builtins: true`.
- **Pinned** — skipped by every auto-transition. Pin anything the house relies on.
- **Unmanaged** — every other live skill. Invisible to auto-stale/archive until
  adopted. Foreground `/create-skill` work is unmanaged on purpose.

Lifecycle for managed skills: `active` → (30d unused) `stale` → (90d unused)
`archived`. Never-used skills are not archived until they are at least 30 days
old. Use, view, or patch resets the clock.

## Host commands (for the user)

```
revebot curator status
revebot curator run [--dry-run]
revebot curator pause | resume
revebot curator pin <name> | unpin <name>
revebot curator adopt <name> [<name> ...]
revebot curator adopt --all-unmanaged [--dry-run]
revebot curator list-unmanaged
revebot curator archive <name> | restore <name> | list-archived
revebot curator backup [--reason …]
revebot curator rollback [--list] [--id <stamp>]
```

`run` is prune-only (no model). First observation seeds the interval clock and
defers a full interval (default 7 days). `run --dry-run` previews without
mutating and without pushing that clock.

## Consolidation (you, not the host)

When asked to tidy the library, this is an **umbrella-building** pass, not a
duplicate-finder. Hundreds of one-session micro-skills are a failure of the
library. One class-level skill with `references/`, `templates/`, `scripts/`
beats five narrow siblings.

Hard rules:

1. Do not touch bundled, pinned, or unmanaged skills. Recommend `adopt` or `pin`.
2. Never delete. Archive with `revebot curator archive <name>` (user runs it),
   or move the directory to `skills/.archive/<name>/` only if they asked you
   to edit the tree directly.
3. If a skill has `references/`, `templates/`, `scripts/`, or `assets/`, keep
   the package, re-home those files and rewrite paths, or archive the whole
   package. Do not flatten only `SKILL.md` into another skill's `references/`.
4. Prefer patching an umbrella in place over creating a near-duplicate.
5. After merging A into B, say so plainly (`A → B`) so they can
   `revebot curator pin B`.

Read candidates with `read` / `ls` / `grep`. House skills:
`/workspace/skills/<name>/SKILL.md`. Bot-local:
`/workspace/agents/<your-id>/skills/<name>/SKILL.md` (shadows house on the
same name). Skip `*/skills/.archive/`.

## Config (`config.yml`)

```yaml
curator:
  enabled: true
  interval_hours: 168      # 7 days
  stale_after_days: 30
  archive_after_days: 90
  prune_builtins: false    # bundled skills stay unless this is true
  backup:
    enabled: true
    keep: 5
```

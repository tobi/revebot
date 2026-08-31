---
name: create-skill
description: >
  Create a new Reve skill (SKILL.md under workspace/skills or this bot's skills/).
  Use when the user wants a new skill, to scaffold a skill, or runs /create-skill.
---

# Create a skill

A skill is a `SKILL.md` the model is given when the user types `/name` (and listed in the system prompt). It is instructions, not host code.

## 1. Ask, one question at a time

1. **Name** — lowercase ascii `a-z`, digits, `-`, `_`. 2–64 chars. Must match `^[a-z0-9][a-z0-9_-]{0,62}$` in spirit: start/end alphanumeric. Example: `deploy-k8s`.
2. **Where** — **House** (recommended): `/workspace/skills/<name>/SKILL.md` (every bot). **This bot**: `/workspace/agents/<your-id>/skills/<name>/SKILL.md` (shadows a house skill of the same name).
3. **What it should do** — the workflow, a prompt they keep repeating, or the job to automate.

## 2. Description frontmatter

Write `description` so it is obvious when to use the skill: what it does, trigger phrases, and `Use when the user runs /<name>`.

Show the draft and let them edit it.

## 3. Write the file

Create the directory, then `SKILL.md`:

```
---
name: <name>
description: <approved description>
---

<markdown body>
```

Body is a prompt for the agent: steps, constraints, examples. No essays. Do not copy this skill into the new file.

Optional: `scripts/` or `references/` next to `SKILL.md` if the skill needs helpers. Those files are in `/workspace` and run in the microVM if a tool shells out — never on the host.

## 4. Confirm

Tell them:

- Slash: `/<name>`
- House vs bot path you used
- Bot-local skills shadow house skills of the same `name`

Do not invent a host command path. Skills are markdown.

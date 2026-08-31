# Evals

Scored cases for the house. Offline cases run in CI with a scripted model and
no microVM. Live cases talk to a real model and stay opt-in.

```bash
make eval                         # offline suite (`revebot eval`)
make eval ARGS='harness'          # one suite / path prefix
revebot eval --list
revebot eval --tag smoke
make eval ARGS='--live'           # real model; OPENROUTER_API_KEY by default
revebot eval --url http://127.0.0.1:7420 --token …   # same HTTP the UI uses
revebot eval --strict --junit evals/reports/junit.xml
```

`cargo test --test eval` runs the same offline suite.

## Layout

```
evals/
  suites.yaml              suite index
  cases/<suite>/*.yaml     one case per file (or a `cases:` list)
  reports/                 JSON written by `revebot eval`
  baselines/               optional saved reports to diff
```

## Case shape

```yaml
description: A prompt becomes a user entry and an assistant reply.
tags: [offline, smoke]
script:
  - text: hello back
test:
  - send: hello
  - succeeded
  - reply_includes: hello back
```

The file path is the identity when `id` is omitted (`evals/cases/harness/foo.yaml` → `harness/foo`).

`test:` is the drive-and-assert script. `send` talks to the agent; the rest are assertions:

| step | meaning |
|---|---|
| `send: …` | user turn |
| `succeeded` | run completed |
| `reply_includes` / `reply_not_includes` / `reply_matches` | last assistant text |
| `called_tool: name` or `{ name, args }` | a matching tool ran |
| `not_called_tool` / `used_no_tools` / `tool_order` | tool policy |
| `closed_qa: …` / `at_least` / `soft` | separate judge model; skipped with no key |
| `skip: reason` | omit this case |

Gates fail the process. `soft: true` is tracked unless you pass `--strict`. The older `prompt` / `graders` form still works.

### Runners

| runner | What it does |
|---|---|
| `harness` | Durable lane. Scripted model, or `--live` HttpModel. Tools are stubs that record calls. No VM. |
| `files` | `revebot init` into a temp directory; graders read the tree. |
| `unit` | Pure functions (`slug`, `unique_slug`, `session_path`, `house_tools`, `init_loads`). |
| `live_house` | Reserved: real House + microVM. Skipped until wired. |

### Graders

`outcome`, `contains`, `not_contains`, `regex`, `exact`, `equals` (unit extras),
`tools`, `tool_args`, `file_exists`, `file_not_exists`, `file_contains`,
`json_pointer`, `transcript`.

`in` is `final_text` (default) or `transcript`.

## Modes

- **offline** — no network, no guest. Default.
- **live** — real model via `models.yml`. Defaults to `openrouter/x-ai/grok-4.6` when `OPENROUTER_API_KEY` is set; otherwise the first provider whose key is present. Set `REVEBOT_EVAL_MODEL` to override the id. Requires `--live`.
- **microvm** — real guest. Requires `--microvm`. Empty in v1.

## Report

JSON goes to `evals/reports/<utc>.json`. `--save-baseline` copies it to
`evals/baselines/offline.json`. `--baseline PATH` prints regressions.

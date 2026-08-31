---
name: browser
description: >
  Steer the shared Google Chrome inside the microVM with agent-browser.
  Use when the user asks to open a site, click, fill a form, take a
  screenshot of a page, log in, or runs /browser.
---

# Browser

The guest runs one persistent Google Chrome on the XFCE desktop (`DISPLAY=:1`).
CDP listens on `127.0.0.1:9222`. `agent-browser` is preinstalled and already
configured (`~/.agent-browser/config.json`, `cdp: 9222`) to attach to that
Chrome. You and the user share the same window: they watch (and can take over)
in the house **Screen** panel.

Do not launch a second Chrome. Do not pass `--headed`, `--cdp`, or
`executablePath` unless you are deliberately isolating a session. Do not kill
Chrome.

## First checks

```bash
agent-browser --version
agent-browser doctor
```

If doctor fails, `wrap-desktop start` then retry. `DISPLAY` is `:1`.

## Drive the page

Prefer the accessibility snapshot and refs (`@e1`) over pixel coordinates.

```bash
agent-browser open https://example.com
agent-browser snapshot
agent-browser click @e1
agent-browser fill @e2 "value"
agent-browser type @e2 "more text"
agent-browser press Enter
agent-browser hover @e3
agent-browser scroll down
agent-browser wait --load
agent-browser get url
agent-browser get title
agent-browser screenshot /tmp/agent-browser-screenshots/page.png
agent-browser eval 'document.title'
```

Tabs:

```bash
agent-browser tabs
agent-browser tab new https://example.com
agent-browser tab select 0
```

`idleTimeout` is 0: the visible browser is not torn down between commands.

A project-level `./agent-browser.json` still overrides. Drop `cdp` there only
when you need an isolated headed Chrome, not the shared one.

## When the user must take over

Captchas, SSO, 2FA, cookie banners you cannot dismiss, or any page that needs
a human eyeball: tell them to use the Screen panel, wait, then snapshot again
and continue. Do not try to brute-force a login UI.

## Not the browser

For Thunar, terminals, dialogs, or drag-and-drop on the XFCE desktop, follow
`/computer`. For fetching HTML without a UI, a Lua `web_fetch` plugin or `curl`
is enough — do not open Chrome for that.

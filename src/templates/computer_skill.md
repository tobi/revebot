---
name: computer
description: >
  Drive the microVM desktop (XFCE on DISPLAY=:1) with xdotool and wrap-desktop.
  Use when a GUI app, file dialog, drag, or non-Chrome window must be operated,
  or when the user runs /computer.
---

# Computer

The house computer is this microVM. You run as unprivileged `user`
(`HOME=/home/user`, passwordless `sudo`). The Unix home is not your agent home
(`/workspace/agents/<your-id>/`).

XFCE is on X display `:1` (Xvfb, 1920×1080). `wrap-desktop start|stop|status|url`
supervises Xvfb, the session, Chrome, x11vnc and noVNC. Login shells autostart
it; if `:1` is down, start it yourself.

```bash
export DISPLAY=:1
wrap-desktop status || wrap-desktop start
xdpyinfo >/dev/null
```

The user sees the same desktop in the house **Screen** panel (noVNC). They can
watch, log in, or take over and hand back. There is no separate computer-use
subagent and no host GUI: you drive this VM with `bash`.

## Chrome vs everything else

- **Chrome** — `/browser` (`agent-browser` against the shared CDP). Do not
  click Chrome with xdotool when agent-browser can snapshot the page.
- **Other GUI** — `xdotool` on `:1`. Terminal: `xfce4-terminal`. Files: `thunar`.

## xdotool

```bash
export DISPLAY=:1
xdotool search --onlyvisible --name 'Firefox|Chrome|Thunar|Terminal'
xdotool windowactivate --sync <id>
xdotool mousemove 400 300 click 1
xdotool key ctrl+l
xdotool type --clearmodifiers -- 'text to type'
xdotool key Return
xdotool getmouselocation
```

Coordinates are the 1920×1080 desktop, origin top-left. The Chrome window is
placed at `(0,0)` sized 1600×1000.

Do not lock, dim, or sleep the session. Do not start a second X server. Do not
run `wrap-desktop stop` unless the user asked.

## Seeing the screen

You do not get a vision screenshot tool. For Chrome, `agent-browser snapshot`
(and `screenshot` to `/tmp/agent-browser-screenshots/`) is how you see. For
other windows, describe what you did, ask the user to look at Screen, or:

```bash
sudo pacman -S --noconfirm --needed scrot
scrot /tmp/desktop.png
```

then `read` is the wrong tool for a PNG — tell the user the path and that it
is on the desktop they are watching.

## When the user must take over

Native file pickers that need a human path, OS prompts, or anything you cannot
identify from a window name: ask them to use Screen, then continue.

# Shared VM knowledge

Only machine facts belong here: installed tools, shared filesystem locations,
network policy, and how to use this computer. No agent persona, project remit,
user preferences, or project-specific instructions.

- All agents share this microVM and `/workspace`.
- Each agent's home is `/workspace/agents/<id>/`.
- The guest Unix account is `user` (`HOME=/home/user`, passwordless `sudo`).
  That Unix home is not an agent home. Chrome, agent-browser and git global
  config live there.
- Shared repositories may live under `/workspace/projects/`.
- Commands run inside the VM. `mise` manages language runtimes and tools.
- Desktop: XFCE on `DISPLAY=:1` (1920×1080). The user watches it in the house
  Screen panel (noVNC) and can click to take over. `wrap-desktop start|status`.
- Browser: one persistent Google Chrome on that desktop, CDP `127.0.0.1:9222`.
  `agent-browser` is preconfigured to attach. Follow `/browser`.
- Other GUI: `xdotool` on `:1`. Follow `/computer`.
- Keep project knowledge in explicitly scoped project memory, not this file.

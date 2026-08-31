# Shared VM knowledge

Only machine facts belong here: installed tools, shared filesystem locations,
network policy, and how to use this computer. No agent persona, project remit,
user preferences, or project-specific instructions.

- All agents share this microVM and `/workspace`.
- Each agent's home is `/workspace/agents/<id>/`.
- Shared repositories may live under `/workspace/projects/`.
- Commands run inside the VM. `mise` manages language runtimes and tools.
- Keep project knowledge in explicitly scoped project memory, not this file.

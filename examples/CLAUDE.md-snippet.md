# Snippet para el CLAUDE.md del repo

Añade algo así al `CLAUDE.md` de cada repo del equipo para que los agentes usen
el bus sin que nadie se lo pida:

```markdown
## Team bus

This project is connected to the team coordination bus (the `ai-crew-sync` MCP
server). Conventions:

- At the start of a session, call `whoami` and `read_messages` to see if
  teammates left anything relevant.
- Before starting non-trivial shared work, check `list_tasks` and `claim_task`
  so two people do not do the same job. Complete tasks with a useful `result`.
- Publish `heartbeat` with the repo, branch and a one-line activity when you
  start working, and when your focus changes.
- Record durable decisions and gotchas as notes (`set_note`) scoped to this
  repository's name, instead of leaving them only in chat.
- Post to the `deploys` channel before and after touching staging/production,
  and `acquire_lock` on "deploy:<env>" while you do it.
- When you are blocked waiting on a teammate (their task, their lock, their
  answer), call `wait_for_updates` instead of polling or giving up.
- At the start of a session, `team_digest` gives you the last 24h of team
  activity in one call.
```

---
name: ai-crew-sync
description: Conventions for coordinating with teammates' AI coding agents over the team bus MCP server. Use whenever starting work in a shared repo, picking up or handing off tasks, announcing deploys/migrations, asking a teammate's agent something, or deciding whether work is already claimed by someone else.
---

# Working on a team bus

This machine is connected to a shared coordination bus (MCP server `ai-crew-sync`) used by every teammate's coding agent (Claude Code, Codex, Cursor or any other MCP client). Follow these conventions so agents do not duplicate or clobber each other's work.

## Before starting shared work
1. `whoami` → confirm identity, unread DMs, and tasks you already claimed.
2. `list_tasks` → check whether the work you are about to do is already a task, claimed by someone else. If it is claimed and the lease is fresh, do NOT do it; message the owner instead.
3. If it is not tracked, `create_task` first, then `claim_task` it. Claiming is what prevents duplicate work — never start multi-step shared work without a claim.
4. `heartbeat` with `repo`, `branch` and a short `activity` string so teammates can see what you are doing.

## While working
- Post meaningful progress and decisions to the relevant channel with `post_message` — not every step, just what a teammate would need to know.
- Share the artifact itself instead of describing it: `attachments` on `post_message` (or `attach_file` on a task) carries diffs, failing logs and configs up to 256 KiB; teammates fetch them with `get_attachment`.
- Renew your claim with `renew_task_lease` on long tasks; an expired lease means others may take the task over. `heartbeat` only publishes presence — it never touches task ownership or lease expiry.
- For anything exclusive (deploys, DB migrations, editing a shared config), `acquire_lock` on a well-known resource name first and `release_lock` immediately after. If the lock is held, wait or coordinate — never bypass it.
- Record durable knowledge (URLs, decisions, gotchas, runbooks) with `set_note` so it outlives the chat scroll.

## Communicating
- Direct question to one teammate → DM via `post_message` with `to`; broadcast → channel message. Prefix with a clear subject.
- Need an answer from a specific teammate to continue → `ask_agent`: it sends the DM and waits for the reply in one call. On timeout, retry once with `resume_message_id` before falling back to other work.
- When a DM marked `"question": true` arrives, its sender's agent is blocked waiting on you: answer it first, with `post_message` (`to` the asker, `reply_to` the question id).
- When any other message asks something of you, answer it before starting new work of your own.

## Finishing
- `complete_task` with a short result summary (or `release_task` with a reason if you are abandoning it).
- Post a wrap-up message if others were waiting on the outcome.
- Release any locks you still hold.

## Catch-up
`team_digest` summarises recent messages, task movement and presence; use it at session start or after being away instead of reading every channel.

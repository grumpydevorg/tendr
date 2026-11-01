---
id: interactive-agent-turns
depends_on: []
---

# Interactive agent turns: route agent hooks into the event log

**Status:** not started. Starts when a program next drives a Claude Code turn
under tendr. Ready: a guide topic, then one generic exit-on-match option for
`tendr events --follow`.

Some agent features exist only in an interactive session. A caller (a script or
another agent) needs to send that session a prompt, learn when the turn ended or
stopped on a question, and find the result without reading the screen. Tendr can
do this today with no code change: the agent's lifecycle hooks call `tendr emit`,
and the caller reads turn state from the session's event log. What is missing is
a guide topic that says how, and a way to wait for one event without a
background follower.

Tendr stores these events and does not interpret them. What `hook.stop` means is
the caller's policy; see [design](../../design.md#tendr-supervises-processes-it-is-not-an-agent-framework).
For a local caller that only needs one Claude Code turn, `claude --bg` and
`claude --bg --resume` are simpler. Tendr is for when the record must be
queryable, the session must survive SSH loss, or the agent has no background
mode.

## Measured facts for the guide

Measured 2026-10-08 on macOS 26.6.2 with tendr 0.3.0 and Claude Code 2.1.293. A
Tendr PTY session with hooks calling `tendr emit` published a Claude Design
artifact; submission, permission prompts, the turn end and the result came from
events and the transcript. Facts 4 and 7 note tendr 0.4.0 changes; the recipe
was not re-run on 0.4.0.

1. **Hooks inherit the session.** Hooks are Claude Code's children, so they
   inherit the sidecar's `TENDR_*` variables; `tendr emit` without `--session`
   wrote to the right session and run.
2. **`prompt_id` correlates a turn.** `UserPromptSubmit` and `Stop` carry the
   same `prompt_id`; `Stop` also carries `last_assistant_message` and
   `transcript_path`. In the transcript, `promptId` is on user entries (the
   prompt and each tool result), not on assistant entries.
3. **Permission prompts are visible.** A `Notification` with
   `notification_type: "permission_prompt"` arrived 9 s after submission, and
   `printf 1 | tendr push` answered it. Claude Code's docs say
   `PermissionRequest` fires at once.
4. **Submit as text, a pause, then a lone CR.** A long prompt ending in `\r` was
   taken as a paste and its CR removed; text, about 0.5 s, then `\r` worked
   first time. Only `hook.user_prompt_submit` confirms acceptance. Since 0.4.0
   (#115) `push` to a PTY needs no `--stdin`.
5. **Launch the resolved binary** (`readlink -f "$(command -v claude)"`):
   `tendr start` takes argv, so shell aliases do not apply.
6. **Oversize `data` loses its fields.** Over 16 KiB, `emit` spills the payload
   and stores only a preview (`MAX_INLINE_DATA_BYTES`, `src/events.rs:37`). A
   raw 40 KB `Stop` payload lost `prompt_id`; project it with `jq` first.
7. **Agent shells set `TERM=dumb`, `CI=true`, `NO_COLOR=1`.** Since 0.4.0 (#117)
   a PTY child gets `xterm-256color` for a dumb `TERM`; remove the others with
   `--unset-env CI --unset-env NO_COLOR`.
8. **`--kind` is a prefix match**: `--kind hook.stop` also matches
   `hook.stop_failure`.
9. **A follower can start first.** `events --follow --from-now --ready-file f`
   became ready before the session existed and caught its first event.
10. **Nothing ends on the first match.** The probe polled a background
    follower's file. Piping `--follow` into `grep -m1` does not end while the
    producer is silent.

`StopFailure` fires instead of `Stop` when a turn ends on an API error. A user
interrupt fires neither, so every wait needs a deadline. The `PermissionRequest`
and `StopFailure` hooks and the `jq` projection were not exercised in a live
Claude session; the projection and spill were checked with a scripted payload.

## Trust notes for the guide

- **Hook events are not authenticated.** Only `tendr.*` sources are reserved,
  and `source` is caller-supplied ([event protocol](../../architecture/07-event-protocol.md)
  §1). Any process in the session can emit a `hook.stop` with a guessed
  `prompt_id`. Never grant a permission on the strength of an event alone.
- **`--best-effort` loses events silently.** A missing hook script, a full disk,
  or a session pruned or replaced mid-turn (orphan emits go to `lost+found`,
  §7) all look like a slow turn. A missed deadline means the outcome is unknown.
- **Payload shapes are Claude Code's** and can change with any release. Name
  fields explicitly in the projection so a rename shows as a missing value.

## Work

### 1. Guide topic: agent hooks into the event log

Add a `tendr guide` topic (a heading in `docs/guide.md`, a topic entry in
`src/commands/guide.rs`, a line in `src/embedded/SKILL.md`). It covers:

- The pattern: a hook runs `tendr emit --kind hook.<name>` for a standalone
  event, or `tendr wrap --event hook.<name> -- <cmd>` to run a command and link
  its output to the event. `hook.` is open vocabulary, not reserved.
- The Claude Code recipe: a hook script that projects the payload with `jq`
  (keeping `last_assistant_message` short) and pipes it to
  `tendr emit --best-effort --source claude.hook --kind "hook.$1" --data-stdin
  >/dev/null`, so nothing reaches stdout, which Claude Code reads as a hook
  decision; settings for `SessionStart`, `UserPromptSubmit`,
  `PermissionRequest`, `Notification`, `Stop` and `StopFailure`, passed with
  `--settings`; the launch line; and the order: follower first, then start,
  then push. The tested script, settings, launch lines and transcript query
  are in this file at `83f83ab`, "The recipe (works today)".
- Reading the result from the transcript, scoped to the turn by `promptId`. The
  transcript is written asynchronously, so retry a missing result briefly.
- The measured facts above that a caller needs, with the versions, and the
  trust notes.
- Consumers (UIs, analytics, `tendr query`) read `hook.*` events; Tendr does not
  route or render them.

In the same change, replace the guide's herdr limit "Run agents in herdr panes,
not under tendr": it holds for agents a person watches, not for agents a program
drives.

### 2. `tendr events --follow` exits on a match

A generic way to stop following after the first matching event, with a deadline.
It matches on the existing `--session`, `--kind` and `--source` filters and knows
nothing about agents. `ready-check` would use it to wait for `run.ready`.

Open points:

- Flag shape: `--max-count N` or `--until-first`, plus `--timeout <duration>`.
- Correlation: match one `data` field in Tendr (for example
  `--data prompt_id=<id>`), or leave it to the caller's `jq`. The second needs
  the follower to write while idle so a closed pipe is noticed; `--cursors`
  already writes a bookmark after 5 s idle (`BOOKMARK_IDLE`,
  `src/commands/events.rs`).
- Exit code for the deadline: a new reserved code in the guide's exit-code
  table.

## Acceptance

- `tendr guide <topic>` prints the hook topic: the pattern, the Claude Code
  recipe with the versions measured, the submit rule, the permission signals and
  the trust notes. It uses only shipped verbs and fields, and the herdr limit no
  longer tells people to keep every agent out of Tendr.
- The guide says plainly that `tendr wait` waits for process exit, not a turn.
- A caller can push a prompt and block until the matching end-of-turn event with
  one `tendr events` call and a deadline, with no background follower.
- A test drives a scripted child that emits submit and stop events and covers:
  two turns in one session; a stop for another `prompt_id` that must not match;
  a `hook.stop_failure` end; a payload over 16 KiB whose correlation field
  matches or fails by a documented rule; the deadline expiring; and an event
  written before a `--from-now` follower started, which must not match.

---
id: interactive-agent-turns
depends_on: []
links:
  - agent-hook-routing.md
  - ../active/00_cloud-pty-control.md
  - ../specs/event-protocol.md
  - ../specs/decision-process-sitter-not-framework.md
  - ../specs/ecosystem-landscape.md
---

# Interactive agent turns — drive one turn of a live agent under a Tendr PTY

Some agent features exist only in an interactive session. A caller (another
agent, a script) needs to send that session a prompt, learn when the turn has
finished or stopped on a question, and find what it produced, without scraping
a screen. Measured on 2026-10-08, Tendr can already do this with no code
change: the agent's own lifecycle hooks publish into the session's event log,
and the caller reads turn state from events instead of the screen.

This plan records that measurement, places the workflow beside herdr, and lists
the small generic additions that would make it a supported recipe rather than a
script. It extends [agent-hook-routing](agent-hook-routing.md) with a measured
Claude Code recipe and a concrete consumer.

## The consumer

oh-my-pi (omp) wanted Claude Code's `/design`, which publishes a Claude Design
artifact to claude.ai. It is available only in an interactive Claude Code
session. Under `claude -p`, `/design` only grants or revokes agent access
(`/design consent`, `/design revoke`), and the `Artifact` tool is not loaded,
even with `--settings '{"enableArtifact":true}'` (Claude Code 2.1.293, checked
from the `-p` init event's tool list). So the one-shot `--after` model in the
[agent-exec spike](../completed/2026-04-03-agent-exec-spike.md) does not reach
it. The caller has to drive a long-lived interactive process turn by turn.

## Measured 2026-10-08

macOS 26.6.2, tendr 0.3.0, Claude Code 2.1.293, herdr 0.9.3.

| Driver | Result |
|---|---|
| tmux `send-keys` + `capture-pane` | Published a design. Needed separate text and Enter keys, a fixed sleep, and reading the URL off the screen. |
| herdr `agent prompt --wait` | Published a design. `--wait` returned on `done` after 34 s. The result was still read off the screen. herdr's Claude hook integration was not installed. |
| omp's built-in PTY tool | Text arrived but was never submitted. This was the omp tool, not raw PTY bytes in general: `tendr push` below submitted correctly. |
| **Tendr PTY + hooks → `tendr emit`** | **Published a design. Submission, completion, permission prompts and the result all came from events and the transcript, not the screen.** |

Facts from the Tendr run:

1. **Hooks inherit the session.** Claude Code runs hook commands as its own
   children, so they inherit the `TENDR_*` variables the sidecar set. `tendr
   emit` without `--session` wrote to the right session, and each event's
   `run_id` matched the sidecar's `run.started`.
2. **`prompt_id` correlates a turn.** Claude Code's `UserPromptSubmit` and
   `Stop` hook payloads carry the same `prompt_id`. The `Stop` payload also
   carries `last_assistant_message`, `session_id` and `transcript_path`. Two
   turns in one session produced two submit/stop pairs with distinct ids.
3. **A permission prompt is observable.** In `--permission-mode default`, a Bash
   request raised `hook.notification` with `notification_type:
   "permission_prompt"` nine seconds after submission. `printf 1 | tendr push`
   approved it, and the next `hook.stop` followed eight seconds later. The
   command's file existed afterwards.
4. **Submission depends on how the bytes arrive.** `printf 'Reply with exactly:
   pong\r' | tendr push` submitted in one write. A longer prompt with a trailing
   `\r` in the same write was treated as a paste: Claude Code removed the CR
   ("Removed 1 invisible character · review and press Enter to send"), and two
   further lone CRs were needed. Pushing the text, pausing about 0.5 s, then
   pushing a lone `\r` submitted first time. The paste heuristic is inferred
   from the message, not from Claude Code's source.
5. **`tendr wait` is not a turn primitive.** It waits for process exit, as
   documented. On the live session it timed out (`--timeout 3`, exit 1) after
   two completed turns. Turn waits must come from events.
6. **The PTY needs `--stdin` for `push`.** `start --pty` without `--stdin`
   refused `push` with "session was not started with --stdin".
7. **Launch the executable, not a shell word.** `tendr start` takes argv, so
   shell aliases and builtins such as `command claude` do not apply. Resolve
   the binary (`readlink -f "$(command -v claude)"`). On the measuring machine
   the interactive `claude` alias routed through an account-rotating proxy;
   launching the resolved binary bypassed it, which was the intent.
8. **The gap is waiting for one event.** The probe ran `tendr events --follow
   --from-now --ready-file` in the background into a file and polled that file
   with `jq`. Nothing in Tendr says "block until the first event matching this
   kind (and this `prompt_id`) arrives, or time out".

PTY recording stayed `Recording` throughout, so the screen history is retained
for humans and replay even though no automation reads it.

## The recipe (works today)

Claude Code settings, passed with `--settings` so they apply to this run only:

```json
{"hooks":{
  "SessionStart":[{"hooks":[{"type":"command","command":"tendr emit --best-effort --source claude.hook --kind hook.session_start --data-stdin"}]}],
  "UserPromptSubmit":[{"hooks":[{"type":"command","command":"tendr emit --best-effort --source claude.hook --kind hook.user_prompt_submit --data-stdin"}]}],
  "Notification":[{"hooks":[{"type":"command","command":"tendr emit --best-effort --source claude.hook --kind hook.notification --data-stdin"}]}],
  "Stop":[{"hooks":[{"type":"command","command":"tendr emit --best-effort --source claude.hook --kind hook.stop --data-stdin"}]}]
}}
```

Use an absolute path to `tendr` if the hook environment's `PATH` may differ.
`--best-effort` keeps a Tendr failure from failing the agent's hook.

```bash
CLAUDE=$(readlink -f "$(command -v claude)")
tendr start cc --namespace agents --pty --stdin --cwd "$PWD" \
  --env CLAUDE_CODE_ARTIFACT_AUTO_OPEN=0 \
  -- "$CLAUDE" --settings ./tendr-hooks.json

# Follow before sending, so the turn's events cannot be missed.
tendr events --session agents/cc --kind hook. --follow --from-now \
  --ready-file ./ready > ./events.ndjson &

# Wait for hook.session_start, then:
printf '/design a settings screen' | tendr push cc --namespace agents
sleep 0.5
printf '\r' | tendr push cc --namespace agents

# The turn:  hook.user_prompt_submit  → note .data.prompt_id
# Blocked:   hook.notification with .data.notification_type == "permission_prompt"
#            → answer with push, e.g. printf 1 | tendr push cc --namespace agents
# Finished:  hook.stop with the same .data.prompt_id
#            → .data.last_assistant_message, .data.transcript_path
```

Result extraction stays with the caller. For `/design`, the artifact URL is the
`url` input of the `Artifact` tool call in the transcript:

```bash
jq -r 'select(.type=="assistant") | .message.content[]?
       | select(.type=="tool_use" and .name=="Artifact") | .input.url // empty' \
   "$TRANSCRIPT" | sort -u
```

A human can watch or take over with `tendr attach cc --namespace agents`, which
is refused while a push is in flight, and `--takeover` claims the terminal
outright (see the guide's "Answer prompts and attach").

## Where this sits beside herdr

herdr and Tendr compose. The question is which one owns the agent process.

| | Agent in a herdr pane (shipped, `tendr guide herdr`) | Agent under a Tendr PTY (this plan) |
|---|---|---|
| Owner of the agent process | herdr pane | Tendr sidecar |
| Survives pane close, server stop, SSH loss | No | Yes |
| Durable record | None for the agent | Event log, `output.log`, exact PTY recording |
| Turn state | herdr, from its agent integration or the screen | Agent hooks → `tendr emit` → events |
| Send a prompt | `herdr agent prompt` | `tendr push` (text, then a lone CR) |
| Remote host | herdr's own SSH machine support | `--host`, and cloud PTY control |
| Human view | The pane itself | `tendr attach` inside any terminal, a herdr pane included |
| herdr state indicator | Correct | Blind: herdr does not see an agent under Tendr |
| Processes the agent starts | Tendr sessions | Tendr sessions |

Use the herdr side for local, human-led work where a person watches the agent.
Use the Tendr side when the caller is a program, the record matters, or the
session must survive disconnects or run remotely — the same workflow the
[cloud PTY plan](../active/00_cloud-pty-control.md) builds for Claude Code on a
VM. That plan already says structured completion comes from hooks, not idle
timers; this recipe is that signal, measured locally.

The blind herdr indicator is herdr's to fix, not Tendr's. herdr accepts agent
state only from a pane's own agent session (`tendr guide herdr`), so Tendr
cannot push state into it. If it matters, ask herdr for an agent kind whose
state comes from a Tendr session's `hook.*` events. Until then a herdr pane
running `tendr attach` is a viewer, not a state source.

## Boundary with the process-sitter decision

[Tendr stays a process sitter](../specs/decision-process-sitter-not-framework.md):
it does not parse LLM responses. Nothing here changes that.

- `hook.*` kinds and their `data` belong to the agent. Tendr stores them under
  the open `hook.` vocabulary and does not interpret them.
- Waiting for a turn is generic event matching: the first event of kind *K*,
  optionally with one `data` field equal to a value, after a known point. Tendr
  does not know that `hook.stop` means "turn finished".
- Reading the transcript, finding an artifact URL, or approving a permission
  prompt is the caller's policy.

## Proposed work, in order

1. **Guide topic.** Add the recipe above, with the measured facts, to the
   `docs/guide.md` topic that [agent-hook-routing](agent-hook-routing.md)
   creates. Claude Code is the measured, most common case; agent-hook-routing
   asks for one Hermes recipe, and this supplies the Claude one. Include the
   submit rule (text, pause, lone CR) and the herdr comparison table.
2. **Wait for the first matching event.** A generic way for `tendr events
   --follow` to exit after a match, with a deadline, so callers stop
   backgrounding a follower and polling a file. Open design points:
   - Flag shape: `--until-first` or `--max-count N`, combined with the existing
     `--kind` / `--source` filters.
   - Correlation: a single `data` equality filter, so a caller can wait for
     `hook.stop` with this `prompt_id`. Keep it to one path and one value; a
     query language belongs in `tendr query`.
   - Starting point: `--from-now` plus `--ready-file` is already race-free if
     the follower starts before the push. A cursor read from the submit event
     is the alternative.
   - Exit codes: 0 on match; a reserved, documented code for the deadline,
     following the command-scoped exit-code table.
3. **Submission without a sleep.** The text–pause–CR rule is an inference
   about one TUI. The proper fix is cloud PTY control slice 4's mode-aware
   keys and bracketed paste in `tendr-screen`. Do not add a core
   "submit" verb ahead of that; document the rule and the measured versions.
4. **herdr.** No Tendr change. Record the blind-indicator limit in the guide,
   and raise the agent-kind idea with herdr if the herdr-viewer layout gets
   used.

## Non-goals

- Agent-specific code in core: no Claude Code exec target, transcript parser,
  or permission policy.
- Screen reading or idle-timer completion.
- Taking over an agent that Tendr did not launch.

## Acceptance criteria

- The guide documents the hook recipe, the submit rule, the permission-prompt
  signal and the herdr table, citing the versions measured.
- A caller can submit a prompt and block until the matching `hook.stop` with
  one `tendr events` invocation and a deadline, without a background follower.
- A test drives a scripted PTY child that emits submit/stop events, covering:
  two turns in one session, a stop for a different id that must not match, the
  deadline expiring, and a follower started after the event (must not match
  with `--from-now`).
- `tendr wait` documentation says plainly that it waits for process exit, not
  for an agent turn.

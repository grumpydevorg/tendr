---
id: interactive-agent-turns
depends_on: []
links:
  - agent-hook-routing.md
  - ../active/00_cloud-pty-control.md
  - ../specs/event-protocol.md
  - ../specs/decision-process-sitter-not-framework.md
  - ../specs/tender-as-block-runtime.md
  - ../specs/ecosystem-landscape.md
---

# Interactive agent turns — drive a live agent's turns under a Tendr PTY

Some agent features exist only in an interactive session. A caller (another
agent, a script) needs to send that session a prompt, learn when the turn has
finished, failed or stopped on a question, and find what it produced, without
scraping a screen. Measured on 2026-10-08, Tendr can already do this with no
code change: the agent's own lifecycle hooks publish into the session's event
log, and the caller reads turn state from events.

This plan records the measurement and an adversarial review of it, places the
workflow beside herdr and the agent's own background mode, and proposes one
small generic addition. It supplies the measured Claude Code recipe that
[agent-hook-routing](agent-hook-routing.md) needs alongside its Hermes example.

## The consumer, and what already serves it

oh-my-pi (omp) wanted Claude Code's `/design`, which publishes a Claude Design
artifact to claude.ai. Claude Code 2.1.293 loads the `Artifact` tool only in an
interactive session:

- `claude -p`: no `Artifact` tool, even with `--settings
  '{"enableArtifact":true}'`; `/design` only offers `consent` and `revoke`.
- `claude -p --input-format stream-json --output-format stream-json`
  (multi-turn print mode): no `Artifact` tool either.
- **`claude --bg "/design …"` works.** Claude Code's own background mode
  published a design, `claude agents --json --all` reported `state: "done"`,
  and the URL was in the transcript. A second turn reached the same session
  with `claude stop <id>` then `claude --bg --resume <session-id> "<prompt>"`.
  It republished to the same artifact URL. Passing any other flag on that
  resume (for example `--model`) started a copy under a new id instead.

So for a local caller that only wants a Claude Code turn, the vendor's
background mode is the simpler answer. Tendr's case is the one `--bg` does not
cover:

- the caller needs a durable record it can query alongside other work;
- the session must survive SSH loss, or run on a remote host (the
  [cloud PTY plan](../active/00_cloud-pty-control.md));
- the same driving pattern must work for agents without a background mode.

## Measured 2026-10-08

macOS 26.6.2, tendr 0.3.0, Claude Code 2.1.293, herdr 0.9.3.

| Driver | Result |
|---|---|
| tmux `send-keys` + `capture-pane` | Published a design. Needed separate text and Enter keys, a fixed sleep, and reading the URL off the screen. |
| herdr `agent prompt --wait` | Published a design. `--wait` returned on `done` after 34 s. The result was still read off the screen. herdr's Claude hook integration was not installed. |
| omp's built-in PTY tool | Text arrived but was never submitted. This was the omp tool, not raw PTY bytes in general: `tendr push` below submitted correctly. |
| `claude --bg` + `--resume` | Published, then a second turn republished the same artifact. State from `claude agents --json --all`. Local only; no record outside Claude Code's own files. |
| **Tendr PTY + hooks → `tendr emit`** | **Published a design. Submission, completion, permission prompts and the result came from events and the transcript, not the screen.** |

Facts from the Tendr run:

1. **Hooks inherit the session.** Claude Code runs hook commands as its own
   children, so they inherit the `TENDR_*` variables the sidecar set. `tendr
   emit` without `--session` wrote to the right session, and each event's
   `run_id` matched the sidecar's `run.started`.
2. **`prompt_id` correlates a turn.** `UserPromptSubmit` and `Stop` payloads
   carry the same `prompt_id`. `Stop` also carries `last_assistant_message`,
   `session_id` and `transcript_path`. Two turns in one session produced two
   submit/stop pairs with distinct ids. In the transcript, `promptId` appears
   on user entries (the prompt and each tool result), not on assistant entries.
3. **A permission prompt is observable.** In `--permission-mode default`, a
   Bash request raised `hook.notification` with `notification_type:
   "permission_prompt"` nine seconds after submission. `printf 1 | tendr push`
   answered the dialog, and `hook.stop` followed eight seconds later. This shows
   the mechanism only; a caller that approves blindly has no permission policy.
   Claude Code's docs say `permission_prompt` waits about six seconds, and
   `PermissionRequest` fires immediately.
4. **Submission depends on how the bytes arrive.** `printf 'Reply with exactly:
   pong\r' | tendr push` submitted in one write. A longer prompt with a trailing
   `\r` in the same write was treated as a paste: Claude Code removed the CR
   ("Removed 1 invisible character · review and press Enter to send"), and two
   further lone CRs were needed. Pushing the text, pausing about 0.5 s, then
   pushing a lone `\r` submitted first time. The paste heuristic is inferred
   from the message, not from Claude Code's source. A push acknowledgement
   means the bytes reached the PTY, not that a prompt was accepted
   ([cloud PTY plan](../active/00_cloud-pty-control.md), "Controller state and
   the single input writer"); only `hook.user_prompt_submit` confirms that.
5. **`tendr wait` is not a turn primitive.** It waits for process exit, as
   documented. On the live session it timed out (`--timeout 3`, exit 1) after
   two completed turns.
6. **The PTY needs `--stdin` for `push`.** `start --pty` without `--stdin`
   refused `push` with "session was not started with --stdin".
7. **Launch the executable, not a shell word.** `tendr start` takes argv, so
   aliases and builtins such as `command claude` do not apply. Resolve the
   binary (`readlink -f "$(command -v claude)"`). On the measuring machine the
   interactive `claude` alias routed through an account-rotating proxy;
   launching the resolved binary bypassed it, which was the intent.
8. **Oversize payloads lose their fields.** `emit` passes no structured
   preview, so `data` over 16 KiB spills and the stored `data` becomes
   `{"preview": "<string>"}`. A 40 KB `Stop` payload emitted raw kept no
   `prompt_id`; the same payload projected with `jq` first kept it. Long final
   messages are exactly the turns a caller waits for.
9. **`--kind` is a prefix match.** `tendr events --kind hook.stop` returned both
   `hook.stop` and `hook.stop_raw`, so it also matches `hook.stop_failure`.
   The recipe relies on this: one filter sees a turn end either way.
10. **A follower can start before its session exists.** `tendr events --session
    ns/x --follow --from-now --ready-file f` became ready before `ns/x` was
    started and caught its first event.
11. **The gap is exit-on-match.** The probe ran a background follower into a
    file and polled it with `jq`. Piping `--follow` into `jq` or `grep -m1`
    does not end on the match while the producer is silent.

## The recipe (works today)

`tendr-hook.sh`, called with the kind suffix. It keeps the payload under the
16 KiB inline cap and never writes to stdout, which Claude Code would parse as
a hook decision (`emit` prints nothing on success today; the redirect keeps it
that way):

```sh
#!/bin/sh
jq -c '{hook_event_name, session_id, prompt_id, transcript_path,
        notification_type, message, tool_name, error,
        last_assistant_message: ((.last_assistant_message // "") | .[0:4000])}' \
  | tendr emit --best-effort --source claude.hook --kind "hook.$1" --data-stdin >/dev/null
```

Claude Code settings, passed with `--settings` so they apply to this run only:

```json
{"hooks":{
  "SessionStart":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh session_start"}]}],
  "UserPromptSubmit":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh user_prompt_submit"}]}],
  "PermissionRequest":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh permission_request"}]}],
  "Notification":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh notification"}]}],
  "Stop":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh stop"}]}],
  "StopFailure":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh stop_failure"}]}]
}}
```

`StopFailure` fires instead of `Stop` when a turn ends on an API error. A user
interrupt fires neither, so every wait needs a deadline. The `PermissionRequest`
and `StopFailure` hooks and the `jq` projection inside a live Claude session
were not exercised in the 2026-10-08 run; the projection and spill behaviour
were checked with a scripted payload.

```bash
# 1. Follow first, so hook.session_start cannot be missed.
rm -f ./ready
tendr events --session agents/cc --kind hook. --follow --from-now \
  --ready-file ./ready > ./events.ndjson &
until [ -f ./ready ]; do sleep 0.1; done

# 2. Launch the resolved binary.
CLAUDE=$(readlink -f "$(command -v claude)")
tendr start cc --namespace agents --pty --stdin --cwd "$PWD" \
  --env CLAUDE_CODE_ARTIFACT_AUTO_OPEN=0 \
  -- "$CLAUDE" --settings ./tendr-hooks.json

# 3. After hook.session_start, give the input box a moment (the probe used 3 s):
printf '/design a settings screen' | tendr push cc --namespace agents
sleep 0.5
printf '\r' | tendr push cc --namespace agents

# Submitted: hook.user_prompt_submit → note .data.prompt_id
# Blocked:   hook.permission_request (immediate) or hook.notification with
#            notification_type "permission_prompt" (~6 s later). Decide by
#            the caller's policy; answer with push.
# Ended:     kind with prefix hook.stop and the same .data.prompt_id;
#            hook.stop_failure means an API error.
# No end before the deadline: the outcome is unknown, not "still working".
```

Result extraction stays with the caller. Scope it to the turn by joining that
turn's tool results (user entries with the turn's `promptId`) to the assistant's
`Artifact` calls; an unscoped query returns every artifact in the session:

```bash
jq -rs --arg p "$PROMPT_ID" '
  ([.[] | select(.type=="user" and .promptId==$p)
        | .message.content[]? | select(.type=="tool_result") | .tool_use_id]) as $ids
  | [.[] | select(.type=="assistant") | .message.content[]?
         | select(.type=="tool_use" and .name=="Artifact" and (.id as $i | $ids | index($i)))
         | .input.url // empty] | unique[]' "$TRANSCRIPT"
```

Checked on a two-turn session: the plain-text turn returned nothing and the
`/design` turn returned its URL. Claude Code's hook docs say the transcript is
written asynchronously and may lag when a hook fires, so retry a missing result
briefly before treating it as absent.

A human can watch or take over with `tendr attach cc --namespace agents`.
Attach is refused while a push is in flight; `--takeover` claims the terminal
outright.

## Trust and failure

- **Hook events are not authenticated.** Only `tendr.*` sources are reserved,
  and `source` is caller-supplied (event-protocol §1). Any process in the
  session, including the agent's own tool children, can emit a `hook.stop` with
  a guessed `prompt_id`. The recipe assumes one user trusts everything running
  in the session. Never grant a permission on the strength of an event alone.
- **`--best-effort` loses events silently.** A missing hook script, a full disk,
  or a session pruned or replaced mid-turn (orphan emits go to `lost+found`,
  event-protocol §7) all look like a slow turn. Deadlines are mandatory, and a
  missed deadline is reported as unknown.
- **Payload shapes are Claude Code's.** Field names were read from 2.1.293. A
  version bump can change them; the projection names fields explicitly so a
  rename shows up as a missing value rather than a silent mismatch.

## Where this sits beside herdr and `--bg`

| | Agent in a herdr pane (`tendr guide herdr`) | Claude Code `--bg` | Agent under a Tendr PTY (this plan) |
|---|---|---|---|
| Owner of the agent process | herdr pane | Claude Code's background service | Tendr sidecar |
| Survives pane close, SSH loss | No | Yes, locally | Yes |
| Durable, queryable record | None for the agent | Claude Code's own transcript | Event log, `output.log`, PTY recording |
| Turn state | herdr, from its agent integration or the screen | `claude agents --json` | Agent hooks → `tendr emit` |
| Send a prompt | `herdr agent prompt` | stop, then `--bg --resume` | `tendr push` (text, then a lone CR) |
| Agents covered | herdr's supported kinds | Claude Code only | Any agent with lifecycle hooks |
| Remote host | herdr's own SSH machines | No | Planned: `events` and `emit` are not in `REMOTE_COMMANDS` yet (`src/ssh.rs`); the cloud PTY plan |
| Human view | The pane | `claude attach` | `tendr attach`, in any terminal |
| herdr state indicator | Correct | Not in a pane | Blind |

Use herdr for local work a person watches, and `claude --bg` when a local
program only needs a Claude Code turn. Use the Tendr PTY when the record matters,
the session must survive disconnects or run remotely, or the agent has no
background mode of its own. The cloud PTY plan already says structured
completion comes from hooks, not idle timers; this recipe is that signal,
measured locally.

The blind herdr indicator is herdr's to fix. herdr accepts agent state only from
a pane's own agent session (`tendr guide herdr`), so Tendr cannot push state
into it. A herdr pane running `tendr attach` is a viewer, not a state source.

## Boundary: process sitter, not agent framework

The [2026-04-03 decision](../specs/decision-process-sitter-not-framework.md)
rejects native LLM protocols. Its premise that agent CLIs are one-shot
(its "Rationale" and "How LLM Orchestration Works Today" sections) no longer describes them:
interactive-only features, Claude Code's background mode, and omp's RPC mode are
all long-lived. The rejection itself still holds, and the refined stance already
lives in [tender-as-block-runtime](../specs/tender-as-block-runtime.md), which
says the original decision "holds for that specific question" and puts
interpreting events such as "Claude Code semantics" in consumer territory.

An adversarial review on 2026-10-08 (GPT-6 Astra and Claude Fable 5.1,
independently, then a rebuttal round) rejected both extremes:

- **Not a turn contract in core.** "Done", "blocked" and "settled" are vendor
  vocabulary that changes between releases. A typed `{turn_id, status}` result
  belongs in a consumer or adapter. Exec targets are not a precedent: for the
  REPL lanes Tendr writes both the frame and the result file, while an agent
  turn's states are written by the vendor.
- **Not "nothing to add" either.** A generic exit-on-match for `tendr events`
  is justified (fact 11).

So, in this plan:

- `hook.*` kinds and their `data` belong to the agent. Tendr stores them under
  the open `hook.` vocabulary and does not interpret them.
- Waiting is generic event matching: the first event of kind *K* (prefix
  match), optionally correlated by one value, after a known point. Tendr does
  not know that `hook.stop` means "turn finished".
- Reading the transcript, extracting results and answering permission prompts
  are the caller's policy.

### omp

omp should be the driver, with Tendr supervising the processes it runs. omp
already owns sessions, subagents, retries and an RPC protocol (`omp --mode rpc`:
newline-delimited JSON with prompt acknowledgement and turn-yield frames).
Running `omp --mode rpc` under a Tendr `--stdin` pipe needs no Tendr change, as
long as Tendr never parses the RPC frames [inference; not run]. A Tendr-backed
task backend inside omp, or an adapter satellite outside both, would duplicate
omp's own session machinery and is not proposed.

## Proposed work, in order

1. **Guide topic.** Add the recipe, trust notes and the comparison table to the
   `docs/guide.md` topic that [agent-hook-routing](agent-hook-routing.md)
   creates. In the same change, rewrite the guide's herdr limit "Run agents in
   herdr panes, not under Tendr": it is right for human-watched agents and
   wrong for program-driven ones.
2. **Exit on the first match.** A generic way for `tendr events --follow` to
   exit after N matching events, with a deadline. Open points:
   - Flag shape: `--max-count N` (or `--until-first`), combined with the
     existing prefix `--kind` / `--source` filters.
   - Correlation: either one `data` field equality in Tendr, or the caller
     filters with `jq`. The latter needs the follower to emit something on
     idle so a downstream `jq` can end; `--cursors` bookmarks every 5 s when
     idle [inference; not run].
   - Starting point: `--from-now` plus `--ready-file`, started before the push,
     is race-free (fact 10); `--since <submit ts>` is the replay alternative.
   - Exit codes: 0 on match; a reserved, documented code for the deadline,
     following the command-scoped exit-code table.
3. **Submission without a sleep.** The text–pause–CR rule is an inference about
   one TUI. The proper fix is the cloud PTY plan's slice 4 mode-aware keys and
   bracketed paste. Do not add a core "submit" verb ahead of that.
4. **Decision spec amendment (owner's call).** Correct the one-shot premise in
   the 2026-04-03 decision, add the boundary row "does not decide when an agent
   turn is done", and link to tender-as-block-runtime. Amend; do not supersede.
5. **herdr.** No Tendr change. Raise an agent kind whose state comes from a
   Tendr session's `hook.*` events with herdr only if the viewer layout gets
   used.

## Non-goals

- Agent-specific code in core: no Claude Code exec target, transcript parser,
  permission policy, or omp RPC parser.
- Screen reading or idle-timer completion.
- Taking over an agent that Tendr did not launch.

## Acceptance criteria

- The guide documents the hook recipe, the submit rule, the permission signals,
  the trust notes and the comparison table, citing the versions measured, and
  no longer says to keep all agents out of Tendr.
- A caller can submit a prompt and block until the matching end-of-turn event
  with one `tendr events` invocation and a deadline, without a background
  follower.
- A test drives a scripted PTY child that emits submit/stop events, covering:
  two turns in one session; a stop for a different id that must not match; a
  `hook.stop_failure` end; an oversize payload (over 16 KiB) whose correlation
  field must still match or fail by a documented rule; the deadline expiring;
  and a follower started after the event that must not match with `--from-now`.
- `tendr wait` documentation says plainly that it waits for process exit, not
  for an agent turn.

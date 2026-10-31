---
id: agent-hook-routing
depends_on: []
---

# Agent hook routing: publish agent hooks into the event log

**Status:** not started. Ready: a docs-only change to `docs/guide.md`.

Goal: add one `tendr guide` topic that shows hook-capable agents how to send
their lifecycle hooks into a session's event log. Analytics, UIs and terminal
adapters can then read those events. The flow is: agent hook → `tendr emit` or
`tendr wrap --event hook.*` → event log → consumers such as Boo, Ghostty, a UI
or analytics.

## Why

`hook.` is an unreserved event kind prefix (see
[event protocol](../../architecture/07-event-protocol.md) §1). It is the
conventional namespace for external lifecycle-hook events from Claude Code, CI
or other agents. What is missing is a short, correct recipe for agent authors.

## Scope

Add a `tendr guide` topic (a heading in `docs/guide.md`, surfaced by `tendr guide <topic>`) —
the `using-tendr` skill is now a thin router to `tendr guide`, not a content home, so guide
topics are where prose lives (the boo section landed there the same way). Use the **shipped**
protocol names/schema only:

1. **The pattern.** A hook fires → the agent runs `tendr emit` (standalone
   event) or `tendr wrap --event hook.<name>` (dual-writes the authoritative
   event + its `output.log` A-line, linked by `event_id`). Events land in
   `~/.tendr/sessions/<ns>/<session>/events/*.jsonl` with the v1 envelope
   (`kind`, `source`, `block_id`, `parent_id`, `data`, UUIDv7 `id`, RFC-3339 `ts`).
   Causal chaining is ambient via `TENDR_BLOCK_ID` / `TENDR_PARENT_EVENT_ID`.

   ```bash
   # from a post-tool-use hook
   tendr emit --kind hook.post_tool_use --data '{"tool":"Bash","exit_code":0}'
   # or, wrapping a supervised command so its output is captured + linked
   tendr wrap --event hook.pre_tool_use -- <cmd>
   ```

2. **One Hermes recipe (example, not an integration plan).** Show mapping a
   Hermes lifecycle hook to `tendr emit --kind hook.*`, as a copy-paste config
   snippet. Verify the current Hermes hook names against its docs before
   writing (the old plan's hook inventory was flagged unverified). No code in
   either codebase.

   A measured Claude Code recipe (hooks → `tendr emit --data-stdin`, with
   submit/blocked/stop events and their correlation by `prompt_id`) is in
   [interactive-agent-turns](interactive-agent-turns.md). Publish it alongside
   the Hermes example.

3. **Consumer note.** State plainly that Boo / Ghostty / a future egui view /
   `tendr query` analytics are **consumers** of `hook.*` events, not core
   Tendr — routing/rendering lives in those adapters (see
   [boo-integration](boo-integration.md) and [decisions](../../decisions.md)).
   Core stays narrow.

## Non-goals

- No changes to tendr product code — this is skill documentation plus one recipe.
- No first-class "agent runtime integration" per external tool; agents publish via the generic `emit`/`wrap` surface.
- No reserved `hook.*` schema — `hook.` stays open vocabulary (event-protocol.md §1).

## Acceptance criteria

- `tendr guide` gains a concise "publishing agent hooks into the event stream" topic (in `docs/guide.md`) using only shipped verbs/fields; the `using-tendr` skill routes agents to it.
- Exactly one worked Hermes recipe, with hook names verified against current Hermes docs.
- The Boo/Ghostty/UI-as-consumer boundary is stated (not implied as core).
- No stale envelope language (`tendr event emit`, `parent_block_id`, ULID, daemon, `schema_version`).

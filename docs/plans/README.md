# Plans

This folder holds open work only. [ROADMAP.md](../ROADMAP.md) is the only index
of plans; a test checks that it lists every plan file here.

## Layout

| Folder | File name | Meaning |
|---|---|---|
| `active/` | `NN_slug.md` | The ordered queue. The number sets priority; `00_` is first. Reorder by renaming. |
| `backlog/` | `slug.md` | Unordered future work. |

The folder is the status. Do not add status tables, summaries or counts here.

## Frontmatter

```yaml
---
id: cloud-pty-control   # stable; never changes
depends_on: []          # ids of other open plans in active/ or backlog/
---
```

No other fields. `id` is the file name without its `NN_` prefix and `.md`. The
heading is the title, and Git holds dates. `depends_on` names only plans that
still exist. Remove an id when that plan ships. The test in `tests/docs.rs`
checks these rules.

## Plan content

Start each plan with a short status line that says what is left, then the goal.
Do not narrate shipped steps; name the PR if a remaining step depends on it.

## Lifecycle

1. New work goes in `active/` with a number, or in `backlog/` without one.
2. A plan moves between `active/` and `backlog/` with `git mv`.
3. A finished plan is deleted in the PR that finishes it. First move its lasting
   facts to their home: `docs/guide.md` (how to use), `docs/architecture/` (how
   it works), `docs/decisions.md` (why), `CHANGELOG.md` (what changed).
4. Update `docs/ROADMAP.md` in the same PR.

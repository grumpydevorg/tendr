# Plans

This folder holds open work only, and it is the only index of plans. What is
out of scope for tendr is in [design](../design.md#scope).

## Layout

| Folder | File name | Meaning |
|---|---|---|
| `active/` | `NN_slug.md` | The ordered queue. The number sets priority; `00_` is first. Reorder by renaming. |
| `backlog/` | `slug.md` | Unordered future work. |

The folder is the status. Do not add an index, status tables, summaries or
counts anywhere else.

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

Other repositories that use this layout also allow a `links:` field. Tendr
does not: links belong in the plan body, where the link checker in CI sees
them.

## Plan content

The line after the title is `**Status:**`, saying what is left, then the goal.
A backlog plan's status also says what would start it. Do not narrate shipped
steps; name the PR if a remaining step depends on it. The test checks that the
status line is there.

## Lifecycle

1. New work goes in `active/` with a number, or in `backlog/` without one.
2. A plan moves between `active/` and `backlog/` with `git mv`.
3. A finished plan is deleted in the PR that finishes it. First move its lasting
   facts to their home: `docs/guide.md` (how to use), `docs/architecture/` (how
   it works), `docs/design.md` (why), `CHANGELOG.md` (what changed).

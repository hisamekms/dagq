---
id: fixture-tasks-left-out
updated: 2026-10-07
note: task 1942 in the frontmatter is not counted
---

# Task numbers that are left out

Inline code is left out: `task 1942` and ``a `task 1943` b``.
A link target is left out: [the ADR](../adr/2026-10-07-t1942-1-design.md) and [x](task 12.md).
ADR IDs have no task word: ADR-t1942-1, adr-t1942-2, t1942.
A count is not a task number: task 3件, tasks 2つ, タスク 5個, task 4本, task 1回.
A word that ends in task is not one: subtask 5, multitask#6.

[ref]: ../plans/task 7.md

```text
task 1942 inside a fenced code block
```

  ```rust
// タスク 8 in an indented fence closed by a longer one
  ````

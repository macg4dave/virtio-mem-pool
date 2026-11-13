---
mode: agent
description: Complete one unblocked roadmap task
---

Task: `${input:task:roadmap task ID or next eligible task}`

1. If unspecified, select the highest-priority unblocked task in the earliest
   incomplete roadmap phase. State its completion criteria and dependencies.
2. Trace affected entry points, callers and tests. Implement one contained
   change meeting those criteria, using the repository rules and validation.
3. Remove the task only when verified complete; otherwise trim it to the
   remaining work. Update phase status if affected and report verification limits.

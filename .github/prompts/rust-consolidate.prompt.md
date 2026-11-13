---
mode: agent
description: Consolidate duplicate Rust code or remove an unused implementation
---

Target: `${input:target:roadmap task ID, responsibility or unused implementation}`

1. Recheck callers, re-exports, trait implementations, CLI dispatch, xtask,
   Make targets, editor tasks and Windows consumers. Public exports can have
   external consumers; absence of local callers is not proof of dead code.
2. Identify contracts and failure behaviour, then choose what to keep, improve,
   combine, delete or keep separate. Confirm feature retirement is within the
   authorised scope when CLI or wire contracts are involved.
3. Move callers and behavioural tests to the canonical implementation; remove
   superseded exports, settings, error variants and dependencies. Explain any
   tests deleted solely because their implementation is gone.
4. Search for remaining references, verify equivalence and update the roadmap.

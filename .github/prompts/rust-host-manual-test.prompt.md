---
mode: agent
description: Prepare and run an authorised bounded host-controller trial
---

Goal: `${input:goal:what the trial should demonstrate}`

1. Read the roadmap's qualification evidence and Development's runner guidance.
   Prepare the config, bounds and recovery plan required by the operational rules.
2. Once the exact trial is authorised, use `cargo xtask host-run` with the
   reviewed config, runtime limit and host-memory floor. Set `observe_only = true`
   when allocation must remain unchanged.
3. Inspect logs, convergence times, actual returned memory and final live state.
   Summarise acceptance evidence in the roadmap and trim the corresponding
   roadmap task. Distinguish interruption from a completed timeout or recovery.

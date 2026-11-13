---
mode: agent
description: Reproduce and fix a Rust bug with a regression test
---

Bug: `${input:bug:observed behaviour, inputs and expected behaviour}`

1. Reproduce the failure beside the owning code and confirm the regression
   test fails for the reported reason.
2. Trace the cause across callers, pending/durable state and restart paths.
   Fix it at the canonical implementation; check sibling paths for the defect.
3. Verify using the repository gate and update any matching roadmap task.

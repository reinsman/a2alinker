---
name: code-review
description: Review repository changes for correctness, regressions and missing validation. Use when asked to review code or inspect a diff.
---

Read `references/checklist.md` before reviewing.

Inspect the relevant code and its callers. Report concrete, actionable problems
introduced by the change, with file locations and the conditions needed to trigger
them. Explain what was checked and any verification limits. If no actionable issue
is found, say so. Do not modify files unless the user requests changes.

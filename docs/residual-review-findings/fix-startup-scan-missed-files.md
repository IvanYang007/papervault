# Residual Review Findings

Source review run: `ce-code-review` (autofix mode) on branch `fix/startup-scan-missed-files`, run id `20260801-151334-c58a312e`, base `bbf795b`.

## Residual Review Findings

- **P2** — `src/indexer/pipeline.rs:130` — Startup scan's metadata fast-path skips files whose Tantivy doc is missing/stale (SQLite-row-without-Tantivy-doc state becomes permanent). Defer failed: filed as https://github.com/IvanYang007/papervault/issues/7 (GitHub Issues).

## Review context

- Reviewers completed: correctness, adversarial (7 mid-tier reviewers failed on provider credit limit: testing, maintainability, project-standards, agent-native, learnings, reliability, performance).
- Advisory findings (report-only, not filed): duplicate-content path ping-pong (P2), scan-loop blocking recv shutdown hang (P2), same-second/same-size change truncation (P3), extraction-failure files silently unindexed (P3).
- Safe-auto fixes applied: none. Test-seeding fix applied as implementation work (commit 075fffe).
- Verdict: Ready to merge; 1 residual actionable finding handed off (issue #7).

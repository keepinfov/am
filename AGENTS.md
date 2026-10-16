# Working On AM

## Read First

1. Read [ARCHITECTURE.md](ARCHITECTURE.md) for the agreed cross-cutting contract.
2. Read [PLAN.md section 13](PLAN.md#13-current-contract-and-agent-handoff) for
   current status, evidence, the next implementation slice and acceptance gates.
3. Read [README.md](README.md) for actual current commands and behavior.
4. Consult [PRODUCT.md](PRODUCT.md) and [MVP.md](MVP.md) for product requirements.

Later explicit user decisions take precedence. ARCHITECTURE resolves the listed
older open choices; PLAN section 13 supersedes conflicting historical iterations,
including section 12. Do not implement obsolete UX just because an earlier section
describes it. Report discrepancies and preserve unrelated worktree changes.

## Development Boundaries

- Stage 1 is offline: one root and a deterministic fake provider, no real tools,
  shell, credentials, network or children. A passed offline test is not provider
  or coding-agent acceptance. Stage 2 may proceed in explicitly tested slices.
- Current ordinary input uses After; `/stop` runs queued work after cancelling the
  selected attempt, or pauses if none is queued. Do not restore removed public
  `/mode`, `/force`, `/queue` or pause commands.
- Runtime owns state and writes; UI sends commands and renders bounded projections.
  Preserve commit-before-ACK, receipt replay, frozen admitted configuration, checked
  cancellation, edit barriers, bounded output and recovery without automatic replay.
- Build modular seams with actual consumers. Do not add fake model names, unused
  registries, placeholder tools, universal traits or schema migrations for future
  stages alone. Profiles/skills/tools ultimately share checked runtime operations.
- Distinguish pinned configuration from current authority. Existing Stage 1 frozen
  fixture quotas are not a complete future hard-host-policy implementation.
- Never claim worktrees, prompt instructions or unrestricted shell are a sandbox.
- Test fixes through production request/poll paths, not only by injecting the state
  that a broken path fails to retrieve. Read operations must remain read-only.

## Verify And Hand Off

Run from the repository root:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 tests/terminal_smoke.py
```

Update PLAN section 13 with actual commands/results, remaining gaps and the next
specific task. Historical pyte/GUI/CI/macOS claims are not fresh evidence. Do not
mark all of Stage 2 complete for one transport mode or invent results not run.

# Contributing To AM

This process applies to people and agents. [AGENTS.md](AGENTS.md) is the short
working agreement, [ARCHITECTURE.md](ARCHITECTURE.md) owns architecture, and
[PLAN section 13](PLAN.md#13-current-contract-and-agent-handoff) owns the current
implementation handoff. Mechanical parameters live in
[.repo-policy.toml](.repo-policy.toml), not independent copies in each script.

## Authorization

An assigned implementation authorizes a working branch, verification, coherent
signed commits, and, once a remote is explicitly agreed, a push of that branch
and a draft PR. It does not authorize creating a public/private remote, changing
repository visibility, merging, tagging, version bumps or releasing packages.

Those actions require separate explicit instructions. Preserve unrelated work,
never force-push or rewrite published history, and do not change Git identity,
signing keys or configuration without permission. Access to a signing key does
not imply permission to publish, merge or release.

The agreed upstream is the public [keepinfov/am](https://github.com/keepinfov/am)
repository. Publishing to another destination or changing visibility still needs
separate authorization. The initial signed Stage 1 snapshot on `master` is the
sole bootstrap exception to branch/PR workflow.

## Branches And Review

- Branch from current `master`, using `type/short-kebab`: lowercase ASCII/digits,
  hyphen-separated words, at most 48 characters. Types are in the policy file.
- Work on a coherent change. Record a plan or issue for architecture changes or
  large multi-subsystem work; an issue is not mandatory for every small fix.
- Writing agents use separate worktrees and clearly assigned files. Shared indexes
  are not a safe way to isolate concurrent authors.
- Keep one or a few useful commits, not permanent WIP/fixup history. Do not amend
  or rewrite history as an unrequested cleanup step.
- Open a draft PR, fill its template and mark it ready only after verification.
  Include every unrun check and its reason. Checkboxes do not prove correctness.
- PRs target `master` and merge with a merge commit, never squash/rebase. A
  conventional merge takes the PR title plus GitHub's `(#N)`, with an empty body.
  Underlying commits carry their explanations and any breaking-change footer.
- Merge newer `master` into published branches rather than rebasing. Never push
  directly to `master`. Delete the merged working branch only with the authorized
  merge operation, not someone else's branch.
- The operator decides when to merge. A mandatory second approving reviewer is
  not required for this single-owner project; green CI is not human approval.

## Commit Contract

```text
type(scope)!: imperative lowercase description
```

The optional `!` means a compatibility break, not a large diff. Scope is required;
types/scopes are enumerated in `.repo-policy.toml`. Add a needed scope through an
explained policy change, not an undocumented exemption.

- Write the subject in English, starting with a lowercase imperative verb, without
  a trailing period. Aim for 72 characters; the hard limit is 95. The PR title
  follows the same contract, before GitHub appends its merge suffix.
- Explain behavior and why it matters, not a list of files. A body is required for
  `feat`, `fix`, `refactor`, `perf`, `test` and `docs`; include useful verification
  evidence where relevant. Explain skipped checks rather than inventing results.
- Use at most two prose paragraphs, wrapped at 80 columns. No bullet lists,
  fenced code blocks, session recaps, conversational replies or attribution.
  Backtick identifiers/paths in prose; write for readers without the original chat.
- Allowed trailers are `Fixes #N` and `Refs #N` for actual issues, and
  `BREAKING CHANGE:` for an explicitly agreed compatibility break. Do not add
  `Co-authored-by`, `Signed-off-by` or generated/AI attribution trailers.
- A nonmerge breaking commit needs both `!` and one explained breaking footer.
  Its PR needs user-facing impact and a `.breaking.md` fragment. A real PR merge
  keeps its body empty and inherits `!`; the underlying commit retains the footer.
- Real branch-synchronization merges may use Git's standard merge subject. An
  ordinary commit cannot bypass the checker by starting its message with `Merge`.
- Every commit must be signed. If signing fails, stop instead of retrying unsigned.
  Signatures establish provenance, not correctness or authorization to merge.

The checker validates structure and selected writing-pattern mistakes. It does
not prove English grammar, useful reasoning, truthful verification, real issue
existence or whether the operator approved an action.

```sh
python3 scripts/repo_policy.py commits --head HEAD
python3 scripts/repo_policy.py commits --base BASE_SHA --head HEAD
```

By default this checks signature armor, not authenticity. To use existing local
Git trust configuration, add `--verify-signatures`; a missing allowed-signers
configuration fails instead of pretending verification succeeded. No script
configures keys or trust automatically. In CI, each added commit must also have
GitHub API verification `verified: true`, `reason: valid`. The upstream `master`
ruleset independently requires verified signed commits; keep this server gate
enabled rather than relying only on a checker within the proposed change.

## Change Size

The PR report compares its head tree to the merge base, not the sum of commit
diffs. It reports additions, deletions, files and binary-file counts, with groups
for source, tests, docs, tooling, generated files and other content.

| Label     | Reviewable Additions + Deletions |
| --------- | -------------------------------: |
| `size/XS` |                             0-49 |
| `size/S`  |                           50-199 |
| `size/M`  |                          200-499 |
| `size/L`  |                          500-999 |
| `size/XL` |                            1000+ |

Only the explicitly listed generated lockfiles leave label churn; their raw totals
remain visible. Tests and documentation count. Renaming an ordinary file to a
lockfile does not hide its diff. Binary changes have no meaningful Git line count
and must still receive review.

Source metrics use `src/**`, including Rust's colocated tests. The report shows
the fraction of base source files modified/deleted, new files separately, and
source churn divided by base source lines. Churn can exceed 100%; this is not
semantic coverage or a claim that exactly that percentage of code was rewritten.
Zero denominators are unavailable, not zero percent. Production code and inline
test lines are not guessed apart by a text heuristic.

L/XL PRs explain **Why Together**, **Risks** and **Review Order** under
`## Large Change`. These requirements encourage reviewable changes, not artificial
splitting. There is no automatic veto based only on size or percentage, and no
`big` marker in branch names. Breaking compatibility is a separate decision.

```sh
python3 scripts/repo_policy.py size --base BASE_SHA --head HEAD --json
python3 scripts/repo_policy.py pr --event EVENT_JSON --base BASE_SHA --head HEAD
```

The PR checker validates real explanations rather than accepting the untouched
template. It cannot judge whether a justification is sound. The label workflow
uses GitHub file metadata; binaries are unknown there, and API diff/rename details
can differ from a local Git report. The local merge-base report is the detailed
review record. Both use trusted, explicit thresholds; label changes do not alter
the mandatory PR-body check.

## Changelog

Declare exactly one `Impact: user-facing` or `Impact: internal` in the PR's
`## Change Impact`, followed by an explanation. User-facing changes need a new or
updated substantive fragment in [changelog.d](changelog.d/README.md). Internal
CI/docs/refactoring without behavior changes can explain why no fragment is needed.

Use separate fragments to avoid parallel edits to a shared Unreleased section.
Releases/version changes are still separately authorized. A checker cannot infer
honest impact from filenames or require a meaningful changelog by word count alone.

## Tool Installation

Application development still needs Rust and the C compiler for bundled SQLite.
The extra tools are development-only; running AM does not require Node or Ruff.

Use Node from [.node-version](.node-version) and Python 3.11+. Exact formatter
versions are pinned by `package.json`/`package-lock.json`, `requirements-dev.txt`
and `tools/actionlint.json`. Install them without npm lifecycle scripts:

```sh
npm ci --ignore-scripts
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements-dev.txt
.venv/bin/python scripts/install_actionlint.py --bin-dir .venv/bin
```

Activate that environment or set `RUFF` and `ACTIONLINT` to its executable paths.
The actionlint installer verifies archive checksums and extracts only the expected
regular binary. Run installers only from a trusted checkout, without elevated
privileges or secrets. Tool/config changes are code changes requiring review.

Markdown uses direct Markdownlint rather than its CLI dependency tree. A scoped
KaTeX override applies a security fix; tooling tests cover that integration.
`npm audit` remains visible and must not be silenced with an unexplained override.

## Verification

There are intentionally no Git hooks or automatic Git-configuration changes.
Use check mode; it does not format or stage files:

```sh
python3 scripts/repo_policy.py files
python3 -m unittest discover -s tests/policy -v
python3 -m unittest discover -s tools -v
python3 scripts/check_style.py
python3 scripts/repo_policy.py commits --base BASE_SHA --head HEAD
```

The stdlib policy tests do not need Rust or installed formatters; tooling tests
also exercise the installed Node packages. Style checks fail for missing tools
or mismatched versions, not pass by skipping them. `files` checks tracked and
nonignored new working-tree files; commit/PR checks use actual Git objects.

Style includes Prettier, Markdownlint, Ruff lint/format, Taplo parse/format and
actionlint. Markdown line-length/duplicate-heading exemptions accommodate the
existing product/iteration documents. Embedded examples are not autoformatted.
Only direct changelog fragments may omit an H1; their README and ordinary
documents retain the heading rule and all other Markdown checks.
Actionlint's optional unpinned ShellCheck/Pyflakes integrations are disabled.

Only an explicit formatting request may write files:

```sh
python3 scripts/check_style.py --fix
```

This applies formatting and Ruff import sorting, not other semantic fixes, and
never runs `git add`. Keep repository-wide normalization separate from behavior.

The application acceptance gate remains the commands under
[AGENTS.md verification](AGENTS.md#verify-and-hand-off). Record actual results and
remaining platform/manual gaps in PLAN section 13; do not replace full acceptance
with a fast metadata check.

## CI And Server Rules

`CI` runs on every PR update, including metadata edits, and pushes to `master`.
There are no path filters that leave required checks pending. Policy, style and
rustfmt are early checks; the Linux/macOS application matrix waits for all three.
The final `required` job always reports and succeeds only when all gates succeed.
Superseded PR runs are cancelled; `master` runs are not cancelled automatically.

The separate privileged size-label workflow checks out only a trusted default-
branch base revision, never PR-head code or PR artifacts. It uses API metadata
and narrow label-write permissions, preserves unrelated labels and rejects stale
or truncated file metadata. GitHub does not offer atomic head-conditional label
writes, so a small check/write race is possible; the next event reconciles it.
The first policy PR cannot run the labeler until trusted base code exists.

The agreed upstream `master` ruleset is active with these settings:

- Default branch `master`, PR-required changes, verified signed commits, blocked
  deletion/non-fast-forward updates, and required `required` CI status.
- Merge commits only, PR-title merge subjects, empty merge bodies, and deletion
  of the authorized merged branch. No automatic merge or implicit release rights.
- No mandatory second reviewer for the current single-owner workflow. Operator
  review remains essential, especially for policy/workflow or executable changes.
- Explicit bypass policy; never mistake a prose rule or editable CI file for an
  independent server-side security boundary.

The ruleset is `Protect master` (ID `25301183`) with no bypass actors. Its
`required` status is bound to the GitHub Actions integration. Recheck live settings
when changing policy; tracked prose alone is not server enforcement.

The initial policy was merged by explicit authorization in
[PR #1](https://github.com/keepinfov/am/pull/1). Its size label was applied manually
because the trusted base did not yet contain the label workflow. That workflow is
now available on `master`; verify its actual runs on subsequent PRs rather than
assuming local tests prove live labeling. Actual CI evidence and remaining gaps
are recorded in PLAN section 13.

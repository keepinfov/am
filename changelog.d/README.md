# Changelog Fragments

For a user-facing change, add a short Markdown fragment named
`<slug>.<kind>.md`. Use lowercase ASCII words/digits separated by hyphens for the
slug; supported kinds are `added`, `changed`, `fixed`, `removed`, `security` and
`breaking`, as defined in [.repo-policy.toml](../.repo-policy.toml).

For example, `history-action-refresh.fixed.md` describes the observable correction
and relevant limitations in ordinary prose. Do not copy commit subjects, use an
empty template or claim verification that was not run. A breaking fragment also
explains what breaks and how a user should adapt.

The PR must declare `Impact: user-facing` and include a new or changed fragment.
Internal CI/docs/refactoring changes without user behavior changes can instead
declare `Impact: internal` and explain the choice in the PR. The checker validates
format and the explicit declaration; review decides whether that choice is honest.

Keep fragments independent so parallel branches do not all edit one changelog
insertion point. Release collation, version changes and tags require a separate
operator request; no automatic release action is introduced here.

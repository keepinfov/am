# DeLM Research Notes for AM

Research date: 2026-10-09. Scope: read-only investigation, not an integration proposal approved for implementation.

## Conclusion

**Useful as a reference for future multi-agent context and artifact exchange, not as a dependency for AM's current foundation.** DeLM is not a terminal UI, command palette, shell autocomplete engine, or a generic OpenAI-compatible model client. Its current public implementation is a Python/Docker research harness coordinating independent Codex or Claude Code workers through a shared board.

AM should retain its Rust/Tokio runtime and ratatui client. Consider selected coordination ideas when delegation and shared materials become real requirements (PLAN stage 4), not during the current single-agent foundation or the initial provider implementation (stage 2).

## Sources and Provenance

- Canonical public repository: [yuzhenmao/DeLM](https://github.com/yuzhenmao/DeLM). The requested lowercase `delm` resolves to this repository.
- Inspected `main` HEAD: [`18ed1c0d188a38b4af8556faaef9c99f0d9f083a`](https://github.com/yuzhenmao/DeLM/commit/18ed1c0d188a38b4af8556faaef9c99f0d9f083a), committed 2026-10-07, message: "Import unified DeLM runtime and update project README".
- Public data retrieved with read-only `gh api` GET requests and `webfetch`: repository metadata, recursive tree, recent commit metadata, tags, releases, README, license, and relevant source files/excerpts. Source links below are pinned to the inspected commit; the README was retrieved from `main` during the same investigation.
- The GitHub HTML fetch failed, and a few API calls had transient TLS errors. Raw GitHub content and subsequent API requests succeeded. Findings are based on successful responses, not inferred from failed requests.
- Local comparison used AM's `README.md`, `Cargo.toml`, and relevant sections of `PLAN.md`. Existing worktree files were not changed.
- No clone, dependency installation, Docker build, agent execution, model request, or benchmark reproduction was performed. No local credential files or chat history were read. No credential values are included here.
- Only this research document was added. This is a source inspection, not a security audit or a verified compatibility test. The separate agent-plugin repository and upstream Humanize implementation were not inspected.

## Confirmed Repository Facts

### Actual Function

Sources: [README](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/README.md), [launcher](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/delm.py), [runner](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/runner.py), [two-worker prompt](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/prompts/prompts_n2.py).

- The launcher supports exactly two or four workers and the `codex` or `claude-code` harness. Each worker has its own task container/workspace and is expected to submit a complete solution, not merely return a delegated subtask to a root agent.
- Workers coordinate asynchronously using a common task queue and append-only shared context. They publish tasks, claim/reserve scopes, mark work done, and publish findings or failed approaches.
- Coordination is decentralized at the agent-planning level: peers can publish and claim work without a permanent central planner. Storage is not a distributed peer-to-peer system: one runner hosts the shared SQLite-backed HTTP board. The two-worker prompt still assigns an initial queue handoff to the first worker.
- The repository includes task fetching, Docker image building, worker launch, trajectory collection, grading, and Markdown reporting. Its README quickstart is oriented around Terminal-Bench tasks.
- The README links a paper, published trajectories, and a separate Codex/Claude Code plugin. Those links do not establish a reusable Rust API in this repository.
- Reported accuracy/latency gains are author-reported benchmark results, not independently checked here. Terminal-Bench and DeepSWE tables use selected ten-task subsets and three runs; accuracy averages independently graded submissions, rather than best-of-n success. Changing worker count also changes role prompts. These results do not establish a speedup or quality gain for AM.

### Shared Context and Materials

Sources: [shared_context.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/board/shared_context.py), [protected_board.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/board/protected_board.py), [context_client.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/board/context_client.py).

- The board stores events in `context.sqlite` and attached file bodies/publications in an attached `context-bodies.sqlite` database. `connect()` uses `BEGIN IMMEDIATE`; queue ownership is derived from logged events. Claims/reservations are transactional and each worker may hold only one unfinished claim.
- Notes have type `FACT` or `FAIL` and require text, scope, and basis. These fields request evidence from the model; the board does not independently verify whether a claimed fact is true.
- An initial read returns a full view. Later responses include new/changed entries and compact previously seen active work, using that actor's last served read as a cursor. `read --all` retrieves older entries again. Read views and file access/application decisions are logged.
- Waiting reads poll every 0.25 seconds and cap each wait at 25 seconds. Peer publications or session endings can wake them; status-only changes do not. The waiting shell command is bounded, but it is still a worker/tool operation, not Tokio-native runtime scheduling.
- File bodies stay out of normal board views; workers explicitly expand or apply selected publications. This is structured, tool-mediated context exchange, not shared model weights, shared KV cache, embedding retrieval, or automatic injection of every peer transcript.
- Attachments are UTF-8 text, limited to 40 files, 200,000 bytes per file, and 400,000 bytes per bundle. They may be workspace-relative or under `/tmp`. The client rejects selected credential/cache/record paths and symlink attachments.
- The runner uses `ProtectedBoard`, whose apply policy is stricter than the base `SharedContext.apply`: it permits original/absent files or eligible versions from the incoming author, but rejects local edits and cross-author versions requiring reconciliation. Any detected conflict refuses the selected batch; there is no automatic merge.
- `done` and `note` publications support request-ID replay with a content fingerprint; reusing an ID for different content is rejected. The client derives request IDs for publication retries from payload hashes; the board assigns separate publication IDs. This is not a general exactly-once guarantee for all commands or filesystem effects.

### Language, Dependencies, and Maturity

Sources: [repository tree](https://api.github.com/repos/yuzhenmao/DeLM/git/trees/18ed1c0d188a38b4af8556faaef9c99f0d9f083a?recursive=1), [runtime config](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/config.py), [Codex Dockerfile](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/Dockerfile.codex), [Claude Dockerfile](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/Dockerfile.claude).

- Python is the primary language; the board itself uses standard-library SQLite, HTTP, threading, and filesystem facilities. The inspected tree contains no Rust implementation.
- README/launcher require Python 3.13+ for live host runs, Linux Docker Engine with BuildKit, Git, and Docker Compose for service tasks. Worker Dockerfiles use Python 3.12 and relocate the runtime into task images.
- Worker dependencies include Codex or Claude Code installed through npm, the Humanize `hmz` driver installed from a pinned Git commit, and its dependencies (the Dockerfiles explicitly check `pydantic`). There are also Node/npm, certificates, and native runtime-library requirements. This is not a dependency-free board package merely because board imports use the standard library.
- Inspected pins: Codex `0.153.4`, Claude Code `2.1.280`, Humanize commit `413d02e44d0cc0514b9f5bd3fcefea156b047a49`. Codex's Dockerfile applies a local patch to Humanize. Driver provenance and CLI versions are checked at runtime; Docker base images and transitive package dependencies are not all pinned by immutable digests/lockfiles in the inspected tree.
- Repository metadata reports creation on 2026-06-07 and a recent runtime import on 2026-10-07. Tags and releases API responses were empty. The inspected tree has no `pyproject.toml`, requirements/lockfile, test directory, or `.github` CI workflow. Runtime validations and reporting code do exist. This supports treating it as a young research implementation, not evidence that it has no tests anywhere or cannot work reliably.
- No stable public library/SDK contract was found in the README or inspected source. Its Python modules are coupled to the repository's runner, container paths, and benchmark records.

### License

Source: [LICENSE](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/LICENSE).

The actual file contains the standard **MIT License**, with a copyright notice for Yuzhen Mao and Azalia Mirhoseini. GitHub metadata labels it `Other` / `NOASSERTION`; the license text is the stronger evidence here. Reuse of substantial code must preserve its copyright and permission notice. This does not determine licenses or service terms for Humanize, bundled CLIs, downloaded tasks, datasets, or model providers; those would need separate review before adoption.

### API and Protocol Coupling

Sources: [codex_api.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/codex_api.py), [native_worker.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/native_worker.py), [claude_api.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/claude_api.py), [containers.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/containers.py), [native_network.py](https://github.com/yuzhenmao/DeLM/blob/18ed1c0d188a38b4af8556faaef9c99f0d9f083a/runtime/native_network.py).

- Despite the `*_api.py` filenames, these are native CLI bootstrap/session/audit adapters, not a standalone Chat Completions transport. Codex is accessed via Humanize and its app-server calls, including `account/read` and `skills/list`; Claude uses Humanize's CLI command and streaming-event protocol.
- Codex configuration and record validation require the `openai` provider; Claude subscription checks require first-party authentication. Both adapters strip inherited provider environment variables and reject inherited settings. Selected model IDs can be overridden, but reasoning effort is fixed to `xhigh`, with no model fallback.
- The launcher exposes no arbitrary base URL, custom request headers, no-auth local-server mode, or proxy configuration. The inspected adapters do not implement AM's required HTTP/HTTPS/CONNECT/SOCKS5, TLS, timeout, redirect, and auth-isolation matrix. Underlying CLIs may have other capabilities, but DeLM does not demonstrate or expose them as an AM-compatible transport contract.
- `native_network.py` reads Codex diagnostic SQLite logs after a run; its name does not mean it provides a general networking layer.
- Agents invoke `sh /tools/context.sh` through their existing shell tool. The Python client sends JSON POST requests to the board, which maps a per-worker token in the URL path to the actor. The runner generates random tokens and starts a threaded HTTP server reachable through the Docker host/bridge. This board protocol is separate from any model-provider protocol; it is not Chat Completions or MCP.
- The inspected code offers no ratatui integration, interactive command palette, or autocomplete facility. `argparse` launch options and model-invoked board shell commands are not terminal input completion.

## Engineering Assessment for AM

The following are recommendations and risks inferred from the confirmed implementation, not promises made by DeLM.

| AM Area                               | Assessment                                                                                        | Recommended Use                                                                         |
| ------------------------------------- | ------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| Current single-agent foundation       | Little immediate benefit; DeLM adds a different execution stack and task lifecycle                | Do not integrate or add a Python/Docker sidecar now                                     |
| Command UI / autocomplete             | Not the problem this project solves                                                               | Keep input completion and UI command design separate                                    |
| OpenAI-compatible endpoints / proxies | Poor direct fit; native CLI/provider/audit coupling                                               | Implement AM's planned provider transport independently                                 |
| Future delegation / context           | Relevant ideas, but peer complete-solution workers differ from AM's root/child ownership          | Borrow selective publication, evidence, and incremental-view concepts only when needed  |
| Shared files / materials              | Explicit publication IDs, bounded bundles, provenance, and conflict refusal are useful references | Design within AM's trusted runtime authorization and artifact model                     |
| Durable state / recovery              | SQLite/logging is familiar, but contracts differ                                                  | Preserve AM's existing ACK, cancellation, bounded-buffer, and no-auto-replay guarantees |

### Pitfalls to Avoid

- **Do not equate a served view with model consumption.** DeLM advances the cursor when it serves/logs a response, not after a model acknowledgement. A lost response can require `read --all`. AM should explicitly define transport delivery, prompt inclusion, and durable visibility rather than silently conflating them.
- **Do not copy unbounded history behavior.** The board reconstructs queue/views from log history and stores served views in read events. No overall log quota or pagination is visible in this code. Bundle limits do not bound total history, response size, or reconstruction work; AM already requires bounded storage/projections.
- **Do not expose the board HTTP endpoint as a general service unchanged.** Tokens are in URL paths, transport is plain HTTP, and `do_POST` reads the declared content length without an explicit request-size cap before JSON parsing. Built-in request logging is suppressed, but intermediaries/error handling can still expose URLs. A hostile-client boundary would need separate authentication, transport, body limits, and resource controls.
- **Do not mistake a file-apply grant for durable filesystem completion.** The client hashes local files, obtains a server decision, then performs multiple replacements with best-effort rollback on caught errors and reports the result. That is not a crash-atomic multi-file transaction and does not eliminate intervening local-write races. AM's executor/artifact contracts need their own failure and concurrency model.
- **Do not treat prompt rules or client filename exclusions as authorization.** FACT labels are model assertions, and filtering selected credential filenames cannot detect every secret in arbitrary content. AM must authorize tool dispatch and material access in trusted runtime code; shared content remains untrusted input.
- **Do not port the coordination policy wholesale.** DeLM limits worker count, gives each worker a full solution submission, and even forbids certain review/testing-only queue assignments in its prompt. AM plans root/child ownership, branch budgets, distinct permissions, cancellation, and controlled material access. These are different product semantics.
- **Budget parallelism, not just latency.** README allocates the official task's full resources per worker and reserves another 4 GiB of host RAM. Faster wall time can still mean more aggregate model/tool compute and operational complexity. Benchmark gains must be validated on representative AM work before changing defaults.
- **Do not assume identical SQLite durability settings.** DeLM's inspected connection code does not explicitly set AM's WAL/`synchronous=FULL` policy. Its use of attached databases should be evaluated against the chosen journal mode and crash contract before any analogous design is adopted.

### Minimal Future Takeaways

If stage 4 later warrants a focused design experiment, the most relevant ideas are immutable evidence-bearing publications, explicit artifact references with opt-in expansion, per-agent incremental context views, and conflict refusal instead of automatic file merging. AM should implement only the needed subset inside its existing runtime, keep the UI a command/snapshot client, and leave provider transport independent of coordination.

No adoption, dependency installation, or code integration is recommended as part of this research action.

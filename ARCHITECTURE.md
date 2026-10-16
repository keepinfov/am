# AM Architecture Contract

Status: agreed design, recorded 2026-10-16. This is not an implementation or
acceptance report. Current code is the offline Stage 1 foundation; see
[PLAN.md section 13](PLAN.md#13-current-contract-and-agent-handoff).

## 1. Authority And Scope

The user selected these decisions in three architecture question rounds. This
contract resolves the corresponding open choices in [PRODUCT.md](PRODUCT.md)
and [MVP.md](MVP.md). Those documents retain product requirements; this document
owns the cross-cutting architecture. PLAN owns sequencing and verification;
README describes implemented behavior. Later explicit user decisions supersede
these documents. Report discrepancies rather than silently treating old examples
or proposed interfaces as implemented requirements.

Design the seams now; implement only mechanisms needed by the current slice.
Neither a stable public SDK nor a universal plugin platform is required.

## 2. Runtime Boundaries

```text
Terminal / Headless / future Web
              |
       commands and views
              |
Runtime: agents / attempts / policies / budgets / ownership / events
       |              |              |                |
   Providers        Tools         Materials       Workflow (future)
                      |
                  Executors
             local / sandbox / remote

Component catalog -> validated, resolved definitions -> Runtime
```

- Root and children use the same agent mechanism. Root is not an authority bypass.
- Runtime owns state, scheduling, authorization, cancellation and durable records.
- Providers translate requests/responses; they do not own SQLite, UI, policy or
  team coordination. The agent runner owns the model/tool loop.
- Tools receive trusted invocation context from runtime, outside model arguments.
  Neither an `agent_id` argument nor possession of a profile name grants authority.
- Extensions and workflow use checked runtime operations, not an unrestricted
  operator client. All executions share ownership, limits and cancellation.
- These are logical boundaries, not a requirement for daemons or microservices.

## 3. Components And Composition

| Kind             | Responsibility                                                |
| ---------------- | ------------------------------------------------------------- |
| Connection       | Endpoint, protocol, network settings, credential references   |
| Model preset     | Connection reference, opaque model ID, request parameters     |
| Role             | Purpose and instructions                                      |
| Skill            | Instructions, reference material and associated resources     |
| Tool             | Input/result schemas, backend and execution requirements      |
| Execution preset | Executor, shell, environment, workspace, required guarantees  |
| Policy preset    | Rights, approvals, expansion ceiling and delegation authority |
| Budget preset    | Resource and concurrency constraints                          |
| Agent profile    | Explicit composition of the selected components               |
| Workflow         | Future event/condition/action definitions                     |

- Structured definitions use TOML; instructions use Markdown, with resources and
  scripts alongside them. A skill can use a `SKILL.md` entry point.
- Profiles refer to reusable components explicitly. No multiple inheritance or
  implicit recursive deep merge. Overrides have field-specific semantics.
- Missing references, cycles and ambiguous references fail validation. Missing,
  empty and explicitly unrestricted values must not share ambiguous meanings.
- Namespaces distinguish `builtin/name`, `user/name` and `project/name`, within a
  component kind. Short names require an unambiguous explicit binding; a project
  must not silently shadow a user or built-in definition.
- Lookup location and authorship are provenance, not proof of trust. Project
  definitions do not grant credentials or widen host authority.
- Loading/validating definitions does not run shell commands, fetch dependencies
  or evaluate executable configuration templates. Third-party formats may use
  adapters; the kernel is not defined by one external format.

## 4. Definitions, Revisions And Activation

Keep schema version, component identity, content revision and effective instance
revision distinct. Resolve the dependency closure and pin its revisions/content,
not only mutable names or filesystem paths. The concrete manifest schema and
revision encoding are chosen with the first real consumers.

```text
author -> validate -> authorize activation -> activate revision -> use
```

- Editing a definition does not mutate an existing agent or admitted assignment.
- New versions are applied explicitly at a supported safe boundary. Record the
  effective values, their sources and application time.
- Queued work keeps its admitted configuration. Current Stage 1 accumulation/edit
  behavior preserves that snapshot; do not resolve names against newer definitions
  when the work eventually starts.
- Preserve the executable content associated with an activated revision or reject
  changed/missing content. Do not approve one script and silently execute another.
- Pinning definitions is not hermetic execution or deterministic model output.
  Installed interpreters, remote APIs and external inputs require separate records
  and guarantees appropriate to the executor.
- A model may author profiles, skills, tool programs and later workflows. Policy
  may permit validation/activation/use automatically in preauthorized scopes.
  Human review is not mandatory for every file or revision.
- Authoring permission does not itself permit activation, execution or a rights
  increase. A skill does not execute bundled code merely by being read.
- Agents may edit and test AM source as a project. Replacing the running kernel
  remains a separate operator action, not an ordinary self-activated extension.
- Imported packages are inert until validated and activated through this path.
  Knowledge import does not mutate roles, configuration or grants.

## 5. Roles, Policy And Approvals

Ship ordinary role presets `Plan`, `Build`, `Review`, `Research`, independently of
approval presets `Ask`, `Auto`, `Full access`. These are composable definitions,
not a closed enum of special agent implementations.

The intended ordinary default is `Build + Ask`: permitted workspace reads are
automatic; writes, shell and new external accesses require approval unless policy
already authorizes them. Configured model API access is separately authorized
when connecting. A Plan role uses a restricted toolset; prompt instructions or
hidden tool schemas do not enforce read-only behavior.

- Check every applicable mandatory host, ownership-chain, agent, resource and
  executor constraint. Within policy, `deny` takes precedence over `ask`, then
  `allow`; ordinary allows cannot override applicable prohibitions.
- No current authority means no execution. Asking for expansion is itself allowed
  only within a separately defined requestable ceiling.
- Distinguish current rights, allowable expansion and delegable authority. A parent
  can issue rights only within its delegable authority, which is not necessarily
  identical to its current working set of rights.
- Runtime verifies the issuer of a grant. A model decision alone is not a grant.
- Exceptions require an authorized policy/grant change, not a hidden ordering
  trick. Prohibited changes cannot be laundered through roles, children or tools.
- Full access automates actions within explicitly granted authority. It never
  disables runtime checks or removes hard host limits/operator-only boundaries.
- One-shot approval is the default. Session grants are explicit, scoped and do
  not automatically reactivate after restart. Persistent permission is a separate
  authorized policy change.
- Bind approval to the subject and actual action: tool revision, arguments, cwd,
  relevant environment, executor and scope. Revalidate authority and the approved
  action before execution. Changes invalidate the old approval as applicable.
- Frozen configuration is not frozen authority. Revocation immediately blocks new
  affected dispatches and initiates cancellation of affected active executions.
  Report actual stop/unknown state; do not promise rollback of previous effects.
- Approval, notification and a recorded blocker are separate. No-question mode
  cannot silently approve work. Wait for events, do independent permitted work or
  finish blocked according to policy, without model polling loops.

Basic dispatch authorization and approvals belong with Stage 3 tools. Stage 5
expands escalation and management; it must not introduce security for the first
time after tools/children already exist.

## 6. Tools, Executors And Services

- First custom backend: managed scripts/programs, with a TOML descriptor and
  structured JSON input/results. Specify bounded framing, output handling and
  result validation before implementation. Do not parse unbounded stdout as JSON.
- Built-ins and custom tools use the same checked invocation/result boundary.
  MCP is a subsequent backend of that boundary, not a second agent runtime.
- Distinguish tool invocation from underlying execution. Record IDs, owner,
  attempt, executable/argv/cwd/environment, deadlines, state and results.
- Read output continuously into bounded projections and quota-bounded artifacts.
  Keep raw bytes separate from terminal-safe rendering.
- Cancellation is requested stop, grace deadline, forced stop where supported,
  exit/reconciliation and output drain. Requesting cancellation is not proof that
  a process or remote operation stopped.
- Support an explicit lifetime distinction between attempt-owned execution and
  managed agent/session services. Services may outlive an answer only when
  authorized and retain an owner, execution ID, logs, limits and stop controls.
  Full subtree stop includes its owned services, not unrelated services.
- This contract does not imply immediate PTY/persistent-shell support or process
  reattachment after a runtime crash. Surviving execution is reconciled honestly.
- Executors declare actual isolation/resource capabilities. An unavailable strict
  requirement rejects launch unless the operator explicitly chooses weaker mode.
- Unrestricted local shell is not sandbox/read-only. Tool-declared side effects,
  filename filters, worktrees and WASM wrappers cannot establish OS isolation.

## 7. Agents, Context And Workspaces

- Profile, agent, conversation, assignment attempt, provider request, tool
  invocation, execution and future durable task have distinct identities.
- One active attempt per agent; incoming assignments are FIFO. A real attempt may
  include multiple model requests and tools. Parallel assignments use participants
  with separate context, not concurrent mutation of one agent conversation.
- Tool parallelism is separately bounded and authorized with access/conflict
  handling. A tool's self-declared purity is not sufficient for safe concurrency.
- Support fresh attempts and persistent workers. Children receive an assignment,
  instructions and selected material access, not automatic parent-chat copies.
- Full durable history and bounded prompt context are separate. Reading a material
  does not prove prompt inclusion or model consumption. Preserve provenance and
  distinguish read, edit, delete and share authority.
- On context overflow, ordinary mode stops with an explanation and offers explicit
  selection/compaction. Profiles may preauthorize automatic compaction, with visible
  events and budget accounting. Preserve source material within storage quotas.
- No hidden truncation, automatic rescue agent or mandatory summary of a child's
  poor report. Parents inspect available materials with ordinary tools.
- Ownership is a tree for authority, budgets and stopping. Tasks, messages and
  reviews may form a graph across authorized branches. Task dependencies do not
  redefine ownership or stop scope; no mandatory central planner prompt is needed.
- Parallel coding workers default to separate Git worktrees, or separate directories
  outside Git. Shared workspace remains selectable. Transfers are explicit and
  conflicts are reported rather than hidden by automatic merges.
- Spawn does not duplicate a branch budget. Account shared usage/concurrency at
  applicable host/session/branch/agent scopes. Unavailable measurements are not zero.

## 8. Providers And Secrets

- Initially Chat Completions; keep transport independent of context and team
  coordination. Normalize text, tool-call assembly, completion, usage/errors and
  measured transport phases. Incomplete calls never execute.
- Treat model IDs as opaque. Validate explicit capabilities/parameters, rather than
  infer authority or support from a model name. No silent model/endpoint fallback.
- First credential sources are explicitly selected environment variables or private
  files outside the project. Definitions, snapshots, history and exported packages
  contain references, not secret values. Keyring/account login can extend this seam.
- Secrets do not automatically enter prompts, logs or tool environments. Passing
  credentials to a tool requires separate authority. Unrestricted unsandboxed shell
  cannot promise protection against reading host secret files.
- Define routing explicitly: HTTP/HTTPS, proxy protocol, DNS, bypass, CA/TLS,
  redirects and timeout/retry behavior. Required proxy never silently falls back
  to direct traffic. Tool environment proxy settings are not network isolation.
- Choosing an HTTP library does not prove the required proxy matrix. No automatic
  replay of side effects follows a model-transport retry.
- Status shows observed phase/time/last event. Only actual reasoning events justify
  reasoning status; silence and animation do not prove model progress.

## 9. Clients, Durability And Workflow

- Terminal, headless and future web use common semantic runtime operations, not
  TUI slash-command parsing as a universal protocol.
- Clients have separate drafts/focus/view state; agent queues are shared durable
  state. Multi-client mutation is serialized and editing uses version/lease checks.
  Client-scoped input can be stored by runtime without becoming shared agent input.
- Disconnect is not stop. Preserve current local `/quit` behavior until an explicit
  client/runtime-lifetime change is implemented.
- Separate cancel attempt, pause dequeue, stop ownership subtree, close client and
  shut down runtime. Current After and `/stop` semantics are in PLAN section 13;
  do not restore obsolete public `/mode`, `/force` or queue commands.
- Critical changes commit before ACK. Command retries are identity/content checked;
  callers are authorized independently of IDs. No exactly-once promise is made for
  external effects across crashes.
- Structured events have session/agent/attempt/execution identity as applicable,
  type, ordering cursor, time and bounded nonsecret payload. Live coalescing must
  not erase durable state transitions. Support snapshot/cursor recovery.
- SQLite metadata and artifact files are not one atomic filesystem transaction.
  Retain explicit partial/incomplete/uncertain states, quotas and recovery rules.
- Future workflow uses declarative event/condition/action rules, checked runtime
  operations, versioned results/reviews, bounded repetitions and cycle protection.
  Arbitrary code belongs in an ordinary tool, not inside the workflow kernel.
- A task may have several attempts/participants. Technical attempt completion does
  not automatically mean task success or acceptance of a result.

## 10. Delivery And Architectural Acceptance

| Stage | First relevant mechanisms                                                                      |
| ----- | ---------------------------------------------------------------------------------------------- |
| 1     | Offline kernel and bounded clients; no fake extension/profile implementation                   |
| 2     | Concrete connection/model definitions, credentials, provider and context                       |
| 3     | Checked tools, script backend, execution presets, basic policy/approvals and service lifecycle |
| 4     | Composed agent profiles, skills, delegation, workspaces and branch budgets                     |
| 5     | Rich escalation, unattended management, revocation and OS guarantee expansion                  |
| 6+    | Knowledge lifecycle/import; MCP, workflow/harness and web as scoped follow-ups                 |

Live authority checks and cancellation hooks start with the relevant privileged
operation, not only in Stage 5. Concrete scripts, manifests and process protocols
must have tests when introduced; no unused placeholders are required today.

Acceptance cases to add with their mechanisms:

- A permitted model-authored tool works without rebuilding AM; an unpermitted one
  cannot activate or bypass dispatch through a child or alternate backend.
- Namespace collisions fail instead of substituting project content silently.
- Editing a definition does not relabel history or alter pinned active/admitted work.
- Changed executable content cannot reuse an approval for the old revision.
- Revocation blocks queued dispatch and initiates affected active cancellation.
- Approval replay does not duplicate effects or revive expired/session authority.
- Spawn/parallel tools cannot multiply branch budgets or escape ownership stop.
- An authorized service outlives an answer, stays observable and stops with its owner.
- Context compaction is policy-controlled, visible and accounted for.
- Crash recovery never automatically reruns an uncertain external side effect.
- Two clients cannot overwrite each other's draft or stale leased edit.
- A workflow/web client uses existing operations without a second execution system.

Concrete crates, field schemas, numerical defaults, process framing and OS
mechanisms are implementation choices constrained by this contract. Record them
before the corresponding implementation; do not reopen the architecture silently.

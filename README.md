# AM

The terminal greets you as **am nyam**; the executable remains `am`.

A Unix-style agent runtime built in stages. The target product is documented in
[PRODUCT.md](PRODUCT.md), its MVP boundary in [MVP.md](MVP.md), and the roadmap
and current stage in [PLAN.md](PLAN.md#13-current-contract-and-agent-handoff).
[ARCHITECTURE.md](ARCHITECTURE.md) records the agreed modular profiles/extension
contract; [AGENTS.md](AGENTS.md) explains where subsequent agents should continue.
Future design is not a claim of implemented features.

## Current Stage

Stage 1 is an offline foundation, **not a real coding agent**. It has one root
participant, a durable message queue, a deterministic fake provider, saved
history, cancellation, recovery, durable drafts/composer, configurable command
chains, an inline ratatui conversation and a fullscreen conversation/action journal.
There are no network requests, credentials, shell commands, file tools, or child
agents. Those belong to subsequent stages.

## Run

Requires Rust with edition 2024 support, a C compiler for bundled SQLite, and a
terminal for interactive mode. The lockfile fixes the current dependencies.

```sh
cargo run --locked -- --config examples/stage1.toml
```

The default session is `session.sqlite3` in the platform's user data directory
(`~/.local/share/am` on a typical Linux installation). Override it explicitly:

```sh
cargo run --locked -- --session ./am.sqlite3
cargo run --locked -- --headless --prompt "stage 1 smoke" --session ./am.sqlite3
```

The parent directory of an explicit session path must exist. Reopening a session
always starts paused. In the interactive client, sending nonempty text or using
`/send <draft-id>` resumes work; empty Enter does not. `--headless --prompt`
is an explicit instruction to resume and run the saved queue followed by the new
message; it prints a bounded saved-history page as JSON. It has a 120-second
overall fixture deadline. Merely opening a database never resumes work.

`--config FILE` explicitly loads runtime, provider, key-binding and interface TOML.
Global UI defaults are also loaded from `preferences.toml` in the platform's user
config directory (`~/.config/am/preferences.toml` on a typical Linux installation).
Use `--preferences FILE` to override that path, including for isolated headless
fixtures. A missing preferences file means built-in defaults; a present invalid
or unreadable file fails startup rather than being ignored. Config and preferences
must be separate files.

Interface precedence is **built-ins < user preferences < explicit TOML**, per
field: only fields actually present in `--config` override saved preferences.
Project config files and environment variables are not silently loaded as AM
settings; platform directory resolution can follow the OS environment. Unknown
fields and out-of-range values fail before starting the runtime. See
[examples/stage1.toml](examples/stage1.toml). Fixture scenarios are `normal`,
`waiting`, `interrupted`, `error`, and `burst`.

## Commands And Controls

Commands, completion descriptions, keyboard actions and `/help` share one catalog.
Only implemented features are listed; there are no placeholder model/login tools.
Local outcomes are saved as short categorized records. The command field, drafts,
stop-only controls and journal below are the current contract; previous iterations
and their historical verification remain in [PLAN.md](PLAN.md).

| Command                                              | Action                                                                                        | Default Shortcut                                  |
| ---------------------------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| `/commands [keep]`                                   | Secondary command field; `keep` pins it beside the retained primary composer                  | Ctrl+P                                            |
| `/exit`                                              | Close the command field, not the application                                                  | None                                              |
| `/help`                                              | Interactive command catalog with categories, search and effective shortcuts                   | None                                              |
| `/history [fullscreen\|<seq>]`                       | Fullscreen list/preview; sequence anchor, or type a fragment and Tab to search saved dialogue | Ctrl+O toggles reader                             |
| `/settings [<key> <value> [--default]]`              | Open UI preferences, or change one value and optionally save its global default               | None                                              |
| `/details`                                           | Runtime IDs, event cursor, response checkpoints                                               | Alt+D                                             |
| `/stop`                                              | Cancel selected active work; run queued input FIFO, or pause if none is queued                | Esc twice within 700 ms in the plain primary root |
| `/quit`                                              | Request exit; finish pending saves/notices, then shut down                                    | Ctrl+C twice within 3 seconds                     |
| `/clear-draft`                                       | Clear the unsent draft                                                                        | Ctrl+U                                            |
| `/draft save`                                        | Save primary text under a generated draft ID, from the secondary field                        | None                                              |
| `/draft delete <id-prefix>`                          | Delete a saved draft after ID/revision validation                                             | Delete in drafts                                  |
| `/drafts`                                            | List saved drafts; Enter loads a copy                                                         | None                                              |
| `/send <draft-id>`                                   | Atomically submit and consume a saved draft, resuming if paused                               | None                                              |
| `/bindings`                                          | Visual command-shortcut editor                                                                | None                                              |
| `/bind <key> /command [ ; /command ...] [--default]` | Bind a chord to a validated command chain                                                     | None                                              |
| `/unbind <key> [--default]`                          | Remove a command shortcut                                                                     | None                                              |

The public catalog has no `/queue`, `/pause`, `/toggle-pause`, `/interrupt`, `/mode`,
`/force`, `/edit`, `/cancel` or `/cancel-edit`, and no separate run/resume command.
Removed commands are errors, not aliases. `/send` accepts only a draft ID, with no
delivery or queue flags.

Outside an active completion pager, Enter sends text, executes an exact command,
or finishes a leased pending edit. Alt+Enter, Shift+Enter and bracketed paste
support multiline input in the composer; help uses Alt+Enter to insert a command
instead. History
uses arrows and PageUp/PageDown; Home/End load the oldest/latest saved messages
from the list. Ctrl+O opens/closes the fullscreen journal.
History always requests the alternate screen and mouse capture, whether opened
by `/history`, `/history fullscreen` or an effective history shortcut. Other
panels and the command field stay inline. Shortcut/secondary-field navigation
retains the primary draft; typing a navigation command in the primary composer
consumes that command text. In inline mode the terminal retains its own scrollback
and mouse behavior.
Details supports scrolling. Esc backs out of completion, the command field,
details or nested panels before returning to the conversation, without stopping
work or discarding the retained primary draft/caret. Pending edits use the
acknowledged cancel/restore flow described below.

Two plain Esc presses within **700 ms** invoke the same stop action as `/stop`,
only in the plain primary root: no menu, palette (including pinned fields),
completion/rollback/search, pending edit or input operation. Closing or cancelling
one of those interfaces does not count toward the gesture. Other input, paste or
shortcuts disarm it. The gesture preserves pending messages and unsent composer text.
Both controls use the same checked `StopPending` operation, carrying the expected
active attempt ID (or no active attempt) and paused state. A stale selection or
pause-state conflict rejects the operation without killing a successor or consuming
input; producer cancellation follows the successful transaction, not validation.

If messages are queued, stop cancels the selected active attempt and leaves dequeue
unpaused so existing messages run automatically in FIFO order, without new input.
Only when none are queued does it leave the session paused until a successful
nonempty send, `/send`, or nonempty pending-edit finish resumes it. The editing
barrier is unchanged: any active lease blocks dequeue even when unpaused, and stop
does not select a pending message into the editor or release its lease.

### Interactive Help

`/help` opens a searchable catalog, not an action launcher. Type to filter command
names, descriptions and categories; Tab/Shift+Tab cycle **All / AM / Input / Queue /
Preferences**. Up/Down select and PageUp/PageDown move by ten entries. Effective
shortcuts appear beside the descriptions.

Enter opens an interface command's panel, such as settings, drafts or bindings.
For an action command it opens command details, initially selecting **Insert**;
it does not run the action. Details offer **Run** only when no required arguments
are missing, and running requires explicitly selecting Run and pressing Enter.
**Insert into command field**, or Alt+Enter from the catalog, prepares the selected
command in the secondary field without executing it or replacing the primary draft.
Esc returns from details or nested panels to the previous interface, retaining the
catalog's filter, category and selection as well as the primary text and caret.
The clear-input shortcut (Ctrl+U by default) clears the visible help filter, not
the retained primary draft. Ctrl+O closes a history reader opened from help back
to that help panel, just as Esc from the history list does.

### Two Fields

Ctrl+P opens a separate command field without replacing primary text or its caret.
To pin both fields while retaining text, open Ctrl+P and execute `/commands keep`
there. Alt+I switches focus between the pinned fields. Commands execute in the
focused command field; primary text remains independent. `/exit` closes that field
without quitting or sending primary text. Esc also closes/unpins it after rolling
back any pending completion. Pinning, focus and the command-field query are
client-only; only the primary composer is recovered after restart.

### Delivery And Drafts

Ordinary text submission, `/send <draft-id>` and nonempty pending-edit finish use
**After** semantics. Input commits durably without automatically steering or
interrupting the active attempt. Queued work waits for the full active generation
and its inline actions to finish, not merely the next action boundary, then runs
in FIFO order. Stage 1 has only a deterministic fake inline tool interval and
journaled fixture actions, not real tool execution. Legacy core delivery modes
and their serialized API remain available internally; they are not public client
commands or delivery choices.

Consecutive sends append to the **latest message only while that message is still
queued**, separated by a blank line (`\n\n`). Its ID, FIFO position and frozen
dispatch configuration stay intact; its revision advances. Once the latest
message has been accepted or cancelled, a new send creates a new message rather
than appending to an older queued item. Any active pending edit lease blocks
submission. There is no artificial batching delay: accumulation depends on what
is still queued when the transaction executes.

Existing queues are not flattened or rewritten on reopening. Older pending rows
retain their own text, IDs and frozen configurations, and dequeue in FIFO order.
An append must fit both the current and original frozen message-byte limits.
Submission resumes a paused queue in the same transaction; the expected pause
state is checked, so a conflicting state change rejects the send without consuming
input or a saved draft. Empty ordinary Enter neither submits nor resumes.

To save current text without sending it, use Ctrl+P, then `/draft save`. No ID
argument is accepted: runtime generates an ID. The primary text clears only after
the save acknowledges. There is no public draft-delivery mode.
Saved drafts retain raw input, including slash escapes and multiline text.
`/send` applies the same slash-unescaping and multiline payload rules as direct
message submission; it does not execute draft text as a local command.
`/drafts` shows IDs; Enter loads a **copy**, keeping the saved draft. If primary
text is already present, it is first saved as another draft before loading the
copy, rather than discarded. Delete or `/draft delete <id>` removes a saved draft.
Drafts are bounded by the queue-count and message-byte limits.

`/send` validates a unique ID prefix of at least four characters and its revision.
Draft removal, message creation or append, a new message's dispatch config snapshot,
resume when paused, durable event and receipt share a SQLite transaction. A failed
dispatch leaves the draft available; a replay does not send it twice. Tab completes cached draft IDs without
sending/deleting; dispatch revalidates them. Reopen `/drafts` to refresh the list.

Primary text and its UTF-8 caret offset are autosaved with a 300 ms debounce and
revision checks; controlled exit drains the latest save. Reopening restores them
without sending text or resuming work. A crash can lose edits not yet acknowledged.
Queued edit-lease text and the secondary command field are not composer autosaves.
Primary submission atomically consumes the editor source using its composer
revision, including recently edited text and `//` escapes. It does not depend on
a later empty-editor autosave to prevent restoration of an acknowledged send.

### Pending Edits

Up first moves through the primary composer's visual rows. At the top, when no
vertical move is possible, it acquires the newest queued message for editing
**before prompt recall**, even with nonempty composer text or `prompt-history`
disabled. The ordinary draft becomes a suspended shadow: its exact text and UTF-8
caret are saved and acknowledged before lease acquisition. Only the authoritative
lease reply replaces the editor. If the message was already accepted or its
revision changed, acquisition fails visibly and preserves the draft/caret.

During the lease, slash text is literal and leased text never autosaves over that
shadow. Esc cancels the edit after any completion/command-field rollback; Down at
the bottom, when no visual move is possible, also cancels it. Cancellation retains
the original pending message. The suspended draft and exact caret return **only
after ACK**, not optimistically while a request is in flight.

Enter with empty or whitespace-only edited text deletes/cancels that pending
message without resuming work. Nonempty Enter uses `FinishPendingEdit`: replacement,
lease release and resume if paused commit together, without steering the active
attempt; delivery waits for its full generation and inline actions to finish.
The message keeps its ID, FIFO position and frozen configuration. Successful finish
or deletion restores the shadow only after ACK; failure retains the lease, edited
text and shadow for correction or retry. These are internal runtime operations,
not public `/edit` or `/cancel` aliases.

### Completion Contract

- Ordinary completion is a compact **fish-style pager below the composer**, shown
  only after Tab, not merely by typing `/`. Ctrl+P or `/commands` explicitly opens
  the palette and is the visibility exception. No above-composer fallback is part
  of this contract. Matching ranks exact, prefix, then substring matches, keeping
  catalog order within each rank; this is not fuzzy matching.
- An ordinary unique Tab result inserts the complete token plus a separating
  space, with no pager or execution. History-search results are an exception:
  even a unique match inserts its sequence without a trailing space and keeps an
  acceptance-only pager. An ambiguous first Tab extends the common prefix when
  possible and opens an **unselected** pager. For example, `/d` has several
  candidates; `/q` now uniquely completes `/quit` rather than a removed command.
- Subsequent Tab/Shift+Tab and Up/Down navigate with wrap and **preview the token
  in the editor**. PageUp/PageDown move by six candidates. Left/Right cycle once
  a pager candidate is selected; before selection they remain caret navigation.
  Shift+Left/Right instead move by words and reset the pager without accepting a
  ghost suffix or cycling candidates.
  Enter commits the preview and closes the pager, **never executing**. Enter on
  an unselected pager only closes it. A separate next Enter dispatches the exact
  resulting command or sends text. An ordinary unique Tab has no extra acceptance
  stage: its next Enter can execute. History-search completion still requires
  acceptance before a separate Enter opens the anchor.
- Escape restores the exact **pre-completion text and caret**, including after
  unique Tab insertion, common-prefix expansion, preview and pager search. Once
  completion is committed or ordinary editing starts, it is no longer a pending
  rollback. Space is ordinary input, not acceptance of an invisible candidate.
- A real gray autosuggestion can extend a token without opening the pager.
  At the end of the editor, Right/End/Ctrl+F/Ctrl+E accepts its suffix; Alt+Right or Alt+F
  accepts one Unicode word (Ctrl+Right also uses word acceptance). Without a
  suffix, or away from the end, these keys move the caret instead. They never run
  a command or invent text from a substring match.
- Ctrl+S opens/toggles explicit pager search; initial Shift+Tab starts completion
  in search mode. Search filters cached labels, values and descriptions; typing
  a query previews a match, while no matches restore the original editor text.
  Search Enter still only commits/closes; Escape restores the original text/caret.
- Completion covers command **arguments and subcommands**, not just names:
  `/settings` keys, exact values and `--default`; history subcommands and saved
  dialogue search/sequence anchors; `/commands keep`; and draft actions/IDs.
  Removed commands and send flags have no completion candidates. Cached draft IDs
  are revalidated at dispatch. There is no `Stage::Apply`, Apply screen or separate
  scope chooser.
- Binding completion also covers commands and arguments inside each `;`-separated
  `/bind` step, current macro chords for `/unbind`, and `--default`. The visual
  binding editor has the same reversible pager, but always requires a separate
  Enter to save after confirming completion, including a unique Tab result.
- Ctrl+P has its own query/editor and does not overwrite the ordinary draft,
  caret or leased edit. Outside an active Tab pager, palette Enter can explicitly
  open/run a visible no-required-argument command, including the bare `/settings`
  panel; commands requiring draft IDs first enter argument completion.
  An active palette pager retains the same commit-only Enter rule. Escape rolls
  back completion first, then closes the
  palette on a subsequent press. Accepting a selected candidate requires a
  readable label, at least 24 columns and four rows; tiny terminals do not force
  an unreadable choice.
- Streaming, timers and resize never accept completion or mutate its preview.
  This borrows fish interaction, **not full fish shell grammar, shell context or
  aliases**. No filesystem/model-name completers or shell execution are claimed.
  Unknown single-line slash commands and invalid arguments remain in the draft
  with an error; they never silently become a model message.
- `//path` sends literal `/path`; normal embedded slashes, indented slash text and
  multiline input, even beginning with `/quit` or another command name, remain
  messages. During a leased edit, slash text is literal.
  Esc cancels a pending edit only after any active completion or command field is
  closed. Up/Down/Esc/Enter edit access does not depend on configurable shortcuts;
  runtime checks stable message IDs and revisions rather than shifting indices.

These are local interactive-client commands. A headless `--prompt` remains literal
prompt text, not an alternate administration protocol.

### Editor Keys

The bounded editor inserts at the caret and moves/deletes whole Unicode extended
graphemes, including combining marks and joined emoji. It wraps by terminal cells,
keeps the caret visible and preserves the desired column during vertical movement.
The configured message-byte limit also bounds the draft; paste is sanitized and
does not split a grapheme to fit the limit.

| Keys                        | Action Outside The Pager                                                                                            |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Left / Right                | Move one grapheme; Right can accept a tail autosuggestion                                                           |
| Ctrl+F                      | Move right or accept a tail autosuggestion                                                                          |
| Ctrl/Alt+Left, Alt+B        | Move one Unicode word left                                                                                          |
| Ctrl/Alt+Right, Alt+F       | Move one Unicode word right or accept one suggested word                                                            |
| Shift+Left / Shift+Right    | Move one Unicode word without ghost acceptance, also with Ctrl/Alt                                                  |
| Home / End, Ctrl+A / Ctrl+E | Start/end of the logical line; End/Ctrl+E can accept a tail suggestion                                              |
| Ctrl+Home / Ctrl+End        | Start/end of the entire editor                                                                                      |
| Up / Down                   | Move through visual rows; at the top Up prefers pending edit over recall, at the bottom Down cancels a pending edit |
| Backspace / Delete          | Delete one grapheme before/after the caret                                                                          |
| Ctrl/Alt+Backspace, Ctrl+W  | Delete the preceding word                                                                                           |
| Ctrl/Alt+Delete             | Delete the following word                                                                                           |
| Ctrl+K / Alt+U              | Delete to logical line end/start                                                                                    |
| Ctrl+U                      | Clear the draft by default, not just the current line                                                               |

Configured action shortcuts take precedence over editor chords. There are no
default function keys and no external editor invocation at this stage.
Shift+Left/Right word motion applies to primary, secondary command-field and
binding-command text; it does not select text or accept an autosuggestion.

### Configure Shortcuts

There are **no default function-key bindings**. Assign multiple shortcuts, replace
defaults, or disable a configurable shortcut in the explicit TOML file:

```toml
[keys]
commands = ["alt+p"]
history = ["alt+h", "f2"] # F-keys are optional, not required.
details = ["alt+d"] # Alt+I defaults to switching pinned input focus.
stop = []
```

Missing fields inherit defaults; `[]` disables the shortcuts for that action, not
its command. Explicit TOML changes apply at the next launch; there is no hidden hot reload.
Supported chords use Ctrl/Alt with ASCII letters, Alt with digits, or F1..F24.
Conflicts, reserved input/navigation keys and classic-TTY ambiguities such as
Ctrl+H/I/J/M or Ctrl+digits are rejected before entering raw mode. Ctrl+C always
retains its two-press exit gesture and cannot be reassigned to another action. Terminal/OS
shortcuts intercepted before reaching AM cannot be overridden by the application.
See [examples/keybindings.toml](examples/keybindings.toml).

Obsolete `[keys]` action lists (`queue`, `pause`, `resume`, `toggle_pause`,
`interrupt`, `cancel_edit`, `edit`, `cancel`) are still accepted and serialized
for shipped configuration data, but are inert: they compile no shortcuts and do
not restore removed commands. Macro steps are different: each is validated against
the current public catalog. A removed command produces an explicit validation
error in TOML, `/bind` or the visual editor; it is not silently migrated, ignored
or treated as an alias.

Interactive command macros apply immediately in this client:

```text
/bind alt+z /stop
/bind alt+y /commands keep ; /bindings --default
/unbind alt+z --default
```

Each binding contains 1..=16 validated slash-command steps, not shell commands,
aliases or arbitrary prompt text. Steps run in order, waiting for their own runtime
or preferences acknowledgement. Failure stops the remaining chain. Macros do
not overwrite ordinary composer text/caret or an unrelated command-field query.
Reserved chords and collisions with built-in action shortcuts are rejected.

In `/bindings`, `n` captures a chord; enter a `;`-separated slash-command chain and
Enter saves it for this client. Enter or `e` edits a selected macro; Delete removes
it. `d` saves the current key map as defaults. Alt+Enter while editing saves defaults
directly. Esc cancels capture/edit or returns from the list. `--default` and visual
default saves write `[keys]` in the global preferences file, preserving existing
`[interface]` fields. Conversely a settings default save preserves bindings.
Explicit `[keys]` fields take precedence at startup; omitted fields retain saved
defaults.
The visual chain editor is bounded to 4,096 bytes. Longer TOML chains are refused
for editing rather than silently truncated; the existing binding stays intact.

The first Ctrl+C in the main conversation interrupts an active response and arms
exit. In a reader/palette it returns to the conversation without interrupting work.
The first Ctrl+C does not pause dequeue or discard pending input. `/stop` pauses
only when no messages are queued; otherwise it automatically continues the queue.
The next Ctrl+C within three seconds saves and exits, with **no `y` question**.
Other keyboard input or the timeout disarms exit. `/quit` requests exit without
another confirmation, while still draining pending saves. Drafts remain intact
after the first Ctrl+C; the primary composer is autosaved but is not queued input.
A submitted draft is cleared only after runtime acknowledges a
successful commit. Pending editing blocks dequeue until the edit is committed or
cancelled. If acceptance won the race, edit acquisition fails visibly.

### History And Input Recall

The live response is not hidden until completion: complete lines and bounded
long-line chunks go into native scrollback, while the unfinished tail stays live.
The final response continues from the same byte cursor, rather than reprinting it.
Completion handoff retains at most two observed attempt tails of up to 4,096 UTF-8
bytes each, keeping the retiring suffix available while the final durable poll or
an immediate next attempt catches up. Final durable text takes precedence over a
lagging active snapshot, and completed tails are not replayed.
Returning from a reader does not duplicate the conversation into scrollback.

Ctrl+O or `/history` opens the fullscreen journal, initially selecting the latest
loaded message. Its list contains saved messages and actions, with a preview of
the selected message or action output. At **100 columns or wider**, list and
preview appear side by side with a thin separator; narrower terminals show one
at a time. Enter opens preview focus; Tab/Shift+Tab switch list/preview focus.
Esc from preview returns to the list; Esc from the list returns to the previous
interface. Message/action rows show their real sequence, kind/category, status
and bounded text, without fabricated timestamps or a merged chronology between
their separate sequence domains.

In the list, Up/Down select entries and can cross between messages and actions;
PageUp/PageDown page within the selected stream, requesting adjoining bounded
pages as needed. Home/End load the oldest/latest saved messages. In preview,
Up/Down and PageUp/PageDown scroll the selected message or output; Home/End move
to the edges of a message. Space folds/unfolds an available artifact preview.
Actions without output show `No output artifact.`; artifact previews expose path,
bytes and incomplete state. Action categories come from structured records, not
inferred text parsing. Actions and output are read in bounded pages on the worker,
not by loading a whole action tree or file into the input/render loop. Arbitrary
action-ID jumps are not supported.

Cached running actions have an independent bounded metadata refresh, including
while browsing away from the tail. Completion/cancellation updates the existing
row without moving selection or consuming page navigation. New output reopens
artifact EOF; an already displayed page and its scroll stay intact until paging.
Artifact read commands do not change file permissions or content; normal filesystem
access-time updates may still occur. Private permissions are established on
creation and hardened on explicit append, not repaired by viewing output.

Press `c` for the chronological conversation scroll view, including the active
response. It starts at the bottom, follows new text there, and retains the reading
position when you scroll away. Arrows/PageUp/PageDown scroll; Home/End load the
oldest/latest messages. `c` or Esc returns to the list. Long messages are read in
UTF-8-safe segments with a bounded row cache; there is no arbitrary 16 KiB display
cutoff. At most eight records per read and 16 cached records are used. Basic
Markdown formatting is segment-local. Cached running entries are refreshed after
completion even when reading away from the tail, without resetting the anchor.

`/history <seq>` opens and selects that exact saved history sequence; a missing
entry reports an error rather than selecting a nearby or invented row.
`/history <fragment>` followed by Tab asynchronously searches **durable user and
assistant dialogue**, not just currently loaded messages. Matching is a
case-insensitive substring of text or sequence digits, newest first. Local notices
and actions are not search candidates. The client caches only the current query's
**up to 32 matches with micropreviews of up to 160 characters each**, not the entire
conversation. New durable events invalidate the cache; stale replies cannot
replace a changed editor/query or reopen an escaped pager. Completion inserts a
sequence anchor without opening history; pager Enter only accepts, and a separate
Enter executes the command. Esc restores the pre-completion text and caret.

Composer recall uses the entire original draft as a **case-insensitive substring
filter**. Up on empty input visits all remembered inputs, newest first, using the
newest occurrence of each exact text. Down moves forward through those matches;
past the newest match it restores the original unsent draft **and caret**. The
query stays fixed while browsing; ordinary editing starts a new recall query.
The client input cache is bounded to 100 entries. Sent prompts are loaded from
the current session on restart; executed local slash commands are recalled only
in this client, not durably across restart, even when their outcomes are saved.
Completion stays closed on untouched recalled commands. Disabling recall does
not delete saved history. Up/Down first try visual-row movement, including soft
wraps. At the top of the primary composer, Up prefers the newest queued message
over recall, even for a nonempty draft and with recall disabled. Recall is the
fallback when no movement or pending edit acquisition is possible and there is
no active lease; bottom Down cancels an active pending edit instead of recalling.

### Saved Console Outcomes

Short local outcomes and errors are conversation records, not assistant replies
or model inputs. They appear in native root output and saved history, including
after restart. Examples are `status-color enabled`,
`status set to braille (default)`, `paused`, and `error: ...`. Only completed local
actions are reported as successful; navigation, reader polling, timers and spinner
frames do not produce log records. Categorized settings, queue, drafts and bindings
outcomes appear in grouped trees in native output and in the journal's action list.
Their history rows remain plain single-line notices, not assistant Markdown.
Native trees also apply when reading saved history after restart. Categories are
restored from indexed durable command receipts by notice sequence, not inferred
from text; legacy notices without a category use an `Events` group. The latest
child stays in the owned live area as `└ result`. When another sibling arrives,
the prior child is committed with `├`; a boundary commits the real last child
with `└`. There is no empty `└─` terminator or rewriting of terminal scrollback.
The native `Settings` group renders its held last child above the pre-input blank
gap, including while an inline panel is open; the heading never leaves an orphan
`●` in place of the result.

SQLite still uses `PRAGMA user_version=2`, not v3. Drafts, composer,
dispatch configs, artifacts and actions are additive tables. Schema v2 allows a
null `history.attempt_id` **only for `kind='notice'`**;
client-only outcomes need no fake attempt. Existing v1 databases migrate
transactionally, preserving record IDs, history sequence values and their
AUTOINCREMENT high-water mark (including deleted rows), explicit history indexes,
receipts and other session data. Foreign keys are checked before migration commits.
`RecordNotice` and categorized `RecordAction` accept nonempty single-line text of
at most 1,024 UTF-8 bytes, without terminal controls. `RecordAction` also carries
an explicit validated category in its command receipt. Both return `Reply::Notice`
and keep the same history-row shape; the categorized action is not a new history
kind. History row, action row, durable event and command receipt
commit together before acknowledgement. `Reply::Notice` includes the saved row,
so publication does not depend on transcript page size. Replay returns that same
row without appending another notice; different content under that ID is rejected.

A provider failure's bounded reason is saved as an error notice in the same final
completion transaction as the response and attempt status. Notices do not enter
prompt recall or provider inputs and do not change queue order, message revisions
or edit leases. Saving an outcome is not saving a UI preference: transient settings
remain client-only; `--default` or the settings panel's **Save as defaults** writes
preferences.

The client sanitizes and bounds errors, deduplicates repeats while pending/in-flight
or still displayed, and uses a 16-entry pending notice queue. Its admission guard
reserves room for outstanding action/save results; saturation delays mutations,
not typing or navigation. A notice-write failure is shown as `history not saved`
without recursively trying to log the logging failure. Disk-full/storage errors
cannot be promised durable when storage itself has failed.

During an active response, saved notice receipts publish in root output without
waiting for the response or a transcript page. Adjacent printed sequences are
compressed into bounded ranges, preventing replay when the ordered native cursor
later reaches them. Publishing a notice never advances the response byte offset.
The saved reader and native output
remain separate projections: reading or refreshing history does not rewind native
publication or reprint viewed records.

### Settings

Bare `/settings` opens an inline panel for four client UI preferences, with
boolean switches and a status-preset choice. Up/Down selects a row. Space toggles
a selected boolean through the acknowledged apply flow. Enter opens a choice
editor for any preference; arrows choose, Enter applies, and Esc cancels without
applying the pending choice.
Esc again returns to the previous interface. There is no staged Apply screen.
The panel's **Bindings** row opens the shortcut editor; nested Esc returns locally.

The latest settings feedback has a dedicated, stable body row, separate from the
controls/footer and runtime status line. For example, an acknowledged change shows
`Status preset: bars -> minimal`. Apply/save success appears only **after ACK**;
errors remain visible and never falsely claim a value changed or defaults were
saved. Feedback survives the three-second transient-notice timeout, active response
generation, navigation and reopening the panel in the same client. Ctrl/Esc
cancellation leaves the previous feedback intact. The row is reserved even before
the first result, so feedback does not grow or shift the normal panel body.

The key/value command and its reversible completion flow remain available:

```text
/settings
/settings <key> <value> [--default]
/settings status bars
/settings status bars --default
/settings status braille
/settings status-color true --default
```

`/settings <key>` prepares value entry; Tab discovers keys, values and the optional
flag. Completing a token does not change a preference. A complete validated
key/value command applies only when executed.

| Command Key      | Values                               | Built-In Default | TOML Field In `[interface]` |
| ---------------- | ------------------------------------ | ---------------- | --------------------------- |
| `status-line`    | `true`, `false`                      | `true`           | `status_line`               |
| `prompt-history` | `true`, `false`                      | `true`           | `prompt_history`            |
| `status`         | `text`, `braille`, `minimal`, `bars` | `bars`           | `status_preset`             |
| `status-color`   | `true`, `false`                      | `true`           | `status_color`              |

Panel changes and commands without the flag apply only to **this running client
until exit**. These are client UI values, not backend telemetry controls.
The preference value is not a durable session setting, is not written to SQLite,
and does not alter another client; its short outcome can still be saved in chat.
After a valid value and whitespace, `--default` is offered as
**Make default**. It saves just the selected field to the global UI preferences
file and applies it to this client only after the save is confirmed. It does not
capture other transient values or explicit config overrides, and it leaves other
saved fields intact. Future clients still honor explicit TOML overrides per field.

The panel's **Save as defaults** instead saves a snapshot of **all four current
client values** in one preferences update, including transient changes or effective
explicit-config values, while preserving key bindings. It does not change runtime
configuration or introduce backend telemetry. Success is reported only after the
worker confirms the save; completing it does not reapply values or consume newer
input. Other clients are unaffected, and future clients still honor explicit TOML
overrides. Settings changes and confirmed saves retain the existing short,
categorized console outcomes.

All four status presets are available in the panel and completion. The built-in
default is `bars`; an explicitly saved or configured `braille` remains `braille`
under the same per-field precedence, rather than being migrated to the new default.

Preferences read-modify-write uses a stable sibling exclusive lock, a private
temporary file (mode `0600` on Unix), file sync, atomic rename and Unix directory
sync. Reads are bounded to 64 KiB and reject non-regular/symlink preference files.
A failed or unconfirmed save is not reported as success: the current client value
and command draft remain unchanged. A failure after replacement can leave an
uncertain disk outcome, so no rollback guarantee is claimed.
UI Quit is an exit intent, not immediate backend `Shutdown`: normal exit and
handled SIGTERM finish outstanding preferences work, action/read replies and
queued/in-flight notices and their native publication before sending `Shutdown`. A default-write or notice-save
failure during exit is retained as an exit error, not swallowed by successful
backend shutdown; `main` still joins/shuts down the backend and propagates the client
error. SIGTERM uses the controlled normal flow. Closing the palette keeps its
submitted text until that result is known; failure makes it available for
correction or retry.

Scope, effective values, per-field sources, config/preferences paths and precedence
are exposed in `/details`, not repeated as a tutorial in the compact pager.
Pager descriptions remain local to the token: valid choices, current value,
and the **Make default** option. Help provides command usage and effective shortcuts.

The equivalent file fields are:

```toml
[interface]
status_line = true
prompt_history = true
status_preset = "bars"
status_color = true
```

Runtime budgets and fake-provider options remain explicit `--config`/restart-only.
Global preferences accept the interface fields and validated key bindings/macros;
there are no fake model, shell, proxy or credential switches.

## Client Style

The first-stage diagnostic layout is not the final MVP design. The client now
uses the built-in `fx-dark` grayscale palette: separators at xterm-256 index 240,
metadata and ghosts at 245, notices at 250, warnings at 252, and bold selections
at 255. The terminal background stays unchanged. A vertical input/user-message
rail and whitespace replace boxes; completion and inline selections have no `>`
marker, only brightness and bold weight.
The root viewport owns its bounded unfinished tail, pending-queue preview, primary
composer, optional secondary field/pager, separators/gaps
and one status line.
There are no UUIDs, permanent ACK notices, colored role
banners, or an always-visible table of shortcuts in the main conversation.

Pending messages appear above the composer on a muted **dotted rail**, with up to
two wrapped rows per message and a six-row total budget. The newest queued
messages are retained when space is limited, displayed in queue order; long text
gets an omission marker. This is a live queue projection, not another copy
published into native scrollback or saved history. Moving/resizing it does not
republish the conversation.

Queue previews hide leased messages, since their text is already in the editor.
The durable message and edit lease remain intact until save, cancellation or
deletion acknowledges; hiding a preview never deletes the pending input.

An accepted card is retained for display using the runtime-authoritative
`ActiveView.message_id`, not inferred from a queue item's disappearance or an
arbitrary matching record. This handoff preview retains at most two cards of up to
4,096 UTF-8 bytes each until the corresponding native user publication succeeds;
retirement belongs to that publication's plan/commit flow. It is not a fake
editable queue entry after acceptance. Display retention does not affect queued
user counts, which remain authoritative snapshot values.

With no queue preview, command field or pager and sufficient height, desired root
rows are `L + I + 3`: `L` is the bounded live tail, `I` is one to three composer
rows, and the remaining three are one blank before input, one blank after input
and the status row. A queue preview adds its bounded rows and a gap before input.
Native entries put their separator **before** the next block, with no trailing native
blank; root owns the one blank before input. Every normal inline panel also keeps
at least one blank row between held/native output and input, including when there
is no held notice, as the space budget permits. Only tiny, clipped layouts may
omit that gap. The welcome emits no extra empty line, so the initial root gap is
also exactly one. There is no ordinary root divider.
An open command field adds its title and one to three editor rows below the primary
composer. Ordinary paused sends resume directly, without a choice row.

The live region uses **actual visible content**, with no empty active-response
reservation. Before native `insert_before` output, the client plans publication
and resizes to the projected post-publication geometry; response offsets,
publication cursors and accepted-card retirement commit only after output succeeds.
Live-to-native replacement therefore does not need retired blank rows to keep the
composer in place. The input/status block remains content-attached, not pinned to
the terminal's physical
bottom. New content, input/menu growth and consuming an actual queue preview can
naturally move it; a completely fixed screen coordinate is not promised.

**All inline panels and completion pagers stay below the composer**, with thin
fx-gray separators above and below their contents. Help uses a separate search
editor in that position while preserving the primary draft. Catalog/pager labels
and descriptions align in columns; selection brightens and bolds both, without
an arrow marker or a selection background.

An open pager has up to six choices, a title/count and blank header row when space
allows, and a shared search/count footer, for a nine-row body budget plus two
separators. Empty/filtered results and search keep that budget and the composer's
top anchor. Inline panels size their bodies to their content within bounded
budgets, rather than all using a fixed fourteen-row viewport; history always uses
the full screen. Settings includes its stable latest-feedback body row, not a
temporary success message in the runtime status line or controls footer.
Tiny terminals prioritize input/status and a readable choice over gaps/live rows.

Viewport changes preserve the top anchor and clear stale owned rows. Growing an
inline area near the physical bottom can necessarily scroll the terminal; this is
not a promise of a completely fixed screen pixel. Synchronized frames reduce
intermediate repaint, not the geometry required by growth or native publication.
The cursor is queried once before starting keyboard input. During drawing and
resize, the backend tracks the client's own cursor operations instead of issuing
competing DSR reads through the keyboard event stream.

`/help` browses commands by category/search with effective shortcuts; `/details`
exposes technical information without crowding the normal screen. History is
also unboxed. Assistant text has basic
Markdown emphasis: bold headings/text, gray inline code and fenced blocks. This
borrows fx styling, not a full fx Markdown renderer or language-specific syntax
highlighter. Errors remain visible. Status defaults to `bars`: three bright-white
terminal cells with independently varying block heights, animated only when an
attempt is active in Waiting, Receiving or Stopping. Frames are time-driven in
100 ms steps, not driven by chunk counts or invented progress. Idle status says
`ready` starting at column 0, without a running timer, animation or reserved blank
prefix. The three white bar cells exist only while an attempt is active.
`status-color false` disables phase-text colors, not the white bars. Stopped state
and queued count stay visible during an active
response instead of being replaced by a short success notice. Timers describe
actual fixture phases, not invented API/tool activity. The default palette uses
explicit fx gray shades rather than terminal-dependent ANSI dimming; no background
color is forced.

### Client Modules

The former single UI file is split by current responsibility, without a second
runtime or a public plugin interface:

```text
src/main.rs                  CLI, startup, headless fixture, backend join/exit errors
src/client/mod.rs            event loop, command worker, framed publication, exit drain
    state.rs                 views, dispatch, notices/native cursors, recall, deferred quit
    input.rs                 bounded grapheme-aware editor and caret layout
    completion.rs            command/argument candidates and real ghost suffixes
    render.rs                fields, queue rail, inline layout, pager, status/details
    panels.rs                inline settings switches/choices and searchable help
    journal.rs               bounded action pages and artifact preview
    history.rs               segmented history index and bounded row cache
    history_browser.rs       fullscreen message/action list, preview, focus and paging
    transcript.rs            plain notices, sanitization, Markdown, scrollback rows
    terminal.rs              anchored viewport, synchronized frame guard, explicit reader
    theme.rs                 fx-dark grayscale styles, no fixed background
    tests.rs                 client interaction tests
src/commands.rs              shared command schema and configurable actions
src/settings.rs              UI fields and key-map preference writes
src/config.rs                validation, per-field precedence and provenance
src/runtime.rs / storage.rs  queue/drafts/composer, frozen dispatch, v2 history/actions
src/artifacts.rs             retained fixture output, bounded reads and quotas
src/provider.rs              deterministic offline fake producer
```

## Guarantees And Limits

- Runtime and SQLite run on a dedicated blocking worker, separate from input
  and rendering. The fake producer runs asynchronously through a bounded channel.
- Critical changes and command-ID receipts commit transactionally before ACK.
  Retrying the same ID and content returns the recorded result; changed content
  under an existing ID is rejected. These are internal contracts, not a public SDK.
- SQLite uses WAL and `synchronous=FULL`. An exclusive session lock prevents
  concurrent writers; symlink and hard-linked database paths are rejected.
  Schema v2 and its transactional v1 migration retain the existing session history.
- Defaults: 32 queued messages, 16 KiB per message, 64 KiB per response, 50
  entries per history page, and a 64 MiB SQLite page quota. Response overflow
  stops the fixture and marks its saved response `truncated`.
- Fake response output is also retained under `<session-path>.artifacts/`, with
  default limits of 16 MiB per output and 256 MiB total artifacts, separate from
  SQLite and response-display limits. Quota exhaustion retains existing output
  and marks it incomplete rather than deleting it. Partial/unregistered files are
  recovered explicitly after restart; SQLite/file writes are not one filesystem
  transaction. No automatic artifact deletion or real tool execution is implied.
- Streaming checkpoints default to 250 ms. The aggregate uncommitted text
  budget is 64 KiB, including producer queue/in-flight reserves. A crash can lose
  the uncommitted suffix, not an acknowledged queue transition. The interval
  is not a hard wall-clock guarantee on a stalled machine or disk.
- WAL growth is checked before mutations. With defaults, the reclamation
  threshold is 1 MiB plus room for one transaction. A reader pinning WAL causes
  a storage error rather than endless writes. The SQLite page quota is not a
  strict total-filesystem quota; WAL/SHM and transaction overhead are separate.
- Write or read failures freeze new work, stop the producer, and remain visible.
  Fix storage and restart. A failed shutdown is not reported as saved; its
  last uncommitted response can remain ambiguous until recovery.
- Recovery releases incomplete edit leases, keeps their original text, marks
  unfinished attempts `interrupted`, and pauses. It never replays an attempt.
- Dispatch freezes fake-provider configuration and runtime limits when a message
  is queued, not when a later client starts its attempt. Existing pending messages
  without snapshots are frozen once on upgrade. Client interface values, UI keys
  and global preferences are not captured as attempt/session defaults.
- Snapshots replace the current projection through `watch`; slow/disconnected
  viewers do not block runtime. Durable events can be read after a cursor.
- Rendering uses bounded projections without deleting displayable history.
  Native publication handles at most 1 KiB of source text per input-loop iteration.
  The reader indexes UTF-8-safe segments and caches at most 10,000 wrapped rows,
  instead of styling the full conversation every frame. All message text remains
  reachable by scrolling; terminal scrollback depth itself is emulator-controlled.
- Terminal control characters are sanitized before display. Terminal modes
  are restored on ordinary exits, errors, unwinding, and handled SIGTERM.
  SIGKILL, aborts, and power loss cannot guarantee terminal restoration.
- A frame guard brackets native publication, viewport resizing and drawing with
  `BeginSynchronizedUpdate`/`EndSynchronizedUpdate` (DEC mode 2026), ending before
  any event-loop await and attempting cleanup on errors/unwinding. Terminals that
  do not support the mode can ignore it; support is not claimed for every terminal
  or established by GUI testing. Root height is projected and resized before
  native insertion, with publication offsets committed only after output succeeds;
  startup worker reads remain read-only and never auto-resume the queue.

Flush guarantees depend on SQLite, the OS, filesystem, and storage device.
Process-kill tests do not prove protection against physical media failure.

## Verify

The application gate below remains separate from repository policy/style checks.
See [CONTRIBUTING.md](CONTRIBUTING.md#verification) for the pinned development
tools, commit/PR checks and changelog rules. Node and those tools are not needed
to run AM. Hooks are intentionally absent.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 tests/terminal_smoke.py
```

The commands above passed locally on Linux on 2026-10-16: formatting,
warnings-denied Clippy, 465 Rust tests (345 library, 1 CLI, 119 integration),
a debug build and the standard-library PTY smoke. New regressions cover cached
running-action completion/cancellation, output growth after EOF, page-one catch-up,
and artifact reads preserving permissions/content and durable state. See
[PLAN section 13](PLAN.md#13-current-contract-and-agent-handoff) for the handoff.
The earlier separate actual-DSR pyte run reported 120 passed checks, including final publication ordering,
accepted-card handoff, idle status alignment and settings feedback/margins.
That run was not repeated in this iteration. These are offline-fixture checks,
not real-provider or manual GUI acceptance.

| Verification Area               | Evidence And Limits                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Runtime/storage Rust tests      | Passed: checked stop/replay/stale selection, transactional rollback, FIFO autorun and edit barriers; ordinary accumulated input, draft send and `FinishPendingEdit` wait for full generation/action completion. Also queue conflicts, frozen configs, cancellation, quotas, locks, shutdown and crash recovery.                                                                                                                                                                                                                                                                                    |
| Client Rust tests               | Passed: stop catalog/flag rejection, double Esc timing/context, input preservation on rejection, queue-first Up with exact shadow/caret saving, ACK-only restore, edit deletion, paused-send resume, completion/settings/help/journal, four status presets and time-driven white bars. Geometry and ordering tests cover content-sized rows, post-publication resizing, completed native backlog, fullscreen receipt synchronization and authoritative accepted-card identities.                                                                                                                   |
| Unix PTY smoke                  | Passed: After delivery through full generation/actions, checked stop FIFO autorun/no-queue pause, bars, lease-preview hiding, edit cancel/delete/resume, shadow recovery, settings feedback and settings/history/exit. Emitted-sequence and SQLite/preferences/artifact checks use only Python's standard library.                                                                                                                                                                                                                                                                                 |
| Linux pyte + live PTY           | Passed 120 checks with no failures using actual emulated DSR replies, above and at the bottom edge, plus settings screens: exact shell/native scrollback preservation, no duplicated user cards or response chunks, no empty active reserve or idle bars prefix, Unicode caret, accepted-card/native publication handoff, retiring-tail visibility, inline gaps, persistent settings feedback, rejected/ACK-only default saves and restored terminal modes. Final report and settings screens are under `/tmp/opencode/am-layout-publication-evidence-spctcvy2`; pyte is not a project dependency. |
| Linux/macOS CI                  | Workflow provided; remote results are not verified here. Earlier local results were on Linux, not a current cross-platform pass report.                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Manual GUI/platform/performance | No manual GUI-terminal, macOS or real-provider acceptance is claimed. Pyte is a virtual terminal model, not a GUI emulator. Long-history latency and RSS are unmeasured here.                                                                                                                                                                                                                                                                                                                                                                                                                      |

The repository PTY smoke script checks emitted sequences and persisted state with
fixed `1,1` cursor-query replies; it does not reconstruct screen geometry. The
separate pyte check responds with the actual emulated cursor and measures absolute
composer/cell positions across synchronized frames. Its minimum live/native
handoff delta was zero in both above/bottom scenarios, including queue handoff:
the accepted card retired with native user publication, without a backward
composer gap, while the retiring response stayed visible or native through the
completion poll gap. Settings screens record wide/narrow layouts, the no-notice
pre-input gap, held last-child rendering, feedback after three seconds, during
generation and on reopen, and rejected versus acknowledged default saves. New
native content still moves the content-attached block naturally. These measurements
do not prove GUI animation quality or universal terminal reflow behavior.
No real API, tools, shell, project-write diffs or
coding-agent work is verified.

## Repository Workflow

Changes use signed commits on `type/short-kebab` branches and PRs into `master`,
merged as merge commits. [AGENTS.md](AGENTS.md#git-authority) records the scoped
agent authority; merges/releases and remote creation need explicit permission.
There is currently no remote and no configured server-side ruleset.

The fast CI gate checks commit/PR structure, declared change impact, changelog
fragments, file validity and its own tests. Separate pinned style/rustfmt checks
precede the existing Linux/macOS application matrix. Advisory size reports include
raw/grouped churn and source-file percentages; L/XL PRs need an explanation, not
an automatic size veto. Details and local commands are in CONTRIBUTING.

## Future Connections

Stage 2 will add real API transport, initially Chat Completions. `/connect` is a
future compact inline form, not a current command or fake login screen: provider
presets plus endpoint, profile name, model and credential-reference selection,
with multiple selectable profiles and explicit sources suitable for CI/headless
use. [ARCHITECTURE.md](ARCHITECTURE.md) selects explicit environment/private-file
credential references; source validation, authorization and transport are still
unimplemented. The next incremental local-only transport task is in PLAN section 13.

Responses is a future adapter. Subscription/account login and OAuth integration
are separate work, not assumed equivalent to an API key. External editing via
`VISUAL`/`EDITOR` (including vim) is also deferred; Stage 1 starts no shell/editor
children and performs no network or credential operations.

## Separate Research

[DELM_NOTES.md](DELM_NOTES.md) documents read-only research into
[yuzhenmao/DeLM](https://github.com/yuzhenmao/DeLM). It is a future multi-agent
coordination reference, not a UI/autocomplete dependency. No DeLM code, Python
sidecar, model request or dependency was integrated into AM.

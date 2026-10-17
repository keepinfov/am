pub use crate::artifacts::{ArtifactChunk, ArtifactRecord};
use crate::{
    config::Config,
    provider::{self, Update},
    storage::{Completion, Store},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

pub const MAX_NOTICE_BYTES: usize = 1024;
pub const MAX_HISTORY_MATCHES: usize = 32;
pub const MAX_HISTORY_QUERY_BYTES: usize = 1024;
pub const MAX_HISTORY_PREVIEW_CHARS: usize = 160;

/// Dispatch modes interrupt active work only when the queue is unpaused after commit.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryMode {
    Steer,
    After,
    Force,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DraftRecord {
    pub id: String,
    pub text: String,
    pub revision: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComposerState {
    pub text: String,
    pub cursor: usize,
    pub revision: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionRecord {
    pub seq: i64,
    pub id: String,
    pub group_id: String,
    pub category: String,
    pub label: String,
    pub status: String,
    pub output: Option<ArtifactRecord>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Error,
    Status,
}

pub(crate) fn validate_notice(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_NOTICE_BYTES,
        "invalid notice size"
    );
    ensure!(
        !text
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}')),
        "notice must be a single line without terminal controls"
    );
    Ok(())
}

pub(crate) fn validate_action(text: &str, category: &str) -> Result<()> {
    validate_notice(text)?;
    ensure!(
        matches!(
            category,
            "settings" | "queue" | "drafts" | "bindings" | "delivery" | "error"
        ),
        "invalid action category"
    );
    Ok(())
}

pub(crate) fn input_payload(source: &str) -> &str {
    if source.starts_with("//") {
        &source[1..]
    } else {
        source
    }
}

pub(crate) fn validate_input(source: &str, text: &str, limit: usize) -> Result<()> {
    ensure!(
        !source.trim().is_empty() && source.len() <= limit,
        "invalid input source size"
    );
    ensure!(
        !text.trim().is_empty() && text.len() <= limit,
        "invalid message size"
    );
    ensure!(
        text == input_payload(source),
        "input payload does not match source"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Drafts,
    SaveDraft {
        text: String,
    },
    DeleteDraft {
        id: String,
        revision: i64,
    },
    DispatchDraft {
        id: String,
        revision: i64,
        mode: DeliveryMode,
        resume: bool,
    },
    Submit {
        text: String,
        mode: DeliveryMode,
        resume: bool,
    },
    SubmitInput {
        source: String,
        text: String,
        composer_revision: i64,
        mode: DeliveryMode,
        resume: bool,
        expected_paused: bool,
    },
    /// Append to the latest message while it remains queued, otherwise enqueue.
    /// Like SubmitInput, source may be newer than the saved composer text.
    /// Paused queues remain paused unless resume is explicitly true.
    /// Accumulation is rejected while any pending message is leased for editing.
    /// Ordinary input uses After; Steer and Force retain explicit legacy behavior.
    SubmitInputAccumulated {
        source: String,
        text: String,
        composer_revision: i64,
        mode: DeliveryMode,
        resume: bool,
        expected_paused: bool,
    },
    DispatchDraftChecked {
        id: String,
        revision: i64,
        mode: DeliveryMode,
        resume: bool,
        expected_paused: bool,
    },
    /// Consume a draft into the pending message and resume if paused.
    /// Wait for the entire active generation, including inline actions.
    DispatchDraftAccumulated {
        id: String,
        revision: i64,
        expected_paused: bool,
    },
    Composer,
    SaveComposer {
        text: String,
        cursor: usize,
        revision: i64,
    },
    Actions {
        before: Option<i64>,
        limit: usize,
    },
    ActionsAfter {
        after: i64,
        limit: usize,
    },
    ReadArtifact {
        id: String,
        offset: u64,
        limit: usize,
    },
    ReadArtifactBefore {
        id: String,
        before: u64,
        limit: usize,
    },
    RecordNotice {
        text: String,
        level: NoticeLevel,
    },
    RecordAction {
        text: String,
        level: NoticeLevel,
        category: String,
    },
    Send {
        text: String,
    },
    BeginEdit {
        id: String,
        revision: i64,
    },
    FinishEdit {
        id: String,
        revision: i64,
        text: String,
    },
    /// Replace a leased pending payload and resume if paused.
    /// Wait for the entire active generation, including inline actions.
    FinishPendingEdit {
        id: String,
        revision: i64,
        text: String,
        expected_paused: bool,
    },
    CancelEdit {
        id: String,
        revision: i64,
    },
    Cancel {
        id: String,
        revision: i64,
    },
    Interrupt {
        attempt_id: String,
    },
    Pause,
    Resume,
    Stop,
    /// Atomically cancel the selected active attempt and enable pending queued work.
    /// With no queued rows, pause instead. Editing leases are never released.
    /// Both expectations must still match; stale selections are rejected.
    StopPending {
        expected_attempt_id: Option<String>,
        expected_paused: bool,
    },
    History {
        before: Option<i64>,
        limit: usize,
    },
    Transcript {
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    },
    SearchHistory {
        query: String,
        limit: usize,
    },
    TranscriptAnchor {
        seq: i64,
        limit: usize,
    },
    NativeTranscript {
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    },
    Inputs {
        limit: usize,
    },
    Events {
        after: i64,
        limit: usize,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Reply {
    Ack,
    Stopped {
        paused: bool,
        cancelled: bool,
    },
    Drafts(Vec<DraftRecord>),
    Draft(DraftRecord),
    Composer(ComposerState),
    Actions(Vec<ActionRecord>),
    Artifact(ArtifactChunk),
    ArtifactPage {
        offset: u64,
        chunk: ArtifactChunk,
    },
    Notice(HistoryEntry),
    Editing(QueueItem),
    History(Vec<HistoryEntry>),
    HistoryMatches(Vec<HistoryMatch>),
    HistoryAnchor {
        seq: i64,
        entries: Vec<HistoryEntry>,
    },
    NativeTranscript(Vec<NativeEntry>),
    Inputs(Vec<String>),
    Events(Vec<JournalEvent>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    pub revision: i64,
    pub state: QueueState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum QueueState {
    Queued,
    Editing,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub seq: i64,
    pub attempt_id: String,
    pub kind: String,
    pub text: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryMatch {
    pub seq: i64,
    pub kind: String,
    pub preview: String,
}

pub(crate) fn validate_history_query(query: &str) -> Result<()> {
    ensure!(
        query.len() <= MAX_HISTORY_QUERY_BYTES,
        "history query exceeds {MAX_HISTORY_QUERY_BYTES} bytes"
    );
    ensure!(
        !query
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}')),
        "history query must be a single line without controls"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeEntry {
    pub entry: HistoryEntry,
    pub category: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEvent {
    pub seq: i64,
    pub kind: String,
    pub attempt_id: Option<String>,
    pub payload: String,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Waiting,
    Receiving,
    StorageError,
    Stopping,
}

#[derive(Clone, Debug)]
pub struct ActiveView {
    pub attempt_id: String,
    pub message_id: String,
    pub text: String,
    pub truncated: bool,
    pub durable_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub session_id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub phase: Phase,
    pub phase_started: Instant,
    pub last_event: Instant,
    pub active: Option<ActiveView>,
    pub queue: Vec<QueueItem>,
    pub paused: bool,
    pub recovered: bool,
    pub cursor: i64,
    pub error: Option<String>,
}

struct Request {
    id: String,
    command: Command,
    reply: oneshot::Sender<Result<Reply>>,
}

#[derive(Clone)]
pub struct Client {
    sender: mpsc::Sender<Request>,
    snapshots: watch::Receiver<Snapshot>,
    task: Arc<Mutex<Option<JoinHandle<Result<()>>>>>,
    message_bytes: usize,
}

impl Client {
    pub async fn start(path: impl AsRef<Path>, config: Config) -> Result<Self> {
        config.validate()?;
        let path = path.as_ref().to_path_buf();
        let (sender, receiver) = mpsc::channel(32);
        let (ready, startup) = oneshot::channel();
        let handle = Handle::current();
        let task =
            tokio::task::spawn_blocking(move || actor(path, config, receiver, ready, handle));
        let (snapshots, message_bytes) =
            match startup.await.context("runtime failed during startup")? {
                Ok(started) => started,
                Err(error) => {
                    let _ = task.await;
                    return Err(error);
                }
            };
        Ok(Self {
            sender,
            snapshots,
            task: Arc::new(Mutex::new(Some(task))),
            message_bytes,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }

    pub async fn command(&self, command: Command) -> Result<Reply> {
        self.command_with_id(Uuid::new_v4().to_string(), command)
            .await
    }

    pub async fn command_with_id(&self, id: String, command: Command) -> Result<Reply> {
        ensure!(!id.is_empty() && id.len() <= 128, "invalid command ID");
        match &command {
            Command::SearchHistory { query, .. } => validate_history_query(query)?,
            Command::TranscriptAnchor { seq, .. } => {
                ensure!(*seq > 0, "history sequence must be positive")
            }
            Command::RecordNotice { text, .. } => validate_notice(text)?,
            Command::RecordAction { text, category, .. } => validate_action(text, category)?,
            Command::SubmitInput { source, text, .. }
            | Command::SubmitInputAccumulated { source, text, .. } => {
                validate_input(source, text, self.message_bytes)?
            }
            Command::Send { text }
            | Command::FinishEdit { text, .. }
            | Command::FinishPendingEdit { text, .. }
            | Command::Submit { text, .. }
            | Command::SaveDraft { text } => {
                ensure!(
                    !text.trim().is_empty() && text.len() <= self.message_bytes,
                    "invalid message size"
                );
            }
            Command::SaveComposer { text, cursor, .. } => {
                ensure!(text.len() <= self.message_bytes, "invalid composer size");
                ensure!(
                    *cursor <= text.len() && text.is_char_boundary(*cursor),
                    "invalid composer cursor"
                );
            }
            _ => {}
        }
        let target = match &command {
            Command::BeginEdit { id, .. }
            | Command::FinishEdit { id, .. }
            | Command::FinishPendingEdit { id, .. }
            | Command::CancelEdit { id, .. }
            | Command::Cancel { id, .. } => Some(id),
            Command::DeleteDraft { id, .. }
            | Command::DispatchDraft { id, .. }
            | Command::DispatchDraftChecked { id, .. }
            | Command::DispatchDraftAccumulated { id, .. }
            | Command::ReadArtifact { id, .. }
            | Command::ReadArtifactBefore { id, .. } => Some(id),
            Command::Interrupt { attempt_id } => Some(attempt_id),
            Command::StopPending {
                expected_attempt_id,
                ..
            } => expected_attempt_id.as_ref(),
            _ => None,
        };
        ensure!(
            target.is_none_or(|id| !id.is_empty() && id.len() <= 128),
            "invalid target ID"
        );
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Request { id, command, reply })
            .await
            .context("runtime stopped")?;
        receiver
            .await
            .context("runtime stopped before acknowledgement")?
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut slot = self.task.lock().await;
        let Some(task) = slot.as_mut() else {
            return Ok(());
        };
        // The UI may already have sent Shutdown; joining also releases the OS lock.
        if !task.is_finished()
            && self.snapshot().phase != Phase::Stopping
            && let Err(error) = self.command(Command::Shutdown).await
            && self.snapshot().phase != Phase::Stopping
        {
            return Err(error);
        }
        // Keep ownership in the slot until the join finishes, even if this future
        // is cancelled while the database is busy.
        let joined = task.await;
        slot.take();
        joined.context("runtime thread panicked")?
    }
}

struct Active {
    view: ActiveView,
    updates: mpsc::Receiver<Update>,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    checkpoint: Instant,
    steer: watch::Sender<bool>,
    limits: crate::config::Limits,
    pending_status: Option<&'static str>,
    producer_gate: Arc<Mutex<()>>,
}

impl Active {
    fn stop(&mut self, handle: &Handle) {
        self.cancel.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
            // This actor runs on a blocking worker, never in the terminal loop.
            let _ = handle.block_on(task);
        }
    }

    fn drain(&mut self, store: &mut Store) -> Result<()> {
        while let Ok(update) = self.updates.try_recv() {
            if let Update::Delta(text) = update {
                store.append_output(&self.view.attempt_id, &text, &self.limits)?;
                self.append(&text, self.limits.response_bytes);
            }
        }
        Ok(())
    }

    fn append(&mut self, text: &str, limit: usize) {
        let mut bytes = text.len().min(limit.saturating_sub(self.view.text.len()));
        while !text.is_char_boundary(bytes) {
            bytes -= 1;
        }
        self.view.text.push_str(&text[..bytes]);
        self.view.truncated |= bytes < text.len();
    }
}

struct Engine {
    store: Store,
    config: Config,
    snapshot: Snapshot,
    active: Option<Active>,
    handle: Handle,
    shutdown_committed: bool,
}

impl Engine {
    fn refresh(&mut self) -> Result<()> {
        // Lowered admission limits must not hide existing pending rows.
        self.snapshot.queue = self.store.queue(1024)?;
        self.snapshot.cursor = self.store.cursor()?;
        if self.snapshot.phase != Phase::StorageError {
            self.snapshot.paused = self.store.paused()?;
        }
        self.snapshot.active = self.active.as_ref().map(|a| a.view.clone());
        Ok(())
    }

    fn phase(&mut self, phase: Phase) {
        if self.snapshot.phase != phase {
            self.snapshot.phase = phase;
            self.snapshot.phase_started = Instant::now();
        }
    }

    fn fault(&mut self, error: &anyhow::Error) {
        if let Some(active) = &mut self.active {
            active.stop(&self.handle);
        }
        self.snapshot.paused = true;
        self.snapshot.active = self.active.as_ref().map(|a| a.view.clone());
        self.snapshot.error = Some(format!("storage unavailable; no new work: {error:#}"));
        self.phase(Phase::StorageError);
    }

    fn command(&mut self, id: &str, command: &Command) -> Result<Reply> {
        self.shutdown_committed = false;
        match command {
            Command::Drafts => return Ok(Reply::Drafts(self.store.drafts()?)),
            Command::Composer => return Ok(Reply::Composer(self.store.composer()?)),
            Command::Actions { before, limit } => {
                return Ok(Reply::Actions(self.store.actions(
                    *before,
                    (*limit).clamp(1, self.config.limits.history_page),
                )?));
            }
            Command::ActionsAfter { after, limit } => {
                return Ok(Reply::Actions(self.store.actions_after(
                    *after,
                    (*limit).clamp(1, self.config.limits.history_page),
                )?));
            }
            Command::ReadArtifact { id, offset, limit } => {
                return Ok(Reply::Artifact(
                    self.store.read_artifact(id, *offset, *limit)?,
                ));
            }
            Command::ReadArtifactBefore { id, before, limit } => {
                let (offset, chunk) = self.store.read_artifact_before(id, *before, *limit)?;
                return Ok(Reply::ArtifactPage { offset, chunk });
            }
            Command::History { before, limit } => {
                return Ok(Reply::History(self.store.history(
                    *before,
                    (*limit).clamp(1, self.config.limits.history_page),
                )?));
            }
            Command::Transcript {
                before,
                after,
                limit,
            } => {
                return Ok(Reply::History(self.store.transcript(
                    *before,
                    *after,
                    (*limit).clamp(1, self.config.limits.history_page),
                )?));
            }
            Command::SearchHistory { query, limit } => {
                return Ok(Reply::HistoryMatches(
                    self.store.search_history(query, *limit)?,
                ));
            }
            Command::TranscriptAnchor { seq, limit } => {
                return Ok(Reply::HistoryAnchor {
                    seq: *seq,
                    entries: self.store.transcript_anchor(
                        *seq,
                        (*limit).clamp(1, self.config.limits.history_page),
                    )?,
                });
            }
            Command::NativeTranscript {
                before,
                after,
                limit,
            } => {
                return Ok(Reply::NativeTranscript(self.store.native_transcript(
                    *before,
                    *after,
                    (*limit).clamp(1, self.config.limits.history_page),
                )?));
            }
            Command::Inputs { limit } => {
                return Ok(Reply::Inputs(self.store.inputs((*limit).clamp(1, 100))?));
            }
            Command::Events { after, limit } => {
                return Ok(Reply::Events(
                    self.store.events(*after, (*limit).clamp(1, 200))?,
                ));
            }
            _ => {}
        }
        if let Some(reply) = self.store.previous(id, command)? {
            return Ok(reply);
        }
        if self.snapshot.phase == Phase::StorageError && !matches!(command, Command::Shutdown) {
            bail!("storage failed; session is paused, restart after fixing storage");
        }
        if let Command::StopPending {
            expected_attempt_id,
            expected_paused,
        } = command
        {
            self.store
                .check_stop_pending(expected_attempt_id.as_deref(), *expected_paused)?;
            ensure!(
                self.active.as_ref().map(|a| a.view.attempt_id.as_str())
                    == expected_attempt_id.as_deref(),
                "selected attempt is no longer active"
            );
            // Quiesce polling, not the producer itself. Rejection releases the gate
            // and leaves work running; commit includes every buffered delta.
            let _gate = self
                .active
                .as_ref()
                .map(|a| self.handle.block_on(a.producer_gate.clone().lock_owned()));
            if let Some(active) = &mut self.active {
                active.drain(&mut self.store)?;
            }
            let completion = self.active.as_ref().map(|a| Completion {
                attempt_id: &a.view.attempt_id,
                text: &a.view.text,
                status: "cancelled",
            });
            let reply = self.store.apply(id, command, &self.config, completion)?;
            if let Some(active) = &mut self.active {
                active.stop(&self.handle);
                self.active = None;
                self.phase(Phase::Idle);
            }
            self.snapshot.last_event = Instant::now();
            self.refresh()?;
            return Ok(reply);
        }
        if matches!(
            command,
            Command::Submit {
                mode: DeliveryMode::Force,
                ..
            } | Command::SubmitInput {
                mode: DeliveryMode::Force,
                ..
            } | Command::SubmitInputAccumulated {
                mode: DeliveryMode::Force,
                ..
            } | Command::DispatchDraft {
                mode: DeliveryMode::Force,
                ..
            } | Command::DispatchDraftChecked {
                mode: DeliveryMode::Force,
                ..
            }
        ) {
            // Capture a FIFO cutoff before admission. Ordinary progress is allowed
            // on rejection, but producer cancellation waits for the dispatch commit.
            let cutoff = self.active.as_ref().map_or(0, |a| a.updates.len());
            self.poll_provider_limit(cutoff, false)?;
        }
        let stopping = match command {
            Command::Interrupt { attempt_id } => {
                ensure!(
                    self.active
                        .as_ref()
                        .is_some_and(|a| &a.view.attempt_id == attempt_id),
                    "selected attempt is no longer active"
                );
                true
            }
            Command::Stop | Command::Shutdown => self.active.is_some(),
            Command::Submit {
                mode: DeliveryMode::Force,
                resume,
                ..
            }
            | Command::DispatchDraft {
                mode: DeliveryMode::Force,
                resume,
                ..
            }
            | Command::SubmitInput {
                mode: DeliveryMode::Force,
                resume,
                ..
            }
            | Command::SubmitInputAccumulated {
                mode: DeliveryMode::Force,
                resume,
                ..
            }
            | Command::DispatchDraftChecked {
                mode: DeliveryMode::Force,
                resume,
                ..
            } => self.active.is_some() && (!self.snapshot.paused || *resume),
            _ => false,
        };
        if stopping {
            let active = self.active.as_mut().unwrap();
            // Do not cancel a producer until dispatch validation and its receipt commit.
            // Its bounded channel retains pending output while this actor writes SQLite.
            if !matches!(
                command,
                Command::Submit { .. }
                    | Command::DispatchDraft { .. }
                    | Command::SubmitInput { .. }
                    | Command::SubmitInputAccumulated { .. }
                    | Command::DispatchDraftChecked { .. }
            ) {
                active.stop(&self.handle);
                active.drain(&mut self.store)?;
            }
        }
        let completion = if stopping {
            self.active.as_ref().map(|a| Completion {
                attempt_id: &a.view.attempt_id,
                text: &a.view.text,
                status: "cancelled",
            })
        } else {
            None
        };
        let reply = self.store.apply(id, command, &self.config, completion)?;
        if stopping {
            self.active.as_mut().unwrap().stop(&self.handle);
            self.active = None;
            self.phase(Phase::Idle);
        }
        if matches!(
            command,
            Command::Submit {
                mode: DeliveryMode::Steer,
                ..
            } | Command::DispatchDraft {
                mode: DeliveryMode::Steer,
                ..
            } | Command::SubmitInput {
                mode: DeliveryMode::Steer,
                ..
            } | Command::SubmitInputAccumulated {
                mode: DeliveryMode::Steer,
                ..
            } | Command::DispatchDraftChecked {
                mode: DeliveryMode::Steer,
                ..
            }
        ) && !self.store.paused()?
            && let Some(active) = &self.active
        {
            active.steer.send_replace(true);
        }
        if matches!(command, Command::Shutdown) {
            self.phase(Phase::Stopping);
            self.shutdown_committed = true;
        }
        self.snapshot.last_event = Instant::now();
        self.refresh()?;
        Ok(reply)
    }

    fn start_next(&mut self) -> Result<bool> {
        if self.active.is_some()
            || self.snapshot.paused
            || self.snapshot.phase == Phase::StorageError
        {
            return Ok(false);
        }
        let Some(started) = self.store.start_next(&self.config)? else {
            return Ok(false);
        };
        let (sender, updates) = mpsc::channel(provider::UPDATE_CAPACITY);
        let (cancel, cancelled) = watch::channel(false);
        let (steer, steering) = watch::channel(false);
        let producer_gate = Arc::new(Mutex::new(()));
        let gate = producer_gate.clone();
        let mut lock = Box::pin(gate.clone().lock_owned());
        let mut producer = Box::pin(provider::run_controlled(
            started.config.provider,
            sender,
            cancelled,
            steering,
        ));
        // Async locking lets a checked stop hold off producer polls across SQLite
        // commit without blocking an executor worker or cancelling on rejection.
        let task = self.handle.spawn(std::future::poll_fn(move |cx| {
            let guard = std::task::ready!(lock.as_mut().poll(cx));
            let result = producer.as_mut().poll(cx);
            drop(guard);
            lock = Box::pin(gate.clone().lock_owned());
            result
        }));
        self.active = Some(Active {
            view: ActiveView {
                attempt_id: started.attempt_id,
                message_id: started.message_id,
                text: String::new(),
                truncated: false,
                durable_bytes: 0,
            },
            updates,
            cancel,
            task: Some(task),
            checkpoint: Instant::now(),
            steer,
            limits: started.config.limits,
            pending_status: None,
            producer_gate,
        });
        self.phase(Phase::Waiting);
        self.snapshot.error = None;
        self.snapshot.last_event = Instant::now();
        self.refresh()?;
        Ok(true)
    }

    fn poll_provider(&mut self) -> Result<bool> {
        self.poll_provider_limit(provider::UPDATE_CAPACITY, true)
    }

    fn poll_provider_limit(&mut self, limit: usize, finalize: bool) -> Result<bool> {
        if self.snapshot.phase == Phase::StorageError {
            return Ok(false);
        }
        let mut changed = false;
        let Some(active) = &mut self.active else {
            return Ok(false);
        };
        let mut finished = active.pending_status.take();
        let count = if finalize && finished.is_some() {
            0
        } else {
            limit.min(provider::UPDATE_CAPACITY)
        };
        for _ in 0..count {
            match active.updates.try_recv() {
                Ok(Update::Delta(text)) => {
                    self.store
                        .append_output(&active.view.attempt_id, &text, &active.limits)?;
                    let producer_reserve =
                        (provider::UPDATE_CAPACITY + 1) * provider::MAX_DELTA_BYTES;
                    if active.view.text.len() - active.view.durable_bytes + text.len()
                        > provider::UNDURABLE_BYTES - producer_reserve
                    {
                        self.store
                            .checkpoint(&active.view.attempt_id, &active.view.text)?;
                        active.view.durable_bytes = active.view.text.len();
                        active.checkpoint = Instant::now();
                    }
                    active.append(&text, active.limits.response_bytes);
                    changed = true;
                    if active.view.truncated {
                        finished.get_or_insert("truncated");
                        if finalize {
                            break;
                        }
                    }
                }
                Ok(Update::Finished) => {
                    finished.get_or_insert("completed");
                    if finalize {
                        break;
                    }
                }
                Ok(Update::Steered) => {
                    finished.get_or_insert("steered");
                    if finalize {
                        break;
                    }
                }
                Ok(update @ (Update::ToolStarted | Update::ToolFinished)) => {
                    self.store.tool_boundary(
                        &active.view.attempt_id,
                        matches!(update, Update::ToolStarted),
                    )?;
                    changed = true;
                }
                Ok(Update::Failed(error)) => {
                    self.snapshot.error = Some(error);
                    finished.get_or_insert("failed");
                    if finalize {
                        break;
                    }
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if finished.is_none() {
                        self.snapshot.error = Some("fixture producer disconnected".into());
                        finished = Some("failed");
                    }
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
            }
        }
        // Rejected force admission leaves finalization to the next ordinary poll.
        active.pending_status = finished;
        if finalize && let Some(status) = finished {
            active.stop(&self.handle);
            self.store.finish_detail(
                Completion {
                    attempt_id: &active.view.attempt_id,
                    text: &active.view.text,
                    status,
                },
                if status == "failed" {
                    self.snapshot.error.as_deref()
                } else {
                    None
                },
            )?;
            self.active = None;
            self.phase(Phase::Idle);
            self.snapshot.last_event = Instant::now();
            self.refresh()?;
            return Ok(true);
        }
        if active.view.text.len() != active.view.durable_bytes
            && active.checkpoint.elapsed() >= Duration::from_millis(active.limits.checkpoint_ms)
        {
            self.store
                .checkpoint(&active.view.attempt_id, &active.view.text)?;
            active.view.durable_bytes = active.view.text.len();
            active.checkpoint = Instant::now();
            changed = true;
        }
        if changed {
            self.phase(Phase::Receiving);
            self.snapshot.last_event = Instant::now();
            self.refresh()?;
        }
        Ok(changed)
    }
}

fn actor(
    path: PathBuf,
    config: Config,
    mut commands: mpsc::Receiver<Request>,
    ready: oneshot::Sender<Result<(watch::Receiver<Snapshot>, usize)>>,
    handle: Handle,
) -> Result<()> {
    let store = match Store::open(&path, &config) {
        Ok(store) => store,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    // Permit exact replay of previously admitted text after limits are lowered.
    // Storage still applies the current size limit to every new mutation.
    let message_bytes = match store.receipt_message_bytes() {
        Ok(bytes) => bytes.max(config.limits.message_bytes),
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let now = Instant::now();
    let snapshot = Snapshot {
        session_id: store.session_id.clone(),
        agent_id: store.agent_id.clone(),
        conversation_id: store.conversation_id.clone(),
        phase: Phase::Idle,
        phase_started: now,
        last_event: now,
        active: None,
        queue: Vec::new(),
        paused: store.reopened,
        recovered: store.reopened,
        cursor: 0,
        error: None,
    };
    let mut engine = Engine {
        store,
        config,
        snapshot,
        active: None,
        handle,
        shutdown_committed: false,
    };
    if let Err(error) = engine.refresh() {
        let _ = ready.send(Err(error));
        return Ok(());
    }
    let (snapshots, receiver) = watch::channel(engine.snapshot.clone());
    if ready.send(Ok((receiver, message_bytes))).is_err() {
        return Ok(());
    }
    loop {
        for _ in 0..16 {
            let request = match commands.try_recv() {
                Ok(request) => request,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    let result = engine.command(&Uuid::new_v4().to_string(), &Command::Shutdown);
                    return result.map(|_| ());
                }
            };
            let mutation = !matches!(
                request.command,
                Command::History { .. }
                    | Command::Transcript { .. }
                    | Command::SearchHistory { .. }
                    | Command::TranscriptAnchor { .. }
                    | Command::NativeTranscript { .. }
                    | Command::Inputs { .. }
                    | Command::Events { .. }
                    | Command::Drafts
                    | Command::Composer
                    | Command::Actions { .. }
                    | Command::ActionsAfter { .. }
                    | Command::ReadArtifact { .. }
                    | Command::ReadArtifactBefore { .. }
            );
            let result = engine.command(&request.id, &request.command);
            let mut publish = mutation;
            if let Err(error) = &result
                && (error.downcast_ref::<rusqlite::Error>().is_some()
                    || error.downcast_ref::<std::io::Error>().is_some())
            {
                engine.fault(error);
                publish = true;
            }
            if publish {
                snapshots.send_replace(engine.snapshot.clone());
            }
            let _ = request.reply.send(result);
            if engine.shutdown_committed {
                return Ok(());
            }
        }
        // Command changes were already published before their ACKs. Only new
        // producer/dequeue progress or a fault belongs to this publication.
        let changed = match engine
            .poll_provider()
            .and_then(|progress| engine.start_next().map(|started| progress || started))
        {
            Ok(progress) => progress,
            Err(error) => {
                engine.fault(&error);
                true
            }
        };
        if changed {
            snapshots.send_replace(engine.snapshot.clone());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checked_stop_quiesces_buffered_output_and_rejected_commits_leave_producer_running() {
        let handle = Handle::current();
        tokio::task::spawn_blocking(move || {
            for failure in [None, Some("paused"), Some("attempt"), Some("transaction")] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("session.sqlite3");
                let mut config = Config::default();
                config.provider.scenario = crate::config::Scenario::Burst;
                config.provider.first_event_ms = 0;
                config.provider.chunks = 1000;
                config.limits.response_bytes = 5;
                let mut store = Store::open(&path, &config).unwrap();
                store.apply("send", &Command::Send { text: "active".into() }, &config, None).unwrap();
                let now = Instant::now();
                let snapshot = Snapshot {
                    session_id: store.session_id.clone(),
                    agent_id: store.agent_id.clone(),
                    conversation_id: store.conversation_id.clone(),
                    phase: Phase::Idle,
                    phase_started: now,
                    last_event: now,
                    active: None,
                    queue: Vec::new(),
                    paused: false,
                    recovered: false,
                    cursor: 0,
                    error: None,
                };
                let mut engine = Engine {
                    store, config, snapshot, active: None,
                    handle: handle.clone(), shutdown_committed: false,
                };
                engine.start_next().unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while engine.active.as_ref().unwrap().updates.len() < provider::UPDATE_CAPACITY {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                let active = engine.active.as_ref().unwrap();
                let attempt = active.view.attempt_id.clone();
                let cancelled = active.cancel.subscribe();
                // Keep the channel full until the command acquires its own gate.
                let gate = handle.block_on(active.producer_gate.clone().lock_owned());
                let expected = (0..provider::UPDATE_CAPACITY)
                    .map(|i| format!("fixture block {i:06}: bounded streaming output.\n"))
                    .collect::<String>();
                let db = rusqlite::Connection::open(&path).unwrap();
                if failure == Some("transaction") {
                    db.execute_batch("CREATE TRIGGER fail_stop BEFORE INSERT ON command_results BEGIN SELECT RAISE(ABORT,'stop fault'); END;").unwrap();
                }
                let command = Command::StopPending {
                    expected_attempt_id: Some(if failure == Some("attempt") { "stale".into() } else { attempt.clone() }),
                    expected_paused: failure == Some("paused"),
                };
                let cursor = engine.store.cursor().unwrap();
                drop(gate);
                let result = engine.command("stop", &command);
                if failure.is_none() {
                    assert!(matches!(result.unwrap(), Reply::Stopped { paused: true, cancelled: true }));
                    assert!(*cancelled.borrow());
                    assert!(engine.active.is_none());
                    assert!(engine.snapshot.paused);
                    assert!(engine.snapshot.active.is_none());
                    let history = engine.store.history(None, 10).unwrap();
                    assert_eq!(history[1].text, expected[..5]);
                    assert_eq!(history[1].status, "cancelled");
                } else {
                    assert!(result.is_err());
                    assert!(!*cancelled.borrow());
                    assert!(engine.active.as_ref().unwrap().task.is_some());
                    assert_eq!(engine.store.cursor().unwrap(), cursor);
                    assert!(!engine.store.paused().unwrap());
                    assert!(engine.store.previous("stop", &command).unwrap().is_none());
                    if failure == Some("transaction") {
                        assert_eq!(engine.active.as_ref().unwrap().view.text, expected[..5]);
                        db.execute_batch("DROP TRIGGER fail_stop").unwrap();
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while engine.active.as_ref().unwrap().updates.len() < provider::UPDATE_CAPACITY {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    engine.command("shutdown", &Command::Shutdown).unwrap();
                }
                let output = engine.store.actions(None, 10).unwrap()[0].output.clone().unwrap();
                assert!(output.incomplete);
                assert!(engine.store.read_artifact(&output.id, 0, 8192).unwrap().text.starts_with(&expected));
            }
        }).await.unwrap();
    }

    #[test]
    fn cancellation_drain_preserves_buffered_output_and_uses_frozen_response_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut frozen = Config::default();
        frozen.limits.response_bytes = 5;
        let mut store = Store::open(&path, &frozen).unwrap();
        store
            .apply(
                "send",
                &Command::Send {
                    text: "fixture".into(),
                },
                &frozen,
                None,
            )
            .unwrap();
        let started = store.start_next(&Config::default()).unwrap().unwrap();
        store
            .append_output(&started.attempt_id, "ab", &started.config.limits)
            .unwrap();
        let (sender, updates) = mpsc::channel(provider::UPDATE_CAPACITY);
        sender.try_send(Update::Delta("cdefgh".into())).unwrap();
        let (cancel, _) = watch::channel(false);
        let (steer, _) = watch::channel(false);
        let mut active = Active {
            view: ActiveView {
                attempt_id: started.attempt_id,
                message_id: started.message_id,
                text: "ab".into(),
                truncated: false,
                durable_bytes: 0,
            },
            updates,
            cancel,
            task: None,
            checkpoint: Instant::now(),
            steer,
            limits: started.config.limits,
            pending_status: None,
            producer_gate: Arc::new(Mutex::new(())),
        };
        active.drain(&mut store).unwrap();
        assert_eq!(active.view.text, "abcde");
        assert!(active.view.truncated);
        store
            .finish(Completion {
                attempt_id: &active.view.attempt_id,
                text: &active.view.text,
                status: "cancelled",
            })
            .unwrap();
        let output = store.actions(None, 1).unwrap()[0].output.clone().unwrap();
        assert!(output.incomplete);
        assert_eq!(output.bytes, 8);
        assert_eq!(
            store.read_artifact(&output.id, 0, 100).unwrap().text,
            "abcdefgh"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_admission_drains_a_burst_cutoff_without_cancelling_rejected_dispatches() {
        let handle = Handle::current();
        tokio::task::spawn_blocking(move || {
            for (failure, response_bytes) in [
                (None, 64 * 1024),
                (None, 67),
                (Some("revision"), 64 * 1024),
                (Some("revision"), 67),
                (Some("paused"), 64 * 1024),
                (Some("draft"), 64 * 1024),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("session.sqlite3");
                let mut config = Config::default();
                config.provider.scenario = crate::config::Scenario::Burst;
                config.limits.response_bytes = response_bytes;
                config.limits.checkpoint_ms = 5000;
                let mut store = Store::open(&path, &config).unwrap();
                store
                    .apply(
                        "initial",
                        &Command::Send {
                            text: "initial".into(),
                        },
                        &config,
                        None,
                    )
                    .unwrap();
                let started = store.start_next(&config).unwrap().unwrap();
                let attempt = started.attempt_id.clone();
                store
                    .apply(
                        "autosave",
                        &Command::SaveComposer {
                            text: "saved earlier".into(),
                            cursor: 0,
                            revision: 0,
                        },
                        &config,
                        None,
                    )
                    .unwrap();
                let Reply::Draft(draft) = store
                    .apply(
                        "draft",
                        &Command::SaveDraft {
                            text: "retained draft".into(),
                        },
                        &config,
                        None,
                    )
                    .unwrap()
                else {
                    panic!()
                };
                let (sender, updates) = mpsc::channel(provider::UPDATE_CAPACITY);
                let mut buffered = String::new();
                for i in 0..provider::UPDATE_CAPACITY {
                    let text = format!("fixture block {i:06}: bounded streaming output.\n");
                    buffered.push_str(&text);
                    sender.try_send(Update::Delta(text)).unwrap();
                }
                let (cancel, cancelled) = watch::channel(false);
                let (steer, _) = watch::channel(false);
                let producer = sender.clone();
                let (refilled, refill_done) = oneshot::channel();
                let task = handle.spawn(async move {
                    if producer
                        .send(Update::Delta("after cutoff".into()))
                        .await
                        .is_ok()
                    {
                        let _ = refilled.send(());
                        std::future::pending::<()>().await;
                    }
                });
                let now = Instant::now();
                let snapshot = Snapshot {
                    session_id: store.session_id.clone(),
                    agent_id: store.agent_id.clone(),
                    conversation_id: store.conversation_id.clone(),
                    phase: Phase::Waiting,
                    phase_started: now,
                    last_event: now,
                    active: None,
                    queue: Vec::new(),
                    paused: false,
                    recovered: false,
                    cursor: 0,
                    error: None,
                };
                let active = Active {
                    view: ActiveView {
                        attempt_id: attempt.clone(),
                        message_id: started.message_id,
                        text: String::new(),
                        truncated: false,
                        durable_bytes: 0,
                    },
                    updates,
                    cancel,
                    task: Some(task),
                    checkpoint: now,
                    steer,
                    limits: started.config.limits,
                    pending_status: None,
                    producer_gate: Arc::new(Mutex::new(())),
                };
                let mut engine = Engine {
                    store,
                    config,
                    snapshot,
                    active: Some(active),
                    handle: handle.clone(),
                    shutdown_committed: false,
                };
                engine.refresh().unwrap();
                let command = if failure == Some("draft") {
                    Command::DispatchDraftChecked {
                        id: draft.id.clone(),
                        revision: draft.revision + 1,
                        mode: DeliveryMode::Force,
                        resume: false,
                        expected_paused: false,
                    }
                } else {
                    Command::SubmitInput {
                        source: "//replacement".into(),
                        text: "/replacement".into(),
                        composer_revision: if failure == Some("revision") { 0 } else { 1 },
                        mode: DeliveryMode::Force,
                        resume: false,
                        expected_paused: failure == Some("paused"),
                    }
                };
                let result = engine.command("force", &command);
                let output = engine.store.actions(None, 1).unwrap()[0]
                    .output
                    .clone()
                    .unwrap();
                assert_eq!(
                    engine
                        .store
                        .read_artifact(&output.id, 0, 8192)
                        .unwrap()
                        .text,
                    buffered
                );
                if failure.is_none() {
                    assert!(matches!(result.unwrap(), Reply::Ack));
                    assert!(*cancelled.borrow());
                    assert!(engine.active.is_none());
                    assert_eq!(
                        engine.store.history(None, 10).unwrap()[1].text,
                        buffered[..response_bytes.min(buffered.len())]
                    );
                    assert_eq!(
                        engine.store.history(None, 10).unwrap()[1].status,
                        "cancelled"
                    );
                    assert_eq!(engine.store.queue(10).unwrap()[0].text, "/replacement");
                    assert_eq!(engine.store.composer().unwrap().revision, 2);
                    assert!(engine.store.composer().unwrap().text.is_empty());
                } else {
                    assert!(result.is_err());
                    assert!(!*cancelled.borrow());
                    assert_eq!(
                        engine.active.as_ref().unwrap().view.text,
                        buffered[..response_bytes.min(buffered.len())]
                    );
                    assert_eq!(engine.active.as_ref().unwrap().view.attempt_id, attempt);
                    assert!(engine.store.queue(10).unwrap().is_empty());
                    assert_eq!(engine.store.composer().unwrap().text, "saved earlier");
                    assert_eq!(engine.store.drafts().unwrap(), std::slice::from_ref(&draft));
                    assert!(!engine.store.paused().unwrap());
                    assert!(engine.store.previous("force", &command).unwrap().is_none());
                    handle.block_on(async {
                        tokio::time::timeout(Duration::from_secs(1), refill_done)
                            .await
                            .unwrap()
                            .unwrap()
                    });
                    engine.poll_provider().unwrap();
                    if response_bytes < buffered.len() {
                        assert!(engine.active.is_none());
                        assert_eq!(
                            engine.store.history(None, 10).unwrap()[1].status,
                            "truncated"
                        );
                    } else {
                        assert!(
                            engine
                                .active
                                .as_ref()
                                .unwrap()
                                .view
                                .text
                                .ends_with("after cutoff")
                        );
                    }
                }
                engine.command("shutdown", &Command::Shutdown).unwrap();
            }
        })
        .await
        .unwrap();
    }
}

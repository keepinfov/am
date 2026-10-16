use crate::{
    artifacts::{ArtifactChunk, ArtifactFiles},
    config::{Config, FakeConfig, Limits},
    runtime::{
        ActionRecord, Command, ComposerState, DraftRecord, HistoryEntry, HistoryMatch,
        JournalEvent, MAX_HISTORY_MATCHES, MAX_HISTORY_PREVIEW_CHARS, MAX_NOTICE_BYTES,
        NativeEntry, NoticeLevel, QueueItem, QueueState, Reply, input_payload, validate_action,
        validate_history_query, validate_input, validate_notice,
    },
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
use uuid::Uuid;

pub struct Store {
    connection: Connection,
    _lock: File,
    pub session_id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub reopened: bool,
    wal_path: std::path::PathBuf,
    transaction_reserve: u64,
    artifacts: ArtifactFiles,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FrozenConfig {
    pub provider: FakeConfig,
    pub limits: Limits,
}

pub struct Started {
    pub attempt_id: String,
    pub message_id: String,
    pub config: FrozenConfig,
}

pub struct Completion<'a> {
    pub attempt_id: &'a str,
    pub text: &'a str,
    pub status: &'a str,
}

fn id() -> String {
    Uuid::new_v4().to_string()
}

fn guard_wal(connection: &Connection, path: &Path, reserve: u64) -> Result<()> {
    let bytes = match path.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                Some(error.to_string()),
            )
            .into());
        }
    };
    if bytes <= reserve {
        return Ok(());
    }
    // Passive auto-checkpoints are not a disk quota: an old reader can pin WAL.
    // Stop before another transaction when truncation cannot reclaim the reserve.
    let busy: i64 = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    if busy != 0 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("WAL budget exhausted; close external SQLite readers and restart".into()),
        )
        .into());
    }
    Ok(())
}

fn event(tx: &Transaction<'_>, kind: &str, attempt: Option<&str>, payload: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO events(kind,attempt_id,payload) VALUES(?1,?2,?3)",
        params![kind, attempt, payload],
    )?;
    Ok(())
}

fn notice(
    tx: &Transaction<'_>,
    text: &str,
    level: NoticeLevel,
    attempt: Option<&str>,
    category: Option<&str>,
) -> Result<HistoryEntry> {
    let status = match level {
        NoticeLevel::Info => "info",
        NoticeLevel::Error => "error",
        NoticeLevel::Status => "status",
    };
    tx.execute(
        "INSERT INTO history(attempt_id,kind,text,status) VALUES(?1,'notice',?2,?3)",
        params![attempt, text, status],
    )?;
    let seq = tx.last_insert_rowid();
    action(tx, category.unwrap_or(status), text, status, None, None)?;
    event(tx, "notice_recorded", attempt, status)?;
    Ok(HistoryEntry {
        seq,
        attempt_id: attempt.unwrap_or_default().to_owned(),
        kind: "notice".into(),
        text: text.to_owned(),
        status: status.into(),
    })
}

fn action(
    tx: &Transaction<'_>,
    category: &str,
    label: &str,
    status: &str,
    output: Option<&str>,
    attempt: Option<&str>,
) -> Result<()> {
    let last: Option<(String, String)> = tx
        .query_row(
            "SELECT category,group_id FROM actions WHERE seq>(SELECT action_seq FROM action_group_boundary WHERE singleton=1) ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let group = match last {
        Some((previous, group)) if previous == category => group,
        _ => id(),
    };
    tx.execute("INSERT INTO actions(id,group_id,category,label,status,output_id,attempt_id) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id(), group, category, label, status, output, attempt])?;
    Ok(())
}

fn group_boundary(tx: &Transaction<'_>) -> Result<()> {
    // Conversation transitions break adjacent local groups without inspecting labels.
    tx.execute("UPDATE action_group_boundary SET action_seq=(SELECT COALESCE(MAX(seq),0) FROM actions) WHERE singleton=1", [])?;
    Ok(())
}

fn save_output_text(
    tx: &Transaction<'_>,
    files: &mut ArtifactFiles,
    attempt: &str,
    text: &str,
) -> Result<Option<String>> {
    let output: Option<String> = tx
        .query_row(
            "SELECT id FROM artifacts WHERE attempt_id=?1",
            [attempt],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(output) = &output {
        let bytes = files.record(output, true)?.bytes as usize;
        if bytes < text.len() && text.is_char_boundary(bytes) {
            let frozen: String =
                tx.query_row("SELECT config FROM attempts WHERE id=?1", [attempt], |r| {
                    r.get(0)
                })?;
            let frozen: FrozenConfig = toml::from_str(&frozen)?;
            files.append(
                output,
                &text[bytes..],
                frozen.limits.output_bytes as u64,
                frozen.limits.artifact_bytes,
            )?;
        }
        files.sync(output)?;
    }
    Ok(output)
}

fn complete(
    tx: &Transaction<'_>,
    files: &mut ArtifactFiles,
    completion: Completion<'_>,
) -> Result<()> {
    ensure!(
        tx.execute(
            "UPDATE attempts SET status=?2 WHERE id=?1 AND status='running'",
            params![completion.attempt_id, completion.status]
        )? == 1,
        "attempt is no longer active"
    );
    tx.execute(
        "UPDATE history SET text=?2,status=?3 WHERE attempt_id=?1 AND kind='assistant'",
        params![completion.attempt_id, completion.text, completion.status],
    )?;
    tx.execute("UPDATE messages SET state='handled',revision=revision+1 WHERE id=(SELECT message_id FROM attempts WHERE id=?1)", [completion.attempt_id])?;
    let output = save_output_text(tx, files, completion.attempt_id, completion.text)?;
    if let Some(output) = output {
        let record = files.record(&output, completion.status != "completed")?;
        tx.execute(
            "UPDATE artifacts SET bytes=?2,incomplete=?3 WHERE id=?1",
            params![output, record.bytes as i64, record.incomplete],
        )?;
        tx.execute(
            "UPDATE actions SET status=?2 WHERE attempt_id=?1",
            params![completion.attempt_id, completion.status],
        )?;
    }
    group_boundary(tx)?;
    event(
        tx,
        "attempt_finished",
        Some(completion.attempt_id),
        completion.status,
    )
}

fn check_stop_pending(
    connection: &Connection,
    expected_attempt_id: Option<&str>,
    expected_paused: bool,
) -> Result<()> {
    ensure!(
        expected_attempt_id.is_none_or(|id| !id.is_empty() && id.len() <= 128),
        "invalid target ID"
    );
    let paused: bool = connection.query_row("SELECT paused FROM session", [], |r| r.get(0))?;
    ensure!(paused == expected_paused, "paused state conflict");
    let active: Option<String> = connection
        .query_row("SELECT id FROM attempts WHERE status='running'", [], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(
        active.as_deref() == expected_attempt_id,
        "selected attempt is no longer active"
    );
    Ok(())
}

impl Store {
    pub fn open(path: &Path, config: &Config) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        ensure!(
            parent.is_dir(),
            "session parent directory does not exist: {}",
            parent.display()
        );
        // Canonical parent avoids two lock names for a relative/absolute path alias.
        let parent = parent.canonicalize()?;
        let path = parent.join(path.file_name().context("session path needs a file name")?);
        ensure!(
            !path.is_symlink(),
            "symlink session files are not supported"
        );
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = std::path::PathBuf::from(lock_name);
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&lock_path)?;
        lock.try_lock_exclusive()
            .context("session already has a writer (do not delete the lock file)")?;
        let file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                file.metadata()?.nlink() == 1,
                "hard-linked session files are not supported"
            );
        }
        drop(file);
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        ensure!(mode == "wal", "WAL unavailable");
        connection.pragma_update(None, "synchronous", "FULL")?;
        let sync: i64 = connection.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        ensure!(sync == 2, "SQLite FULL synchronous unavailable");
        let page_size: i64 = connection.pragma_query_value(None, "page_size", |r| r.get(0))?;
        let pages: i64 = connection.pragma_query_value(None, "page_count", |r| r.get(0))?;
        let max_pages = config.limits.storage_bytes as i64 / page_size;
        ensure!(
            pages <= max_pages,
            "existing session exceeds storage_bytes quota"
        );
        connection.pragma_update(None, "max_page_count", max_pages)?;
        connection.pragma_update(None, "wal_autocheckpoint", 128)?;
        connection.pragma_update(None, "journal_size_limit", 1024 * 1024)?;
        let mut wal_name = path.as_os_str().to_os_string();
        wal_name.push("-wal");
        let wal_path = std::path::PathBuf::from(wal_name);
        let transaction_reserve =
            (4 * (config.limits.response_bytes + config.limits.message_bytes) as u64 + 256 * 1024)
                .max(1024 * 1024);
        guard_wal(&connection, &wal_path, transaction_reserve)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 2, "unsupported newer session schema {version}");
        if version == 0 {
            let tx = connection.transaction()?;
            tx.execute_batch("
                CREATE TABLE session(id TEXT PRIMARY KEY,agent_id TEXT NOT NULL,conversation_id TEXT NOT NULL,paused INTEGER NOT NULL);
                CREATE TABLE messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,agent_id TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,revision INTEGER NOT NULL);
                CREATE TABLE attempts(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id),status TEXT NOT NULL,config TEXT NOT NULL);
                CREATE TABLE history(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT REFERENCES attempts(id),kind TEXT NOT NULL,text TEXT NOT NULL,status TEXT NOT NULL,CHECK(attempt_id IS NOT NULL OR kind='notice'));
                CREATE INDEX history_attempt ON history(attempt_id);
                CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT,kind TEXT NOT NULL,attempt_id TEXT,payload TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
                CREATE TABLE command_results(id TEXT PRIMARY KEY,command TEXT NOT NULL,reply TEXT NOT NULL);
                PRAGMA user_version=2;
            ")?;
            tx.commit()?;
        } else if version == 1 {
            let tx = connection.transaction()?;
            let sequence: Option<i64> = tx
                .query_row(
                    "SELECT seq FROM sqlite_sequence WHERE name='history'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let indexes = tx
                .prepare("SELECT sql FROM sqlite_schema WHERE type='index' AND tbl_name='history' AND sql IS NOT NULL ORDER BY name")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // Rebuilding is required to remove NOT NULL; retain even deleted seqs.
            tx.execute_batch("
                CREATE TABLE history_v2(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT REFERENCES attempts(id),kind TEXT NOT NULL,text TEXT NOT NULL,status TEXT NOT NULL,CHECK(attempt_id IS NOT NULL OR kind='notice'));
                INSERT INTO history_v2 SELECT seq,attempt_id,kind,text,status FROM history ORDER BY seq;
                DROP TABLE history;
                ALTER TABLE history_v2 RENAME TO history;
            ")?;
            for sql in indexes {
                tx.execute_batch(&sql)?;
            }
            if let Some(sequence) = sequence {
                tx.execute("DELETE FROM sqlite_sequence WHERE name='history'", [])?;
                tx.execute(
                    "INSERT INTO sqlite_sequence(name,seq) VALUES('history',?1)",
                    [sequence],
                )?;
            }
            ensure!(
                tx.prepare("PRAGMA foreign_key_check")?
                    .query([])?
                    .next()?
                    .is_none(),
                "session migration failed foreign key check"
            );
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }
        // Additive feature tables leave the v1/v2 history migration and high-water
        // marks untouched, including databases created by older offline clients.
        let tx = connection.transaction()?;
        tx.execute_batch("
            CREATE INDEX IF NOT EXISTS command_results_notice_seq ON command_results(json_extract(reply,'$.Notice.seq')) WHERE json_valid(reply);
            CREATE TABLE IF NOT EXISTS drafts(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,text TEXT NOT NULL,revision INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS composer(singleton INTEGER PRIMARY KEY CHECK(singleton=1),text TEXT NOT NULL,cursor INTEGER NOT NULL,revision INTEGER NOT NULL);
            INSERT OR IGNORE INTO composer VALUES(1,'',0,0);
            CREATE TABLE IF NOT EXISTS dispatch_configs(message_id TEXT PRIMARY KEY REFERENCES messages(id),config TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY,attempt_id TEXT UNIQUE REFERENCES attempts(id),bytes INTEGER NOT NULL,incomplete INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS actions(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,group_id TEXT NOT NULL,category TEXT NOT NULL,label TEXT NOT NULL,status TEXT NOT NULL,output_id TEXT REFERENCES artifacts(id),attempt_id TEXT REFERENCES attempts(id));
            CREATE TABLE IF NOT EXISTS action_group_boundary(singleton INTEGER PRIMARY KEY CHECK(singleton=1),action_seq INTEGER NOT NULL);
            INSERT INTO action_group_boundary
                SELECT 1,CASE WHEN
                    COALESCE((SELECT MAX(seq) FROM events WHERE kind IN ('attempt_started','attempt_finished')),0) >
                    COALESCE((SELECT MAX(seq) FROM events WHERE kind='notice_recorded'),0)
                THEN COALESCE((SELECT MAX(seq) FROM actions),0) ELSE 0 END
                WHERE NOT EXISTS(SELECT 1 FROM action_group_boundary WHERE singleton=1);
        ")?;
        // Existing unsent messages have no dispatch snapshot. Freeze them once at
        // upgrade rather than changing their configuration on every later reopen.
        tx.execute("INSERT OR IGNORE INTO dispatch_configs SELECT id,?1 FROM messages WHERE state IN ('queued','editing')", [toml::to_string(&config.attempt_config())?])?;
        tx.commit()?;
        let mut artifacts = ArtifactFiles::open(&path)?;
        let existing = connection
            .query_row("SELECT id,agent_id,conversation_id FROM session", [], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()?;
        let reopened = existing.is_some();
        let (session_id, agent_id, conversation_id) =
            existing.unwrap_or_else(|| (id(), id(), id()));
        let tx = connection.transaction()?;
        if reopened {
            let running = tx.prepare("SELECT a.id,h.text,a.config FROM attempts a JOIN history h ON h.attempt_id=a.id AND h.kind='assistant' WHERE a.status='running'")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (attempt, text, frozen) in running {
                group_boundary(&tx)?;
                let output: Option<String> = tx
                    .query_row(
                        "SELECT id FROM artifacts WHERE attempt_id=?1",
                        [&attempt],
                        |r| r.get(0),
                    )
                    .optional()?;
                let output = if let Some(output) = output {
                    output
                } else {
                    let output = id();
                    artifacts.create(&output)?;
                    tx.execute(
                        "INSERT INTO artifacts VALUES(?1,?2,0,1)",
                        params![output, attempt],
                    )?;
                    action(
                        &tx,
                        "fixture",
                        "Fixture result",
                        "running",
                        Some(&output),
                        Some(&attempt),
                    )?;
                    output
                };
                // Old sessions only have a durable transcript checkpoint. Invalid
                // legacy diagnostic snapshots must remain preserved verbatim.
                if artifacts.record(&output, true)?.bytes == 0 && !text.is_empty() {
                    let limits = toml::from_str::<FrozenConfig>(&frozen)
                        .map(|frozen| frozen.limits)
                        .unwrap_or_else(|_| config.limits.clone());
                    artifacts.append(
                        &output,
                        &text,
                        limits.output_bytes as u64,
                        limits.artifact_bytes,
                    )?;
                }
                artifacts.sync(&output)?;
                let record = artifacts.record(&output, true)?;
                tx.execute(
                    "UPDATE artifacts SET bytes=?2,incomplete=1 WHERE id=?1",
                    params![output, record.bytes as i64],
                )?;
                tx.execute(
                    "UPDATE actions SET status='interrupted' WHERE attempt_id=?1",
                    [&attempt],
                )?;
            }
            tx.execute("UPDATE history SET status='interrupted' WHERE attempt_id IN (SELECT id FROM attempts WHERE status='running') AND kind='assistant'", [])?;
            tx.execute(
                "UPDATE messages SET state='handled',revision=revision+1 WHERE state='processing'",
                [],
            )?;
            tx.execute(
                "UPDATE attempts SET status='interrupted' WHERE status='running'",
                [],
            )?;
            tx.execute(
                "UPDATE messages SET state='queued',revision=revision+1 WHERE state='editing'",
                [],
            )?;
            tx.execute("UPDATE session SET paused=1", [])?;
            event(
                &tx,
                "session_recovered",
                None,
                "paused; unfinished attempts interrupted; editing leases released; no automatic replay",
            )?;
        } else {
            tx.execute(
                "INSERT INTO session VALUES(?1,?2,?3,0)",
                params![session_id, agent_id, conversation_id],
            )?;
            event(&tx, "session_created", None, &agent_id)?;
        }
        for output in artifacts.ids()? {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=?1)",
                [&output],
                |r| r.get(0),
            )?;
            if !exists {
                let record = artifacts.record(&output, true)?;
                tx.execute(
                    "INSERT INTO artifacts VALUES(?1,NULL,?2,1)",
                    params![output, record.bytes as i64],
                )?;
                action(
                    &tx,
                    "recovery",
                    "Uncommitted fixture output recovered",
                    "interrupted",
                    Some(&output),
                    None,
                )?;
            }
        }
        tx.commit()?;
        Ok(Self {
            connection,
            _lock: lock,
            session_id,
            agent_id,
            conversation_id,
            reopened,
            wal_path,
            transaction_reserve,
            artifacts,
        })
    }

    pub fn cursor(&self) -> Result<i64> {
        Ok(self
            .connection
            .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))?)
    }

    pub fn paused(&self) -> Result<bool> {
        Ok(self
            .connection
            .query_row("SELECT paused FROM session", [], |r| r.get(0))?)
    }

    pub(crate) fn receipt_message_bytes(&self) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(length(CAST(COALESCE(
                json_extract(command,'$.Send.text'),
                json_extract(command,'$.Submit.text'),
                json_extract(command,'$.SubmitInput.source'),
                json_extract(command,'$.SubmitInputAccumulated.source'),
                json_extract(command,'$.FinishEdit.text'),
                json_extract(command,'$.FinishPendingEdit.text'),
                json_extract(command,'$.SaveDraft.text'),
                json_extract(command,'$.SaveComposer.text'),'') AS BLOB))),0)
             FROM command_results",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    pub fn drafts(&self) -> Result<Vec<DraftRecord>> {
        Ok(self
            .connection
            .prepare("SELECT id,text,revision FROM drafts ORDER BY seq")?
            .query_map([], |r| {
                Ok(DraftRecord {
                    id: r.get(0)?,
                    text: r.get(1)?,
                    revision: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn composer(&self) -> Result<ComposerState> {
        Ok(self.connection.query_row(
            "SELECT text,cursor,revision FROM composer WHERE singleton=1",
            [],
            |r| {
                Ok(ComposerState {
                    text: r.get(0)?,
                    cursor: r.get::<_, i64>(1)? as usize,
                    revision: r.get(2)?,
                })
            },
        )?)
    }

    pub fn actions(&self, before: Option<i64>, limit: usize) -> Result<Vec<ActionRecord>> {
        self.action_page(before, limit, false)
    }

    pub fn actions_after(&self, after: i64, limit: usize) -> Result<Vec<ActionRecord>> {
        self.action_page(Some(after), limit, true)
    }

    fn action_page(
        &self,
        boundary: Option<i64>,
        limit: usize,
        ascending: bool,
    ) -> Result<Vec<ActionRecord>> {
        let sql = if ascending {
            "SELECT seq,id,group_id,category,label,status,output_id FROM actions WHERE seq>?1 ORDER BY seq ASC LIMIT ?2"
        } else {
            "SELECT seq,id,group_id,category,label,status,output_id FROM actions WHERE (?1 IS NULL OR seq<?1) ORDER BY seq DESC LIMIT ?2"
        };
        let rows = self
            .connection
            .prepare(sql)?
            .query_map(params![boundary, limit.clamp(1, 200) as i64], |r| {
                Ok((
                    ActionRecord {
                        seq: r.get(0)?,
                        id: r.get(1)?,
                        group_id: r.get(2)?,
                        category: r.get(3)?,
                        label: r.get(4)?,
                        status: r.get(5)?,
                        output: None,
                    },
                    r.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut records = Vec::new();
        for (mut record, output) in rows {
            if let Some(output) = output {
                record.output = Some(self.artifact_record(&output)?);
            }
            records.push(record);
        }
        if !ascending {
            records.reverse();
        }
        Ok(records)
    }

    fn artifact_record(&self, id: &str) -> Result<crate::artifacts::ArtifactRecord> {
        let incomplete: bool = self
            .connection
            .query_row("SELECT incomplete FROM artifacts WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .context("unknown artifact ID")?;
        self.artifacts.record(id, incomplete)
    }

    pub fn read_artifact(&self, id: &str, offset: u64, limit: usize) -> Result<ArtifactChunk> {
        self.artifacts
            .read(self.artifact_record(id)?, offset, limit)
    }

    pub fn read_artifact_before(
        &self,
        id: &str,
        before: u64,
        limit: usize,
    ) -> Result<(u64, ArtifactChunk)> {
        self.artifacts
            .read_before(self.artifact_record(id)?, before, limit)
    }

    pub fn append_output(&mut self, attempt: &str, text: &str, limits: &Limits) -> Result<()> {
        let output: String = self.connection.query_row(
            "SELECT r.id FROM artifacts r JOIN attempts a ON a.id=r.attempt_id WHERE a.id=?1 AND a.status='running'",
            [attempt],
            |r| r.get(0),
        ).optional()?.context("attempt is no longer active")?;
        self.artifacts.append(
            &output,
            text,
            limits.output_bytes as u64,
            limits.artifact_bytes,
        )
    }

    pub fn tool_boundary(&mut self, attempt: &str, started: bool) -> Result<()> {
        guard_wal(&self.connection, &self.wal_path, self.transaction_reserve)?;
        let tx = self.connection.transaction()?;
        event(
            &tx,
            if started {
                "fixture_tool_started"
            } else {
                "fixture_tool_finished"
            },
            Some(attempt),
            "simulated local boundary",
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn queue(&self, limit: usize) -> Result<Vec<QueueItem>> {
        let mut statement = self.connection.prepare("SELECT id,text,revision,state FROM messages WHERE state IN ('queued','editing') ORDER BY seq LIMIT ?1")?;
        let items = statement.query_map([limit as i64], |r| {
            Ok(QueueItem {
                id: r.get(0)?,
                text: r.get(1)?,
                revision: r.get(2)?,
                state: if r.get::<_, String>(3)? == "editing" {
                    QueueState::Editing
                } else {
                    QueueState::Queued
                },
            })
        })?;
        Ok(items.collect::<rusqlite::Result<_>>()?)
    }

    pub fn history(&self, before: Option<i64>, limit: usize) -> Result<Vec<HistoryEntry>> {
        let mut statement = self.connection.prepare("SELECT seq,kind,text,status,CASE WHEN kind='notice' THEN COALESCE(attempt_id,'') ELSE attempt_id END FROM history WHERE (?1 IS NULL OR seq<?1) AND status!='running' ORDER BY seq DESC LIMIT ?2")?;
        let items = statement.query_map(params![before, limit as i64], |r| {
            Ok(HistoryEntry {
                seq: r.get(0)?,
                kind: r.get(1)?,
                text: r.get(2)?,
                status: r.get(3)?,
                attempt_id: r.get(4)?,
            })
        })?;
        let mut items = items.collect::<rusqlite::Result<Vec<_>>>()?;
        items.reverse();
        Ok(items)
    }

    pub fn transcript(
        &self,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>> {
        let sql = if after.is_some() {
            "SELECT seq,kind,text,status,CASE WHEN kind='notice' THEN COALESCE(attempt_id,'') ELSE attempt_id END FROM history WHERE (?1 IS NULL OR seq<?1) AND seq>?2 ORDER BY seq ASC LIMIT ?3"
        } else {
            "SELECT seq,kind,text,status,CASE WHEN kind='notice' THEN COALESCE(attempt_id,'') ELSE attempt_id END FROM history WHERE (?1 IS NULL OR seq<?1) AND (?2 IS NULL OR seq>?2) ORDER BY seq DESC LIMIT ?3"
        };
        let mut statement = self.connection.prepare(sql)?;
        let items = statement.query_map(params![before, after, limit as i64], |r| {
            Ok(HistoryEntry {
                seq: r.get(0)?,
                kind: r.get(1)?,
                text: r.get(2)?,
                status: r.get(3)?,
                attempt_id: r.get(4)?,
            })
        })?;
        let mut items = items.collect::<rusqlite::Result<Vec<_>>>()?;
        if after.is_none() {
            items.reverse();
        }
        Ok(items)
    }

    /// Search durable dialogue, not the terminal's small loaded-message cache.
    pub fn search_history(&self, query: &str, limit: usize) -> Result<Vec<HistoryMatch>> {
        validate_history_query(query)?;
        let query = query.to_lowercase();
        let limit = limit.clamp(1, MAX_HISTORY_MATCHES);
        // SQLite's built-in lower/NOCASE are ASCII-only. Rust matching also
        // makes '%' and '_' literal without LIKE escaping or SQL interpolation.
        let mut statement = self.connection.prepare(
            "SELECT seq,kind,text FROM history WHERE kind IN ('user','assistant') ORDER BY seq DESC",
        )?;
        let mut rows = statement.query([])?;
        let mut matches = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let text: String = row.get(2)?;
            if !seq.to_string().contains(&query) && !text.to_lowercase().contains(&query) {
                continue;
            }
            let preview = text
                .chars()
                .filter_map(|ch| {
                    if ch.is_whitespace() {
                        Some(' ')
                    } else if ch.is_control() {
                        None
                    } else {
                        Some(ch)
                    }
                })
                .take(MAX_HISTORY_PREVIEW_CHARS)
                .collect();
            matches.push(HistoryMatch {
                seq,
                kind: row.get(1)?,
                preview,
            });
            if matches.len() == limit {
                break;
            }
        }
        Ok(matches)
    }

    /// The first row is exactly the requested saved entry, never a nearby row.
    pub fn transcript_anchor(&self, seq: i64, limit: usize) -> Result<Vec<HistoryEntry>> {
        ensure!(seq > 0, "history sequence must be positive");
        let entries = self.transcript(None, Some(seq - 1), limit.clamp(1, 8))?;
        ensure!(
            entries.first().is_some_and(|entry| entry.seq == seq),
            "saved history entry {seq} does not exist"
        );
        Ok(entries)
    }

    pub fn native_transcript(
        &self,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<NativeEntry>> {
        let entries = self.transcript(before, after, limit)?;
        let mut receipt = self.connection.prepare(
            "SELECT command FROM command_results INDEXED BY command_results_notice_seq WHERE json_valid(reply) AND json_extract(reply,'$.Notice.seq')=?1 LIMIT 1",
        )?;
        entries
            .into_iter()
            .map(|entry| {
                // Only a receipt for this exact notice supplies semantic metadata.
                let command: Option<String> = if entry.kind == "notice" {
                    receipt.query_row([entry.seq], |r| r.get(0)).optional()?
                } else {
                    None
                };
                let category = match command
                    .as_deref()
                    .map(serde_json::from_str::<Command>)
                    .transpose()?
                {
                    Some(Command::RecordAction { category, .. }) => Some(category),
                    _ => None,
                };
                Ok(NativeEntry { entry, category })
            })
            .collect()
    }

    pub fn inputs(&self, limit: usize) -> Result<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT text FROM messages ORDER BY seq DESC LIMIT ?1")?;
        let items = statement.query_map([limit as i64], |r| r.get(0))?;
        let mut items = items.collect::<rusqlite::Result<Vec<_>>>()?;
        items.reverse();
        Ok(items)
    }

    pub fn events(&self, after: i64, limit: usize) -> Result<Vec<JournalEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT seq,kind,attempt_id,payload,created_at FROM events WHERE seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![after, limit as i64], |r| {
                Ok(JournalEvent {
                    seq: r.get(0)?,
                    kind: r.get(1)?,
                    attempt_id: r.get(2)?,
                    payload: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn previous(&self, command_id: &str, command: &Command) -> Result<Option<Reply>> {
        let saved = self
            .connection
            .query_row(
                "SELECT command,reply FROM command_results WHERE id=?1",
                [command_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        match saved {
            Some((body, reply)) => {
                ensure!(
                    body == serde_json::to_string(command)?,
                    "command ID already used with different content"
                );
                Ok(Some(serde_json::from_str(&reply)?))
            }
            None => Ok(None),
        }
    }

    pub fn apply(
        &mut self,
        command_id: &str,
        command: &Command,
        config: &Config,
        completion: Option<Completion<'_>>,
    ) -> Result<Reply> {
        if let Some(reply) = self.previous(command_id, command)? {
            return Ok(reply);
        }
        if let Command::RecordNotice { text, .. } = command {
            validate_notice(text)?;
        }
        if let Command::RecordAction { text, category, .. } = command {
            validate_action(text, category)?;
        }
        guard_wal(&self.connection, &self.wal_path, self.transaction_reserve)?;
        let tx = self.connection.transaction()?;
        if let Command::StopPending {
            expected_attempt_id,
            expected_paused,
        } = command
        {
            check_stop_pending(&tx, expected_attempt_id.as_deref(), *expected_paused)?;
            ensure!(
                completion.as_ref().map(|c| c.attempt_id) == expected_attempt_id.as_deref()
                    && completion.as_ref().is_none_or(|c| c.status == "cancelled"),
                "stop requires the selected attempt's cancelled completion"
            );
        }
        if let Command::SubmitInput {
            expected_paused, ..
        }
        | Command::SubmitInputAccumulated {
            expected_paused, ..
        }
        | Command::DispatchDraftChecked {
            expected_paused, ..
        }
        | Command::DispatchDraftAccumulated {
            expected_paused, ..
        }
        | Command::FinishPendingEdit {
            expected_paused, ..
        } = command
        {
            let paused: bool = tx.query_row("SELECT paused FROM session", [], |r| r.get(0))?;
            ensure!(paused == *expected_paused, "paused state conflict");
        }
        if let Command::SubmitInput {
            source,
            text,
            composer_revision,
            ..
        }
        | Command::SubmitInputAccumulated {
            source,
            text,
            composer_revision,
            ..
        } = command
        {
            let revision: i64 =
                tx.query_row("SELECT revision FROM composer WHERE singleton=1", [], |r| {
                    r.get(0)
                })?;
            ensure!(revision == *composer_revision, "composer revision conflict");
            validate_input(source, text, config.limits.message_bytes)?;
        }
        if matches!(
            command,
            Command::SubmitInputAccumulated { .. } | Command::DispatchDraftAccumulated { .. }
        ) {
            let editing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE state='editing')",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                !editing,
                "finish or cancel the pending edit before submitting input"
            );
        }
        if let Some(completion) = completion {
            complete(&tx, &mut self.artifacts, completion)?;
        }
        let reply = match command {
            Command::RecordNotice { text, level } => {
                Reply::Notice(notice(&tx, text, *level, None, None)?)
            }
            Command::RecordAction {
                text,
                level,
                category,
            } => Reply::Notice(notice(&tx, text, *level, None, Some(category))?),
            Command::SaveDraft { text } => {
                ensure!(
                    !text.trim().is_empty() && text.len() <= config.limits.message_bytes,
                    "invalid draft size"
                );
                let count: i64 = tx.query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))?;
                ensure!(
                    count < config.limits.queue_messages as i64,
                    "draft limit reached"
                );
                let draft = DraftRecord {
                    id: id(),
                    text: text.clone(),
                    revision: 0,
                };
                tx.execute(
                    "INSERT INTO drafts(id,text,revision) VALUES(?1,?2,0)",
                    params![draft.id, text],
                )?;
                event(&tx, "draft_saved", None, &draft.id)?;
                Reply::Draft(draft)
            }
            Command::DeleteDraft { id, revision } => {
                ensure!(
                    tx.execute(
                        "DELETE FROM drafts WHERE id=?1 AND revision=?2",
                        params![id, revision]
                    )? == 1,
                    "draft missing or revision conflict"
                );
                event(&tx, "draft_deleted", None, id)?;
                Reply::Ack
            }
            Command::SaveComposer {
                text,
                cursor,
                revision,
            } => {
                ensure!(
                    text.len() <= config.limits.message_bytes,
                    "invalid composer size"
                );
                ensure!(
                    *cursor <= text.len() && text.is_char_boundary(*cursor),
                    "invalid composer cursor"
                );
                ensure!(tx.execute("UPDATE composer SET text=?1,cursor=?2,revision=revision+1 WHERE singleton=1 AND revision=?3", params![text,*cursor as i64,revision])? == 1, "composer revision conflict");
                event(&tx, "composer_saved", None, "")?;
                Reply::Composer(ComposerState {
                    text: text.clone(),
                    cursor: *cursor,
                    revision: revision + 1,
                })
            }
            Command::Send { .. }
            | Command::Submit { .. }
            | Command::DispatchDraft { .. }
            | Command::SubmitInput { .. }
            | Command::SubmitInputAccumulated { .. }
            | Command::DispatchDraftAccumulated { .. }
            | Command::DispatchDraftChecked { .. } => {
                let draft_text;
                let text = match command {
                    Command::Send { text }
                    | Command::Submit { text, .. }
                    | Command::SubmitInput { text, .. }
                    | Command::SubmitInputAccumulated { text, .. } => text,
                    Command::DispatchDraft { id, revision, .. }
                    | Command::DispatchDraftChecked { id, revision, .. }
                    | Command::DispatchDraftAccumulated { id, revision, .. } => {
                        draft_text = tx
                            .query_row(
                                "SELECT text FROM drafts WHERE id=?1 AND revision=?2",
                                params![id, revision],
                                |r| r.get::<_, String>(0),
                            )
                            .optional()?
                            .context("draft missing or revision conflict")?;
                        if matches!(
                            command,
                            Command::DispatchDraftChecked { .. }
                                | Command::DispatchDraftAccumulated { .. }
                        ) {
                            validate_input(
                                &draft_text,
                                input_payload(&draft_text),
                                config.limits.message_bytes,
                            )?;
                            input_payload(&draft_text)
                        } else {
                            &draft_text
                        }
                    }
                    _ => unreachable!(),
                };
                ensure!(!text.trim().is_empty(), "message is empty");
                ensure!(
                    text.len() <= config.limits.message_bytes,
                    "message byte limit exceeded"
                );
                // Never fall back to an older queued item after the latest input
                // was accepted, cancelled, or leased for editing.
                let pending = if matches!(
                    command,
                    Command::SubmitInputAccumulated { .. }
                        | Command::DispatchDraftAccumulated { .. }
                ) {
                    tx.query_row(
                        "SELECT id,text FROM messages WHERE seq=(SELECT MAX(seq) FROM messages) AND state='queued'",
                        [],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?
                } else {
                    None
                };
                let (message_id, message_event) = if let Some((message_id, previous)) = pending {
                    let frozen: String = tx.query_row(
                        "SELECT config FROM dispatch_configs WHERE message_id=?1",
                        [&message_id],
                        |r| r.get(0),
                    )?;
                    let frozen: FrozenConfig = toml::from_str(&frozen)?;
                    ensure!(
                        previous.len().saturating_add(2).saturating_add(text.len())
                            <= config.limits.message_bytes.min(frozen.limits.message_bytes),
                        "combined message byte limit exceeded"
                    );
                    self.artifacts.admit(
                        config
                            .limits
                            .artifact_bytes
                            .min(frozen.limits.artifact_bytes),
                    )?;
                    tx.execute(
                        "UPDATE messages SET text=?2,revision=revision+1 WHERE id=?1 AND state='queued'",
                        params![message_id, format!("{previous}\n\n{text}")],
                    )?;
                    (message_id, "message_appended")
                } else {
                    let count: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM messages WHERE state IN ('queued','editing')",
                        [],
                        |r| r.get(0),
                    )?;
                    ensure!(
                        count < config.limits.queue_messages as i64,
                        "queue limit reached"
                    );
                    self.artifacts.admit(config.limits.artifact_bytes)?;
                    let message_id = id();
                    tx.execute("INSERT INTO messages(id,agent_id,text,state,revision) VALUES(?1,?2,?3,'queued',0)", params![message_id, self.agent_id, text])?;
                    tx.execute(
                        "INSERT INTO dispatch_configs VALUES(?1,?2)",
                        params![message_id, toml::to_string(&config.attempt_config())?],
                    )?;
                    (message_id, "message_queued")
                };
                if let Command::DispatchDraft { id, revision, .. }
                | Command::DispatchDraftChecked { id, revision, .. }
                | Command::DispatchDraftAccumulated { id, revision, .. } = command
                {
                    ensure!(
                        tx.execute(
                            "DELETE FROM drafts WHERE id=?1 AND revision=?2",
                            params![id, revision]
                        )? == 1,
                        "draft revision conflict"
                    );
                    event(&tx, "draft_dispatched", None, id)?;
                }
                if matches!(command, Command::Submit { .. }) {
                    tx.execute("UPDATE composer SET text='',cursor=0,revision=revision+1 WHERE singleton=1 AND text=?1", [text])?;
                }
                if let Command::SubmitInput {
                    composer_revision, ..
                }
                | Command::SubmitInputAccumulated {
                    composer_revision, ..
                } = command
                {
                    ensure!(tx.execute("UPDATE composer SET text='',cursor=0,revision=revision+1 WHERE singleton=1 AND revision=?1", [composer_revision])? == 1, "composer revision conflict");
                }
                if matches!(
                    command,
                    Command::Submit { resume: true, .. }
                        | Command::DispatchDraft { resume: true, .. }
                        | Command::SubmitInput { resume: true, .. }
                        | Command::SubmitInputAccumulated { resume: true, .. }
                        | Command::DispatchDraftChecked { resume: true, .. }
                        | Command::DispatchDraftAccumulated {
                            expected_paused: true,
                            ..
                        }
                ) {
                    tx.execute("UPDATE session SET paused=0", [])?;
                    event(&tx, "session_resumed", None, "dispatch")?;
                }
                event(&tx, message_event, None, &message_id)?;
                Reply::Ack
            }
            Command::BeginEdit { id, revision } => {
                let leases: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM messages WHERE state='editing'",
                    [],
                    |r| r.get(0),
                )?;
                ensure!(leases == 0, "an edit lease is already active");
                ensure!(tx.execute("UPDATE messages SET state='editing',revision=revision+1 WHERE id=?1 AND revision=?2 AND state='queued'", params![id, revision])? == 1, "message already accepted or revision conflict");
                let text =
                    tx.query_row("SELECT text FROM messages WHERE id=?1", [id], |r| r.get(0))?;
                event(&tx, "message_editing", None, id)?;
                Reply::Editing(QueueItem {
                    id: id.clone(),
                    text,
                    revision: revision + 1,
                    state: QueueState::Editing,
                })
            }
            Command::FinishEdit { id, revision, text }
            | Command::FinishPendingEdit {
                id, revision, text, ..
            } => {
                ensure!(
                    !text.trim().is_empty() && text.len() <= config.limits.message_bytes,
                    "invalid message size"
                );
                if matches!(command, Command::FinishPendingEdit { .. }) {
                    let frozen: String = tx.query_row(
                        "SELECT d.config FROM messages m JOIN dispatch_configs d ON d.message_id=m.id WHERE m.id=?1 AND m.revision=?2 AND m.state='editing'",
                        params![id, revision],
                        |r| r.get(0),
                    ).optional()?.context("edit lease or revision conflict")?;
                    let frozen: FrozenConfig = toml::from_str(&frozen)?;
                    ensure!(
                        text.len() <= frozen.limits.message_bytes,
                        "message byte limit exceeded for frozen dispatch config"
                    );
                    self.artifacts.admit(
                        config
                            .limits
                            .artifact_bytes
                            .min(frozen.limits.artifact_bytes),
                    )?;
                }
                ensure!(tx.execute("UPDATE messages SET text=?3,state='queued',revision=revision+1 WHERE id=?1 AND revision=?2 AND state='editing'", params![id,revision,text])? == 1, "edit lease or revision conflict");
                if matches!(
                    command,
                    Command::FinishPendingEdit {
                        expected_paused: true,
                        ..
                    }
                ) {
                    tx.execute("UPDATE session SET paused=0", [])?;
                    event(&tx, "session_resumed", None, "pending edit")?;
                }
                event(&tx, "message_edited", None, id)?;
                Reply::Ack
            }
            Command::CancelEdit { id, revision } => {
                ensure!(tx.execute("UPDATE messages SET state='queued',revision=revision+1 WHERE id=?1 AND revision=?2 AND state='editing'", params![id,revision])? == 1, "edit lease or revision conflict");
                event(&tx, "edit_cancelled", None, id)?;
                Reply::Ack
            }
            Command::Cancel { id, revision } => {
                ensure!(tx.execute("UPDATE messages SET state='cancelled',revision=revision+1 WHERE id=?1 AND revision=?2 AND state IN ('queued','editing')", params![id,revision])? == 1, "message already accepted or revision conflict");
                event(&tx, "message_cancelled", None, id)?;
                Reply::Ack
            }
            Command::Interrupt { attempt_id } => {
                event(
                    &tx,
                    "interrupt_acknowledged",
                    Some(attempt_id),
                    "only the selected attempt",
                )?;
                Reply::Ack
            }
            Command::StopPending {
                expected_attempt_id,
                ..
            } => {
                let pending: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE state='queued')",
                    [],
                    |r| r.get(0),
                )?;
                let paused = !pending;
                tx.execute("UPDATE session SET paused=?1", [paused])?;
                event(
                    &tx,
                    if paused {
                        "session_paused"
                    } else {
                        "session_resumed"
                    },
                    None,
                    "stop pending",
                )?;
                Reply::Stopped {
                    paused,
                    cancelled: expected_attempt_id.is_some(),
                }
            }
            Command::Pause | Command::Resume | Command::Stop | Command::Shutdown => {
                let paused = !matches!(command, Command::Resume);
                tx.execute("UPDATE session SET paused=?1", [paused])?;
                if matches!(command, Command::Shutdown) {
                    tx.execute("UPDATE messages SET state='queued',revision=revision+1 WHERE state='editing'", [])?;
                }
                event(
                    &tx,
                    if paused {
                        "session_paused"
                    } else {
                        "session_resumed"
                    },
                    None,
                    "",
                )?;
                Reply::Ack
            }
            _ => bail!("not a mutating command"),
        };
        tx.execute(
            "INSERT INTO command_results VALUES(?1,?2,?3)",
            params![
                command_id,
                serde_json::to_string(command)?,
                serde_json::to_string(&reply)?
            ],
        )?;
        tx.commit()?;
        Ok(reply)
    }

    pub fn start_next(&mut self, _config: &Config) -> Result<Option<Started>> {
        guard_wal(&self.connection, &self.wal_path, self.transaction_reserve)?;
        let tx = self.connection.transaction()?;
        let paused: bool = tx.query_row("SELECT paused FROM session", [], |r| r.get(0))?;
        let editing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE state='editing'",
            [],
            |r| r.get(0),
        )?;
        if paused || editing > 0 {
            return Ok(None);
        }
        let next = tx
            .query_row(
                "SELECT m.id,m.text,d.config FROM messages m JOIN dispatch_configs d ON d.message_id=m.id WHERE m.state='queued' ORDER BY m.seq LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?;
        let Some((message_id, text, frozen)) = next else {
            return Ok(None);
        };
        let attempt_id = id();
        let frozen_config: FrozenConfig = toml::from_str(&frozen)?;
        self.artifacts.admit(frozen_config.limits.artifact_bytes)?;
        tx.execute(
            "UPDATE messages SET state='processing',revision=revision+1 WHERE id=?1",
            [&message_id],
        )?;
        tx.execute(
            "INSERT INTO attempts VALUES(?1,?2,'running',?3)",
            params![attempt_id, message_id, frozen],
        )?;
        tx.execute(
            "INSERT INTO history(attempt_id,kind,text,status) VALUES(?1,'user',?2,'accepted')",
            params![attempt_id, text],
        )?;
        tx.execute(
            "INSERT INTO history(attempt_id,kind,text,status) VALUES(?1,'assistant','','running')",
            [&attempt_id],
        )?;
        event(&tx, "attempt_started", Some(&attempt_id), &message_id)?;
        group_boundary(&tx)?;
        let output = id();
        self.artifacts.create(&output)?;
        tx.execute(
            "INSERT INTO artifacts VALUES(?1,?2,0,1)",
            params![output, attempt_id],
        )?;
        action(
            &tx,
            "fixture",
            "Fixture result",
            "running",
            Some(&output),
            Some(&attempt_id),
        )?;
        tx.commit()?;
        Ok(Some(Started {
            attempt_id,
            message_id,
            config: frozen_config,
        }))
    }

    pub(crate) fn check_stop_pending(
        &self,
        expected_attempt_id: Option<&str>,
        expected_paused: bool,
    ) -> Result<()> {
        check_stop_pending(&self.connection, expected_attempt_id, expected_paused)
    }

    pub fn checkpoint(&mut self, attempt_id: &str, text: &str) -> Result<()> {
        guard_wal(&self.connection, &self.wal_path, self.transaction_reserve)?;
        let tx = self.connection.transaction()?;
        ensure!(tx.execute("UPDATE history SET text=?2 WHERE attempt_id=?1 AND kind='assistant' AND status='running'", params![attempt_id,text])? == 1, "attempt is no longer active");
        let output = save_output_text(&tx, &mut self.artifacts, attempt_id, text)?;
        if let Some(output) = output {
            let record = self.artifacts.record(&output, true)?;
            tx.execute(
                "UPDATE artifacts SET bytes=?2 WHERE id=?1",
                params![output, record.bytes as i64],
            )?;
        }
        event(
            &tx,
            "response_checkpoint",
            Some(attempt_id),
            &text.len().to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish(&mut self, completion: Completion<'_>) -> Result<()> {
        self.finish_detail(completion, None)
    }

    pub fn finish_detail(
        &mut self,
        completion: Completion<'_>,
        reason: Option<&str>,
    ) -> Result<()> {
        guard_wal(&self.connection, &self.wal_path, self.transaction_reserve)?;
        let tx = self.connection.transaction()?;
        let attempt_id = completion.attempt_id;
        complete(&tx, &mut self.artifacts, completion)?;
        if let Some(reason) = reason {
            let mut text = String::from("error: ");
            for ch in reason.chars() {
                let ch = if ch.is_control() || ch.is_whitespace() {
                    ' '
                } else {
                    ch
                };
                if text.len() + ch.len_utf8() > MAX_NOTICE_BYTES {
                    break;
                }
                text.push(ch);
            }
            notice(&tx, &text, NoticeLevel::Error, Some(attempt_id), None)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Scenario;

    #[test]
    fn persisted_attempt_snapshot_contains_only_fake_provider_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.scenario = Scenario::Burst;
        config.provider.delay_ms = 11;
        config.provider.first_event_ms = 23;
        config.provider.chunks = 7;
        config.limits.queue_messages = 5;
        config.limits.message_bytes = 4096;
        config.limits.response_bytes = 8192;
        config.limits.history_page = 13;
        config.limits.checkpoint_ms = 100;
        config.limits.storage_bytes = 8 * 1024 * 1024;
        config.interface.status_line = false;
        config.interface.status_color = true;
        config.validate().unwrap();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply(
                "send",
                &Command::Send {
                    text: "test".into(),
                },
                &config,
                None,
            )
            .unwrap();
        let started = store.start_next(&config).unwrap().unwrap();
        let text: String = store
            .connection
            .query_row(
                "SELECT config FROM attempts WHERE id=?1",
                [&started.attempt_id],
                |r| r.get(0),
            )
            .unwrap();
        let value: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(value.as_table().unwrap().len(), 2);
        assert!(value.get("interface").is_none());
        assert!(value.get("keys").is_none());
        assert_eq!(
            value["provider"],
            toml::Value::try_from(&config.provider).unwrap()
        );
        assert_eq!(
            value["limits"],
            toml::Value::try_from(&config.limits).unwrap()
        );
    }

    #[test]
    fn old_full_config_snapshots_reopen_unchanged_without_a_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.sqlite3");
        let config = Config::default();
        let old_snapshot = toml::to_string(&config).unwrap();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply(
                "send",
                &Command::Send {
                    text: "test".into(),
                },
                &config,
                None,
            )
            .unwrap();
        let started = store.start_next(&config).unwrap().unwrap();
        store
            .connection
            .execute(
                "UPDATE attempts SET config=?1 WHERE id=?2",
                params![old_snapshot, started.attempt_id],
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path, &config).unwrap();
        assert!(store.reopened);
        assert!(store.paused().unwrap());
        let (snapshot, status): (String, String) = store
            .connection
            .query_row(
                "SELECT config,status FROM attempts WHERE id=?1",
                [&started.attempt_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(snapshot, old_snapshot);
        assert_eq!(status, "interrupted");
        assert_eq!(
            store
                .connection
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            2
        );
    }
}

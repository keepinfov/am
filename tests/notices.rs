use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, HistoryEntry, MAX_NOTICE_BYTES, NoticeLevel, Phase, Reply},
    storage::{Completion, Store},
};
use rusqlite::Connection;
use std::time::Duration;
use tokio::time::{sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(5);

fn notice(text: &str, level: NoticeLevel) -> Command {
    Command::RecordNotice {
        text: text.into(),
        level,
    }
}

async fn rows(client: &Client, command: Command) -> Vec<HistoryEntry> {
    let Reply::History(rows) = timeout(DEADLINE, client.command(command))
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected history")
    };
    rows
}

fn history(before: Option<i64>, limit: usize) -> Command {
    Command::History { before, limit }
}

fn transcript(before: Option<i64>, after: Option<i64>, limit: usize) -> Command {
    Command::Transcript {
        before,
        after,
        limit,
    }
}

fn count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notices_are_durable_paged_receipts_without_queue_or_lease_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.history_page = 1;
    let client = Client::start(&path, config.clone()).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client
        .command(Command::Send {
            text: "queued input".into(),
        })
        .await
        .unwrap();
    let item = client.snapshot().queue[0].clone();
    client
        .command(Command::BeginEdit {
            id: item.id,
            revision: item.revision,
        })
        .await
        .unwrap();
    let db = Connection::open(&path).unwrap();
    let receipts = count(&db, "command_results");
    let events = count(&db, "events");
    let before = client.snapshot();
    let first = notice("settings saved", NoticeLevel::Info);
    let second = notice("resumed", NoticeLevel::Status);
    let Reply::Notice(first_saved) = client
        .command_with_id("notice-1".into(), first.clone())
        .await
        .unwrap()
    else {
        panic!("expected saved notice")
    };
    assert_eq!(client.snapshot().paused, before.paused);
    client.command(Command::Resume).await.unwrap();
    let resumed = client.snapshot();
    assert!(!resumed.paused);
    let Reply::Notice(second_saved) = client
        .command_with_id("notice-2".into(), second.clone())
        .await
        .unwrap()
    else {
        panic!("expected saved notice")
    };
    let after = client.snapshot();
    assert_eq!(after.paused, resumed.paused);
    assert_eq!(after.phase, resumed.phase);
    assert!(after.active.is_none());
    assert_eq!(
        serde_json::to_value(&after.queue).unwrap(),
        serde_json::to_value(&before.queue).unwrap()
    );
    assert_eq!(count(&db, "messages"), 1);
    assert_eq!(count(&db, "attempts"), 0);
    assert_eq!(count(&db, "command_results"), receipts + 3); // Two notices and Resume.
    assert_eq!(count(&db, "events"), events + 3);
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM history WHERE attempt_id IS NULL",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(db.query_row::<i64, _, _>("SELECT COUNT(*) FROM events WHERE kind='notice_recorded' AND attempt_id IS NULL AND payload IN ('info','status')", [], |r| r.get(0)).unwrap(), 2);
    assert!(
        matches!(client.command(Command::Inputs { limit: 100 }).await.unwrap(), Reply::Inputs(inputs) if inputs == ["queued input"])
    );

    let latest = rows(&client, history(None, usize::MAX)).await;
    assert_eq!(latest.len(), 1);
    assert_eq!(second_saved, latest[0]);
    assert_eq!(latest[0].seq, 2);
    assert_eq!(latest[0].kind, "notice");
    assert_eq!(latest[0].text, "resumed");
    assert_eq!(latest[0].status, "status");
    assert!(latest[0].attempt_id.is_empty());
    let older = rows(&client, history(Some(2), usize::MAX)).await;
    assert_eq!(older.len(), 1);
    assert_eq!(first_saved, older[0]);
    assert_eq!(older[0].seq, 1);
    assert_eq!(older[0].text, "settings saved");
    assert_eq!(older[0].status, "info");
    assert!(older[0].attempt_id.is_empty());
    assert!(rows(&client, history(Some(1), 1)).await.is_empty());
    assert_eq!(
        rows(&client, transcript(None, Some(0), usize::MAX)).await[0].seq,
        1
    );
    assert_eq!(
        rows(&client, transcript(None, Some(1), usize::MAX)).await[0].seq,
        2
    );
    assert!(rows(&client, transcript(None, Some(2), 1)).await.is_empty());
    assert_eq!(rows(&client, transcript(Some(2), None, 1)).await[0].seq, 1);
    assert_eq!(rows(&client, transcript(None, None, 0)).await[0].seq, 2);
    let cursor = client.snapshot().cursor;
    for (id, command, saved) in [
        ("notice-1", first.clone(), &first_saved),
        ("notice-2", second.clone(), &second_saved),
    ] {
        assert!(matches!(
            client.command_with_id(id.into(), command).await.unwrap(),
            Reply::Notice(entry) if &entry == saved
        ));
    }
    assert!(
        client
            .command_with_id("notice-1".into(), notice("changed", NoticeLevel::Info))
            .await
            .is_err()
    );
    assert!(
        client
            .command_with_id(
                "notice-1".into(),
                notice("settings saved", NoticeLevel::Error)
            )
            .await
            .is_err()
    );
    assert_eq!(client.snapshot().cursor, cursor);
    assert_eq!(count(&db, "history"), 2);
    client.shutdown().await.unwrap();

    let reopened = Client::start(&path, config).await.unwrap();
    let cursor = reopened.snapshot().cursor;
    for (id, command, saved) in [
        ("notice-1", first, &first_saved),
        ("notice-2", second, &second_saved),
    ] {
        assert!(matches!(
            reopened.command_with_id(id.into(), command).await.unwrap(),
            Reply::Notice(entry) if &entry == saved
        ));
    }
    assert_eq!(reopened.snapshot().cursor, cursor);
    assert_eq!(
        serde_json::to_value(rows(&reopened, history(None, 1)).await).unwrap(),
        serde_json::to_value(latest).unwrap()
    );
    assert_eq!(
        serde_json::to_value(rows(&reopened, history(Some(2), 1)).await).unwrap(),
        serde_json::to_value(older).unwrap()
    );
    assert_eq!(count(&db, "history"), 2);
    assert_eq!(count(&db, "attempts"), 0);
    reopened.shutdown().await.unwrap();

    for history_page in [1, 50] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unfinished.sqlite3");
        let mut config = Config::default();
        config.limits.history_page = history_page;
        config.provider.scenario = Scenario::Waiting;
        config.provider.first_event_ms = 60_000;
        let client = Client::start(&path, config).await.unwrap();
        client
            .command(Command::Send {
                text: "unfinished input".into(),
            })
            .await
            .unwrap();
        let mut snapshots = client.subscribe();
        timeout(DEADLINE, async {
            loop {
                if snapshots.borrow_and_update().active.is_some() {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let active = client.snapshot().active.unwrap().attempt_id;
        let unfinished = rows(&client, transcript(None, Some(1), 1)).await;
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].kind, "assistant");
        assert_eq!(unfinished[0].status, "running");
        let command = notice("saved during response", NoticeLevel::Status);
        let Reply::Notice(saved) = client
            .command_with_id("live-notice".into(), command.clone())
            .await
            .unwrap()
        else {
            panic!("expected saved notice")
        };
        assert_eq!(
            saved,
            HistoryEntry {
                seq: 3,
                attempt_id: String::new(),
                kind: "notice".into(),
                text: "saved during response".into(),
                status: "status".into()
            }
        );
        assert_eq!(client.snapshot().active.unwrap().attempt_id, active);
        assert_eq!(
            rows(&client, transcript(None, Some(1), 1)).await,
            unfinished
        );
        assert_eq!(
            rows(&client, transcript(None, Some(2), 1)).await,
            std::slice::from_ref(&saved)
        );
        assert!(
            matches!(client.command_with_id("live-notice".into(), command).await.unwrap(), Reply::Notice(entry) if entry == saved)
        );
        let db = Connection::open(&path).unwrap();
        assert_eq!(count(&db, "history"), 3);
        assert_eq!(count(&db, "attempts"), 1);
        let receipt: String = db
            .query_row(
                "SELECT reply FROM command_results WHERE id='live-notice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            matches!(serde_json::from_str::<Reply>(&receipt).unwrap(), Reply::Notice(entry) if entry == saved)
        );
        client.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_notices_reject_before_a_busy_actor_and_valid_text_has_its_own_byte_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.message_bytes = 1;
    let client = Client::start(&path, config.clone()).await.unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let busy = client.clone();
    let mutation = tokio::spawn(async move { busy.command(Command::Pause).await });
    sleep(Duration::from_millis(20)).await;
    let mut invalid = vec![
        String::new(),
        "   ".into(),
        "x".repeat(MAX_NOTICE_BYTES + 1),
        "\u{e9}".repeat(513),
        "a\u{2028}b".into(),
        "a\u{2029}b".into(),
    ];
    for code in (0..=31).chain(127..=159) {
        invalid.push(format!("a{}b", char::from_u32(code).unwrap()));
    }
    for (index, text) in invalid.into_iter().enumerate() {
        let result = timeout(
            Duration::from_millis(100),
            client.command_with_id(format!("invalid-{index}"), notice(&text, NoticeLevel::Info)),
        )
        .await
        .expect("validation waited for actor");
        assert!(result.is_err(), "accepted malformed notice {text:?}");
    }
    db.execute_batch("ROLLBACK").unwrap();
    mutation.await.unwrap().unwrap();
    assert_eq!(count(&db, "history"), 0);
    assert_eq!(count(&db, "command_results"), 1);
    assert_eq!(count(&db, "events"), 2);
    let boundary = "\u{e9}".repeat(MAX_NOTICE_BYTES / 2);
    let command = notice(&boundary, NoticeLevel::Error);
    let encoded = serde_json::to_string(&command).unwrap();
    let decoded: Command = serde_json::from_str(&encoded).unwrap();
    let Reply::Notice(reply) = client.command(decoded).await.unwrap() else {
        panic!("expected saved notice")
    };
    let saved = rows(&client, history(None, 1)).await;
    assert_eq!(reply, saved[0]);
    assert_eq!(reply.seq, 1);
    assert_eq!(reply.kind, "notice");
    assert_eq!(saved[0].text, boundary);
    assert_eq!(saved[0].status, "error");
    assert!(saved[0].attempt_id.is_empty());
    assert_eq!(count(&db, "messages"), 0);
    assert_eq!(count(&db, "attempts"), 0);
    client.shutdown().await.unwrap();

    let mut store = Store::open(&path, &config).unwrap();
    let receipts = count(&db, "command_results");
    assert!(
        store
            .apply(
                "invalid-direct",
                &notice("bad\nnotice", NoticeLevel::Info),
                &config,
                None
            )
            .is_err()
    );
    assert_eq!(count(&db, "command_results"), receipts);
    assert_eq!(count(&db, "history"), 1);
}

#[test]
fn version_one_migration_preserves_history_indexes_high_water_and_recovery() {
    for paused in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("
            PRAGMA foreign_keys=ON;
            CREATE TABLE session(id TEXT PRIMARY KEY,agent_id TEXT NOT NULL,conversation_id TEXT NOT NULL,paused INTEGER NOT NULL);
            CREATE TABLE messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,agent_id TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,revision INTEGER NOT NULL);
            CREATE TABLE attempts(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id),status TEXT NOT NULL,config TEXT NOT NULL);
            CREATE TABLE history(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT NOT NULL REFERENCES attempts(id),kind TEXT NOT NULL,text TEXT NOT NULL,status TEXT NOT NULL);
            CREATE INDEX history_attempt ON history(attempt_id);
            CREATE INDEX history_kind ON history(kind,seq);
            CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT,kind TEXT NOT NULL,attempt_id TEXT,payload TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE TABLE command_results(id TEXT PRIMARY KEY,command TEXT NOT NULL,reply TEXT NOT NULL);
            INSERT INTO session VALUES('session','agent','conversation',0);
            INSERT INTO messages VALUES(1,'handled','agent','old input','handled',2),(2,'processing','agent','partial input','processing',1),(3,'queued','agent','queued input','queued',4),(4,'editing','agent','edit input','editing',7);
            INSERT INTO attempts VALUES('complete','handled','completed','old persisted config'),('running','processing','running','running persisted config');
            INSERT INTO history VALUES(3,'complete','user','old input','accepted'),(9,'complete','assistant','old response','completed'),(12,'running','user','partial input','accepted'),(21,'running','assistant','saved partial','running'),(80,'complete','assistant','deleted row','completed');
            DELETE FROM history WHERE seq=80;
            INSERT INTO events(seq,kind,attempt_id,payload) VALUES(5,'attempt_finished','complete','completed');
            INSERT INTO command_results VALUES('old-pause','\"Pause\"','\"Ack\"');
            PRAGMA user_version=1;
        ").unwrap();
        db.execute("UPDATE session SET paused=?1", [paused])
            .unwrap();
        // Fail the final FK check after the history rebuild, then verify rollback.
        db.execute_batch("PRAGMA foreign_keys=OFF; UPDATE attempts SET message_id='missing' WHERE id='complete';")
            .unwrap();
        drop(db);

        assert!(Store::open(&path, &config).is_err());
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT \"notnull\" FROM pragma_table_info('history') WHERE name='attempt_id'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT seq FROM sqlite_sequence WHERE name='history'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            80
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='history_v2'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row::<bool, _, _>("SELECT paused FROM session", [], |r| r.get(0))
                .unwrap(),
            paused
        );
        assert_eq!(
            db.query_row::<String, _, _>("SELECT status FROM history WHERE seq=21", [], |r| r
                .get(0))
                .unwrap(),
            "running"
        );
        assert_eq!(count(&db, "events"), 1);
        db.execute(
            "UPDATE attempts SET message_id='handled' WHERE id='complete'",
            [],
        )
        .unwrap();
        drop(db);

        let mut store = Store::open(&path, &config).unwrap();
        assert!(store.reopened);
        assert!(store.paused().unwrap());
        assert_eq!(
            (
                &*store.session_id,
                &*store.agent_id,
                &*store.conversation_id
            ),
            ("session", "agent", "conversation")
        );
        let entries = store.history(None, 100).unwrap();
        assert_eq!(
            entries.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [3, 9, 12, 21]
        );
        assert_eq!(
            entries.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            [
                "old input",
                "old response",
                "partial input",
                "saved partial"
            ]
        );
        assert_eq!(entries[1].status, "completed");
        assert_eq!(entries[3].status, "interrupted");
        assert_eq!(entries[3].attempt_id, "running");
        assert!(matches!(
            store.previous("old-pause", &Command::Pause).unwrap(),
            Some(Reply::Ack)
        ));
        let queue = store.queue(10).unwrap();
        assert_eq!(
            queue
                .iter()
                .map(|r| (r.text.as_str(), r.revision))
                .collect::<Vec<_>>(),
            [("queued input", 4), ("edit input", 8)]
        );
        assert!(
            queue
                .iter()
                .all(|r| r.state == am::runtime::QueueState::Queued)
        );
        assert!(store.start_next(&config).unwrap().is_none());
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT seq FROM sqlite_sequence WHERE name='history'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            80
        );
        let indexes: Vec<String> = db.prepare("SELECT sql FROM sqlite_schema WHERE type='index' AND tbl_name='history' ORDER BY name").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(
            indexes,
            [
                "CREATE INDEX history_attempt ON history(attempt_id)",
                "CREATE INDEX history_kind ON history(kind,seq)"
            ]
        );
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT config FROM attempts WHERE id='running'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "running persisted config"
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT revision FROM messages WHERE id='processing'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(store.events(0, 10).unwrap()[0].seq, 5);
        assert_eq!(store.events(0, 10).unwrap()[1].seq, 6);
        let Reply::Notice(saved_notice) = store
            .apply(
                "migrated-notice",
                &notice("migration saved", NoticeLevel::Status),
                &config,
                None,
            )
            .unwrap()
        else {
            panic!("expected saved notice")
        };
        assert_eq!(saved_notice, store.history(None, 1).unwrap()[0]);
        assert_eq!(saved_notice.kind, "notice");
        assert_eq!(saved_notice.text, "migration saved");
        assert_eq!(saved_notice.status, "status");
        assert_eq!(store.history(None, 1).unwrap()[0].seq, 81);
        assert!(store.history(None, 1).unwrap()[0].attempt_id.is_empty());
        store
            .apply("resume", &Command::Resume, &config, None)
            .unwrap();
        let started = store.start_next(&config).unwrap().unwrap();
        store
            .finish(Completion {
                attempt_id: &started.attempt_id,
                text: "new response",
                status: "completed",
            })
            .unwrap();
        assert_eq!(
            store
                .transcript(None, Some(81), 10)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            [82, 83]
        );
        assert!(
            db.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none()
        );
        drop(store);
        let reopened = Store::open(&path, &config).unwrap();
        assert!(reopened.paused().unwrap());
        assert_eq!(
            reopened.history(Some(82), 1).unwrap()[0].text,
            "migration saved"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notice_write_faults_rollback_text_event_and_receipt_without_recursive_logging() {
    for (table, predicate) in [
        ("history", "NEW.kind='notice'"),
        ("events", "NEW.kind='notice_recorded'"),
        ("command_results", "NEW.id='failed-notice'"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let client = Client::start(&path, config.clone()).await.unwrap();
        let saved = notice("already saved", NoticeLevel::Info);
        let Reply::Notice(saved_row) = client
            .command_with_id("saved-notice".into(), saved.clone())
            .await
            .unwrap()
        else {
            panic!("expected saved notice")
        };
        assert_eq!(
            saved_row,
            HistoryEntry {
                seq: 1,
                attempt_id: String::new(),
                kind: "notice".into(),
                text: "already saved".into(),
                status: "info".into()
            }
        );
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_notice BEFORE INSERT ON {table} WHEN {predicate} BEGIN SELECT RAISE(ABORT,'injected notice failure'); END;")).unwrap();
        let failed = notice("not acknowledged", NoticeLevel::Error);
        let result = timeout(
            DEADLINE,
            client.command_with_id("failed-notice".into(), failed.clone()),
        )
        .await
        .unwrap();
        assert!(result.is_err(), "failed notice must not ACK");
        assert_eq!(client.snapshot().phase, Phase::StorageError);
        assert!(client.snapshot().paused);
        assert!(
            client
                .snapshot()
                .error
                .unwrap()
                .contains("injected notice failure")
        );
        assert_eq!(count(&db, "history"), 1);
        assert_eq!(count(&db, "events"), 2);
        assert_eq!(count(&db, "command_results"), 1);
        assert_eq!(count(&db, "messages"), 0);
        assert_eq!(count(&db, "attempts"), 0);
        assert!(matches!(
            client
                .command_with_id("saved-notice".into(), saved)
                .await
                .unwrap(),
            Reply::Notice(entry) if entry == saved_row
        ));
        db.execute_batch("DROP TRIGGER fail_notice").unwrap();
        assert!(
            client
                .command(notice("do not log the fault", NoticeLevel::Error))
                .await
                .is_err()
        );
        assert_eq!(count(&db, "history"), 1);
        assert_eq!(
            rows(&client, history(None, 10)).await[0].text,
            "already saved"
        );
        client.shutdown().await.unwrap();
        let reopened = Client::start(&path, config).await.unwrap();
        let Reply::Notice(saved_after_repair) = reopened
            .command_with_id("failed-notice".into(), failed)
            .await
            .unwrap()
        else {
            panic!("expected saved notice")
        };
        assert_eq!(
            saved_after_repair,
            HistoryEntry {
                seq: 2,
                attempt_id: String::new(),
                kind: "notice".into(),
                text: "not acknowledged".into(),
                status: "error".into()
            }
        );
        assert_eq!(count(&db, "history"), 2);
        reopened.shutdown().await.unwrap();
    }
}

#[test]
fn completion_detail_is_bounded_single_line_and_correlated_with_the_real_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "send",
            &Command::Send {
                text: "input".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let started = store.start_next(&config).unwrap().unwrap();
    let reason = format!("bad\r\n\t\x1b\u{2028}{}", "\u{e9}".repeat(1024));
    store
        .finish_detail(
            Completion {
                attempt_id: &started.attempt_id,
                text: "partial",
                status: "failed",
            },
            Some(&reason),
        )
        .unwrap();
    let rows = store.history(None, 10).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].status, "failed");
    assert_eq!(rows[1].text, "partial");
    assert_eq!(rows[2].kind, "notice");
    assert_eq!(rows[2].status, "error");
    assert_eq!(rows[2].attempt_id, started.attempt_id);
    assert!(rows[2].text.starts_with("error: bad"));
    assert!(rows[2].text.len() <= MAX_NOTICE_BYTES);
    assert!(rows[2].text.len() >= MAX_NOTICE_BYTES - 1);
    assert!(
        !rows[2]
            .text
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    );
    let db = Connection::open(&path).unwrap();
    assert_eq!(count(&db, "attempts"), 1);
    assert_eq!(count(&db, "messages"), 1);
    assert_eq!(count(&db, "command_results"), 1);
    assert_eq!(
        store
            .events(0, 10)
            .unwrap()
            .last()
            .unwrap()
            .attempt_id
            .as_ref(),
        Some(&started.attempt_id)
    );
    drop(store);
    let reopened = Store::open(&path, &config).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.transcript(None, None, 10).unwrap()).unwrap(),
        serde_json::to_value(rows).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_notice_and_terminal_completion_commit_together_or_not_at_all() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Interrupted;
    config.provider.first_event_ms = 100;
    config.provider.delay_ms = 1;
    let client = Client::start(&path, config.clone()).await.unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_detail BEFORE INSERT ON history WHEN NEW.kind='notice' BEGIN SELECT RAISE(ABORT,'injected completion notice failure'); END;").unwrap();
    client
        .command(Command::Send {
            text: "partial failure".into(),
        })
        .await
        .unwrap();
    let mut snapshots = client.subscribe();
    timeout(DEADLINE, async {
        loop {
            if snapshots.borrow_and_update().phase == Phase::StorageError {
                break;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        client
            .snapshot()
            .active
            .unwrap()
            .text
            .starts_with("[test provider]")
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT status FROM attempts", [], |r| r.get(0))
            .unwrap(),
        "running"
    );
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT status FROM history WHERE kind='assistant'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "running"
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT state FROM messages", [], |r| r.get(0))
            .unwrap(),
        "processing"
    );
    assert_eq!(count(&db, "history"), 2);
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM events WHERE kind IN ('attempt_finished','notice_recorded')",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_detail").unwrap();
    assert!(
        client
            .command(notice("no recursive error", NoticeLevel::Error))
            .await
            .is_err()
    );
    client.shutdown().await.unwrap();
    let reopened = Client::start(&path, config).await.unwrap();
    let restored = rows(&reopened, history(None, 10)).await;
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[1].status, "cancelled");
    assert!(!restored[1].text.is_empty());
    reopened.shutdown().await.unwrap();
}

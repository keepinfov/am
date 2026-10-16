use am::{
    config::Config,
    runtime::{Client, Command, NativeEntry, NoticeLevel, Phase, Reply},
    storage::{Completion, Store},
};
use rusqlite::Connection;

fn action(text: &str, category: &str) -> Command {
    Command::RecordAction {
        text: text.into(),
        level: NoticeLevel::Status,
        category: category.into(),
    }
}

async fn page(
    client: &Client,
    before: Option<i64>,
    after: Option<i64>,
    limit: usize,
) -> Vec<NativeEntry> {
    let Reply::NativeTranscript(entries) = client
        .command(Command::NativeTranscript {
            before,
            after,
            limit,
        })
        .await
        .unwrap()
    else {
        panic!("expected native transcript")
    };
    entries
}

fn counts(db: &Connection) -> (i64, i64, i64, i64) {
    db.query_row(
        "SELECT (SELECT COUNT(*) FROM history),(SELECT COUNT(*) FROM actions),
                (SELECT COUNT(*) FROM events),(SELECT COUNT(*) FROM command_results)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

#[test]
fn categories_survive_reopen_and_upgrade_and_duplicate_text_uses_notice_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    for (index, (text, category)) in [
        ("session paused", "queue"),
        ("status set braille", "settings"),
        ("status-color enabled", "settings"),
        ("resumed", "queue"),
        ("identical notice", "settings"),
        ("identical notice", "queue"),
    ]
    .into_iter()
    .enumerate()
    {
        let command = action(text, category);
        let Reply::Notice(saved) = store
            .apply(&format!("action-{index}"), &command, &config, None)
            .unwrap()
        else {
            panic!("expected notice receipt")
        };
        let native = store.native_transcript(None, None, 1).unwrap();
        assert_eq!(native[0].entry, saved);
        assert_eq!(native[0].category.as_deref(), Some(category));
        assert!(matches!(
            store.apply(&format!("action-{index}"), &command, &config, None).unwrap(),
            Reply::Notice(entry) if entry == saved
        ));
    }
    for (index, text) in ["identical notice", "session paused", "status set braille"]
        .into_iter()
        .enumerate()
    {
        store
            .apply(
                &format!("generic-{index}"),
                &Command::RecordNotice {
                    text: text.into(),
                    level: NoticeLevel::Status,
                },
                &config,
                None,
            )
            .unwrap();
    }
    let expected = store.native_transcript(None, Some(0), 100).unwrap();
    assert_eq!(
        expected
            .iter()
            .map(|row| row.category.as_deref())
            .collect::<Vec<_>>(),
        [
            Some("queue"),
            Some("settings"),
            Some("settings"),
            Some("queue"),
            Some("settings"),
            Some("queue"),
            None,
            None,
            None,
        ]
    );
    assert_eq!(
        expected
            .iter()
            .map(|row| row.entry.clone())
            .collect::<Vec<_>>(),
        store.transcript(None, Some(0), 100).unwrap()
    );
    let encoded = serde_json::to_string(&Reply::NativeTranscript(expected.clone())).unwrap();
    assert!(matches!(
        serde_json::from_str::<Reply>(&encoded).unwrap(),
        Reply::NativeTranscript(entries) if entries == expected
    ));
    drop(store);
    let db = Connection::open(&path).unwrap();
    // Simulate a session written before the additive receipt index existed.
    db.execute_batch("DROP INDEX command_results_notice_seq")
        .unwrap();
    let store = Store::open(&path, &config).unwrap();
    assert_eq!(
        store.native_transcript(None, Some(0), 100).unwrap(),
        expected
    );
    let plan: Vec<String> = db
        .prepare("EXPLAIN QUERY PLAN SELECT command FROM command_results INDEXED BY command_results_notice_seq WHERE json_valid(reply) AND json_extract(reply,'$.Notice.seq')=?1 LIMIT 1")
        .unwrap()
        .query_map([1], |r| r.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        plan.iter().any(
            |detail| detail.contains("SEARCH") && detail.contains("command_results_notice_seq")
        ),
        "{plan:?}"
    );
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_transcript_pages_match_both_directions_and_actor_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.history_page = 3;
    let mut store = Store::open(&path, &config).unwrap();
    for index in 0..2 {
        store
            .apply(
                &format!("before-{index}"),
                &action("before response", "settings"),
                &config,
                None,
            )
            .unwrap();
        store
            .apply(
                &format!("send-{index}"),
                &Command::Send {
                    text: "prompt".into(),
                },
                &config,
                None,
            )
            .unwrap();
        let started = store.start_next(&config).unwrap().unwrap();
        store
            .checkpoint(&started.attempt_id, "durable checkpoint")
            .unwrap();
        store
            .apply(
                &format!("during-{index}"),
                &action("during response", "queue"),
                &config,
                None,
            )
            .unwrap();
        let running = store.native_transcript(None, None, 2).unwrap();
        assert_eq!(running[0].entry.status, "running");
        assert_eq!(running[0].entry.text, "durable checkpoint");
        assert_eq!(running[0].category, None);
        assert_eq!(running[1].category.as_deref(), Some("queue"));
        store
            .finish(Completion {
                attempt_id: &started.attempt_id,
                text: "response",
                status: "completed",
            })
            .unwrap();
    }
    let expected = store.native_transcript(None, Some(0), 100).unwrap();
    assert_eq!(expected.len(), 8);
    assert_eq!(
        expected
            .iter()
            .map(|row| row.entry.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "notice",
            "user",
            "assistant",
            "notice",
            "notice",
            "user",
            "assistant",
            "notice"
        ]
    );
    for before in [None, Some(1), Some(4), Some(9)] {
        for after in [None, Some(0), Some(1), Some(4), Some(8)] {
            for limit in [0, 1, 3, 100] {
                assert_eq!(
                    store
                        .native_transcript(before, after, limit)
                        .unwrap()
                        .into_iter()
                        .map(|row| row.entry)
                        .collect::<Vec<_>>(),
                    store.transcript(before, after, limit).unwrap()
                );
            }
        }
    }
    drop(store);
    let client = Client::start(&path, config).await.unwrap();
    let mut forwards = Vec::new();
    let mut after = 0;
    loop {
        let rows = page(&client, None, Some(after), 1).await;
        if rows.is_empty() {
            break;
        }
        after = rows[0].entry.seq;
        forwards.extend(rows);
    }
    assert_eq!(forwards, expected);
    let mut backwards = Vec::new();
    let mut before = None;
    loop {
        let rows = page(&client, before, None, 1).await;
        if rows.is_empty() {
            break;
        }
        before = Some(rows[0].entry.seq);
        backwards.extend(rows);
    }
    backwards.reverse();
    assert_eq!(backwards, expected);
    assert_eq!(
        page(&client, None, Some(0), usize::MAX).await,
        expected[..3]
    );
    assert_eq!(page(&client, None, None, usize::MAX).await, expected[5..]);
    assert_eq!(page(&client, None, Some(0), 0).await, expected[..1]);
    assert_eq!(page(&client, None, None, 0).await, expected[7..]);
    assert_eq!(page(&client, Some(7), Some(3), 100).await, expected[3..6]);
    assert!(page(&client, Some(3), Some(3), 1).await.is_empty());
    client.shutdown().await.unwrap();
}

#[test]
fn failed_action_and_composer_save_leave_no_metadata_or_orphan_receipts() {
    for (table, command) in [
        ("history", action("not saved", "queue")),
        ("actions", action("not saved", "queue")),
        ("events", action("not saved", "queue")),
        ("command_results", action("not saved", "queue")),
        (
            "command_results",
            Command::SaveComposer {
                text: "not saved".into(),
                cursor: 0,
                revision: 0,
            },
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply("saved", &action("not saved", "settings"), &config, None)
            .unwrap();
        let expected = store.native_transcript(None, None, 100).unwrap();
        let composer = store.composer().unwrap();
        let db = Connection::open(&path).unwrap();
        let saved_counts = counts(&db);
        db.execute_batch(&format!("CREATE TRIGGER fail_save BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'save fault'); END;")).unwrap();
        assert!(store.apply("failed", &command, &config, None).is_err());
        assert_eq!(counts(&db), saved_counts);
        assert_eq!(store.composer().unwrap(), composer);
        assert_eq!(store.native_transcript(None, None, 100).unwrap(), expected);
        assert!(store.previous("failed", &command).unwrap().is_none());
        db.execute_batch("DROP TRIGGER fail_save").unwrap();
        drop(store);
        let mut store = Store::open(&path, &config).unwrap();
        assert_eq!(store.native_transcript(None, None, 100).unwrap(), expected);
        store.apply("failed", &command, &config, None).unwrap();
        let rows = store.native_transcript(None, None, 100).unwrap();
        if matches!(command, Command::RecordAction { .. }) {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[1].entry.seq, 2);
            assert_eq!(rows[1].category.as_deref(), Some("queue"));
        } else {
            assert_eq!(rows, expected);
            assert_eq!(store.composer().unwrap().text, "not saved");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_reads_do_not_write_receipts_events_or_publish_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, Config::default()).await.unwrap();
    client
        .command_with_id("used".into(), Command::Pause)
        .await
        .unwrap();
    client
        .command(action("status set braille", "settings"))
        .await
        .unwrap();
    let mut snapshots = client.subscribe();
    let before = snapshots.borrow_and_update().clone();
    let db = Connection::open(&path).unwrap();
    let saved_counts = counts(&db);
    for id in ["used", "read-only"] {
        let command = Command::NativeTranscript {
            before: None,
            after: None,
            limit: 10,
        };
        let command = serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
        assert!(matches!(
            client.command_with_id(id.into(), command).await.unwrap(),
            Reply::NativeTranscript(rows) if rows.len() == 1 && rows[0].category.as_deref() == Some("settings")
        ));
    }
    assert_eq!(counts(&db), saved_counts);
    assert!(!snapshots.has_changed().unwrap());
    let after = client.snapshot();
    assert_eq!(after.cursor, before.cursor);
    assert_eq!(after.last_event, before.last_event);
    assert_eq!(after.phase_started, before.phase_started);
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.paused, before.paused);
    client
        .command_with_id("read-only".into(), action("resumed", "queue"))
        .await
        .unwrap();
    assert_eq!(page(&client, None, None, 10).await.len(), 2);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_index_tolerates_malformed_replies_without_bypassing_corruption_errors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let command = action("saved", "settings");
    store.apply("saved", &command, &config, None).unwrap();
    let db = Connection::open(&path).unwrap();
    let reply: String = db
        .query_row(
            "SELECT reply FROM command_results WHERE id='saved'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE command_results SET reply='malformed' WHERE id='saved'",
        [],
    )
    .unwrap();
    assert!(store.previous("saved", &command).is_err());
    drop(store);
    db.execute_batch("DROP INDEX command_results_notice_seq")
        .unwrap();
    let store = Store::open(&path, &config).unwrap();
    assert!(store.previous("saved", &command).is_err());
    assert_eq!(
        store.native_transcript(None, None, 1).unwrap()[0].category,
        None
    );
    db.execute(
        "UPDATE command_results SET reply=?1 WHERE id='saved'",
        [reply],
    )
    .unwrap();
    assert_eq!(
        store.native_transcript(None, None, 1).unwrap()[0]
            .category
            .as_deref(),
        Some("settings")
    );
    db.execute(
        "UPDATE command_results SET command='malformed' WHERE id='saved'",
        [],
    )
    .unwrap();
    assert!(store.native_transcript(None, None, 1).is_err());
    drop(store);
    assert!(Client::start(&path, config).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_read_faults_publish_storage_error_and_remain_readable_after_repair() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, Config::default()).await.unwrap();
    client.command(action("saved", "queue")).await.unwrap();
    let expected = page(&client, None, None, 10).await;
    let db = Connection::open(&path).unwrap();
    db.execute_batch("ALTER TABLE history RENAME TO hidden_history")
        .unwrap();
    assert!(
        client
            .command(Command::NativeTranscript {
                before: None,
                after: None,
                limit: 10
            })
            .await
            .is_err()
    );
    assert_eq!(client.snapshot().phase, Phase::StorageError);
    assert!(client.snapshot().paused);
    db.execute_batch("ALTER TABLE hidden_history RENAME TO history")
        .unwrap();
    let saved_counts = counts(&db);
    assert_eq!(page(&client, None, None, 10).await, expected);
    assert_eq!(counts(&db), saved_counts);
    assert_eq!(client.snapshot().phase, Phase::StorageError);
    client.shutdown().await.unwrap();
}

use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, HistoryEntry, Phase, Reply, Snapshot},
    storage::{Completion, Store},
};
use rusqlite::Connection;
use std::time::Duration;
use tokio::time::timeout;

async fn wait(client: &Client, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let mut snapshots = client.subscribe();
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

async fn transcript(
    client: &Client,
    before: Option<i64>,
    after: Option<i64>,
    limit: usize,
) -> Vec<HistoryEntry> {
    let Reply::History(entries) = client
        .command(Command::Transcript {
            before,
            after,
            limit,
        })
        .await
        .unwrap()
    else {
        panic!("expected transcript")
    };
    entries
}

async fn inputs(client: &Client, limit: usize) -> Vec<String> {
    let Reply::Inputs(entries) = client.command(Command::Inputs { limit }).await.unwrap() else {
        panic!("expected inputs")
    };
    entries
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_pages_both_directions_and_history_still_excludes_running() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.history_page = 3;
    let mut store = Store::open(&path, &config).unwrap();
    let mut attempts = Vec::new();
    for index in 0..4 {
        store
            .apply(
                &format!("send-{index}"),
                &Command::Send {
                    text: format!("prompt {index}"),
                },
                &config,
                None,
            )
            .unwrap();
        let started = store.start_next(&config).unwrap().unwrap();
        store
            .checkpoint(&started.attempt_id, "saved checkpoint")
            .unwrap();
        if index < 3 {
            store
                .finish(Completion {
                    attempt_id: &started.attempt_id,
                    text: "finished response",
                    status: "completed",
                })
                .unwrap();
        }
        attempts.push(started.attempt_id);
    }
    let entries = store.transcript(None, Some(0), 100).unwrap();
    assert_eq!(entries.len(), 8);
    for (pair, attempt_id) in entries.as_chunks::<2>().0.iter().zip(&attempts) {
        assert_eq!(&pair[0].attempt_id, attempt_id);
        assert_eq!(&pair[1].attempt_id, attempt_id);
    }
    let running = entries.last().unwrap();
    assert_eq!(running.kind, "assistant");
    assert_eq!(running.status, "running");
    assert_eq!(running.text, "saved checkpoint");
    let started = store
        .events(0, 100)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "attempt_started")
        .map(|event| event.attempt_id.unwrap())
        .collect::<Vec<_>>();
    assert_eq!(started, attempts);

    let seqs = |rows: Vec<HistoryEntry>| rows.into_iter().map(|row| row.seq).collect::<Vec<_>>();
    assert_eq!(seqs(store.transcript(None, None, 3).unwrap()), [6, 7, 8]);
    assert_eq!(seqs(store.transcript(Some(6), None, 3).unwrap()), [3, 4, 5]);
    assert_eq!(seqs(store.transcript(Some(3), None, 3).unwrap()), [1, 2]);
    assert_eq!(seqs(store.transcript(None, Some(0), 3).unwrap()), [1, 2, 3]);
    assert_eq!(seqs(store.transcript(None, Some(3), 3).unwrap()), [4, 5, 6]);
    assert_eq!(seqs(store.transcript(None, Some(6), 3).unwrap()), [7, 8]);
    assert_eq!(
        seqs(store.transcript(Some(7), Some(3), 3).unwrap()),
        [4, 5, 6]
    );
    assert!(store.transcript(Some(3), Some(3), 3).unwrap().is_empty());
    assert!(store.transcript(None, Some(8), 3).unwrap().is_empty());
    assert!(store.transcript(Some(1), None, 3).unwrap().is_empty());
    let history = store.history(None, 3).unwrap();
    assert_eq!(seqs(history.clone()), [5, 6, 7]);
    assert!(history.iter().all(|row| row.status != "running"));
    assert_eq!(history[2].attempt_id, running.attempt_id);
    assert_eq!(seqs(store.history(Some(5), 3).unwrap()), [2, 3, 4]);
    drop(store);

    let client = Client::start(&path, config).await.unwrap();
    assert_eq!(
        seqs(transcript(&client, None, Some(0), usize::MAX).await),
        [1, 2, 3]
    );
    assert_eq!(
        seqs(transcript(&client, None, None, usize::MAX).await),
        [6, 7, 8]
    );
    assert_eq!(seqs(transcript(&client, None, Some(0), 0).await), [1]);
    assert_eq!(seqs(transcript(&client, None, None, 0).await), [8]);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_transcript_returns_durable_checkpoint_and_active_attempt_id() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.first_event_ms = 0;
    config.provider.delay_ms = 60_000;
    config.limits.checkpoint_ms = 10;
    let client = Client::start(&path, config).await.unwrap();
    client
        .command(Command::Send {
            text: "live prompt".into(),
        })
        .await
        .unwrap();
    let snapshot = wait(&client, |snapshot| {
        snapshot
            .active
            .as_ref()
            .is_some_and(|active| active.durable_bytes > 0)
    })
    .await;
    let active = snapshot.active.unwrap();
    let rows = transcript(&client, None, None, 10).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].attempt_id, active.attempt_id);
    assert_eq!(rows[1].attempt_id, active.attempt_id);
    assert_eq!(rows[1].status, "running");
    assert!(!rows[1].text.is_empty());
    let database = Connection::open(&path).unwrap();
    let message_id: String = database
        .query_row(
            "SELECT message_id FROM attempts WHERE id=?1",
            [&active.attempt_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active.message_id, message_id);
    let saved: String = database
        .query_row(
            "SELECT text FROM history WHERE seq=?1",
            [rows[1].seq],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows[1].text, saved);
    let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 10,
        })
        .await
        .unwrap()
    else {
        panic!("expected history")
    };
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].kind, "user");
    assert_eq!(history[0].attempt_id, active.attempt_id);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inputs_return_latest_saved_text_across_states_in_chronological_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Burst;
    config.provider.first_event_ms = 0;
    config.provider.chunks = 1;
    config.limits.queue_messages = 128;
    let client = Client::start(&path, config.clone()).await.unwrap();
    client
        .command(Command::Send {
            text: "handled input".into(),
        })
        .await
        .unwrap();
    wait(&client, |snapshot| {
        snapshot.cursor >= 4 && snapshot.active.is_none() && snapshot.queue.is_empty()
    })
    .await;
    assert_eq!(inputs(&client, 10).await, ["handled input"]);
    client.command(Command::Pause).await.unwrap();
    for index in 1..105 {
        client
            .command(Command::Send {
                text: format!("input {index}"),
            })
            .await
            .unwrap();
    }
    let queue = client.snapshot().queue;
    let cancelled = &queue[99];
    client
        .command(Command::Cancel {
            id: cancelled.id.clone(),
            revision: cancelled.revision,
        })
        .await
        .unwrap();
    let edited = &queue[102];
    let Reply::Editing(lease) = client
        .command(Command::BeginEdit {
            id: edited.id.clone(),
            revision: edited.revision,
        })
        .await
        .unwrap()
    else {
        panic!("expected edit lease")
    };
    client
        .command(Command::FinishEdit {
            id: lease.id,
            revision: lease.revision,
            text: "saved edit".into(),
        })
        .await
        .unwrap();
    let editing = &queue[103];
    client
        .command(Command::BeginEdit {
            id: editing.id.clone(),
            revision: editing.revision,
        })
        .await
        .unwrap();
    assert!(
        client
            .command(Command::Send { text: " ".into() })
            .await
            .is_err()
    );
    assert_eq!(
        inputs(&client, 5).await,
        [
            "input 100",
            "input 101",
            "input 102",
            "saved edit",
            "input 104"
        ]
    );
    assert_eq!(inputs(&client, 0).await, ["input 104"]);
    assert_eq!(inputs(&client, 1).await, ["input 104"]);
    let all = inputs(&client, usize::MAX).await;
    assert_eq!(all.len(), 100);
    assert_eq!(all.first().unwrap(), "input 5");
    assert_eq!(all.last().unwrap(), "input 104");
    assert_eq!(inputs(&client, 100).await, all);
    let database = Connection::open(&path).unwrap();
    let states: Vec<String> = database
        .prepare("SELECT DISTINCT state FROM messages ORDER BY state")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(states, ["cancelled", "editing", "handled", "queued"]);
    client.shutdown().await.unwrap();
    let reopened = Client::start(&path, config).await.unwrap();
    assert_eq!(inputs(&reopened, usize::MAX).await, all);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_idle_mutations_do_not_publish_again_during_reads() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::start(&directory.path().join("session.sqlite3"), Config::default())
        .await
        .unwrap();
    let mut snapshots = client.subscribe();
    for _ in 0..32 {
        client.command(Command::Pause).await.unwrap();
        let acknowledged = snapshots.borrow_and_update().clone();
        assert!(acknowledged.paused);
        for _ in 0..2 {
            assert!(matches!(
                client.command(Command::Composer).await.unwrap(),
                Reply::Composer(_)
            ));
        }
        assert!(!snapshots.has_changed().unwrap());
        assert_eq!(client.snapshot().cursor, acknowledged.cursor);
    }
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn focused_reads_have_no_receipts_events_or_snapshot_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, Config::default()).await.unwrap();
    client
        .command_with_id("used".into(), Command::Pause)
        .await
        .unwrap();
    let mut snapshots = client.subscribe();
    let before = snapshots.borrow_and_update().clone();
    let database = Connection::open(&path).unwrap();
    let counts = || {
        database
            .query_row(
                "SELECT (SELECT COUNT(*) FROM command_results),(SELECT COUNT(*) FROM events)",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap()
    };
    let saved_counts = counts();
    for id in ["used", "read-only"] {
        assert!(
            matches!(client.command_with_id(id.into(), Command::Transcript { before: None, after: None, limit: 10 }).await.unwrap(), Reply::History(rows) if rows.is_empty())
        );
        assert!(
            matches!(client.command_with_id(id.into(), Command::Inputs { limit: 10 }).await.unwrap(), Reply::Inputs(rows) if rows.is_empty())
        );
    }
    assert_eq!(counts(), saved_counts);
    assert!(!snapshots.has_changed().unwrap());
    let after = client.snapshot();
    assert_eq!(after.cursor, before.cursor);
    assert_eq!(after.last_event, before.last_event);
    assert_eq!(after.phase_started, before.phase_started);
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.paused, before.paused);
    client
        .command_with_id(
            "read-only".into(),
            Command::Send {
                text: "real input".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(inputs(&client, 10).await, ["real input"]);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn focused_read_faults_publish_storage_error_and_reads_remain_available() {
    for (table, command) in [
        (
            "history",
            Command::Transcript {
                before: None,
                after: Some(0),
                limit: 10,
            },
        ),
        ("messages", Command::Inputs { limit: 10 }),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.scenario = Scenario::Waiting;
        let client = Client::start(&path, config).await.unwrap();
        client
            .command(Command::Send {
                text: "waiting input".into(),
            })
            .await
            .unwrap();
        wait(&client, |snapshot| snapshot.active.is_some()).await;
        let database = Connection::open(&path).unwrap();
        database
            .execute_batch(&format!("ALTER TABLE {table} RENAME TO hidden_table"))
            .unwrap();
        assert!(client.command(command.clone()).await.is_err());
        let snapshot = wait(&client, |snapshot| snapshot.phase == Phase::StorageError).await;
        assert!(snapshot.paused);
        assert!(snapshot.error.is_some());
        database
            .execute_batch(&format!("ALTER TABLE hidden_table RENAME TO {table}"))
            .unwrap();
        client.command(command).await.unwrap();
        assert_eq!(inputs(&client, 10).await, ["waiting input"]);
        assert_eq!(
            transcript(&client, None, None, 10).await[1].status,
            "running"
        );
        assert_eq!(client.snapshot().phase, Phase::StorageError);
        client.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_history_search_finds_old_unicode_dialogue_and_literal_wildcards() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    for index in 0..45 {
        let text = match index {
            0 => {
                "Old \u{00c4}PFEL \u{03a3}\u{039f}\u{03a6}\u{0399}\u{0391} 100%_literal".to_owned()
            }
            1 => "100xxliteral".to_owned(),
            2 => format!("{} distant NEEDLE", "x".repeat(200)),
            _ => format!("recent prompt {index}"),
        };
        store
            .apply(
                &format!("search-{index}"),
                &Command::Send { text },
                &config,
                None,
            )
            .unwrap();
        let attempt = store.start_next(&config).unwrap().unwrap();
        store
            .finish(Completion {
                attempt_id: &attempt.attempt_id,
                text: &format!("response {}", "x".repeat(200)),
                status: "completed",
            })
            .unwrap();
    }
    assert!(
        store
            .transcript(None, None, 16)
            .unwrap()
            .iter()
            .all(|entry| entry.seq > 1)
    );
    for query in [
        "\u{00e4}pfel",
        "\u{03c3}\u{03bf}\u{03c6}\u{03b9}\u{03b1}",
        "%",
        "_",
        "%_literal",
    ] {
        let matches = store.search_history(query, usize::MAX).unwrap();
        assert_eq!(matches.len(), 1, "{query}");
        assert_eq!(matches[0].seq, 1);
        assert_eq!(matches[0].kind, "user");
    }
    assert_eq!(
        store.search_history("", usize::MAX).unwrap().len(),
        am::runtime::MAX_HISTORY_MATCHES
    );
    assert_eq!(store.search_history("response", 0).unwrap().len(), 1);
    assert!(
        store
            .search_history("response", 10)
            .unwrap()
            .iter()
            .all(|entry| entry.preview.chars().count() <= am::runtime::MAX_HISTORY_PREVIEW_CHARS)
    );
    assert!(
        store
            .search_history("1", 32)
            .unwrap()
            .iter()
            .any(|entry| entry.seq == 1)
    );
    assert!(store.search_history("never saved", 10).unwrap().is_empty());
    let distant = store.search_history("needle", 10).unwrap();
    assert_eq!(distant.len(), 1);
    assert_eq!(distant[0].seq, 5);
    assert!(!distant[0].preview.to_lowercase().contains("needle"));
    assert!(store.search_history(&"x".repeat(1025), 1).is_err());
    assert!(store.search_history("line\nline", 1).is_err());
    assert!(store.search_history("line\u{2028}line", 1).is_err());
    drop(store);

    let client = Client::start(&path, config).await.unwrap();
    let Reply::HistoryMatches(matches) = client
        .command(Command::SearchHistory {
            query: "\u{00e4}pfel".into(),
            limit: 10,
        })
        .await
        .unwrap()
    else {
        panic!("expected search summaries")
    };
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].seq, 1);
    let Reply::HistoryMatches(matches) = client
        .command(Command::SearchHistory {
            query: String::new(),
            limit: usize::MAX,
        })
        .await
        .unwrap()
    else {
        panic!("expected bounded summaries")
    };
    assert_eq!(matches.len(), am::runtime::MAX_HISTORY_MATCHES);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_anchor_validates_exact_saved_entry_without_mutating_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "seed",
            &Command::Send {
                text: "saved prompt".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let attempt = store.start_next(&config).unwrap().unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt.attempt_id,
            text: "saved response",
            status: "completed",
        })
        .unwrap();
    assert_eq!(
        store.transcript_anchor(1, 10).unwrap()[0].text,
        "saved prompt"
    );
    assert_eq!(store.transcript_anchor(2, 10).unwrap().len(), 1);
    for seq in [0, -1, 3, i64::MAX] {
        assert!(store.transcript_anchor(seq, 10).is_err(), "{seq}");
    }
    drop(store);
    let client = Client::start(&path, config).await.unwrap();
    let database = Connection::open(&path).unwrap();
    // A hole must not silently anchor to the next existing entry.
    database
        .execute("DELETE FROM history WHERE seq=1", [])
        .unwrap();
    let mut snapshots = client.subscribe();
    snapshots.borrow_and_update();
    let before = client.snapshot();
    let counts = || {
        database
            .query_row(
                "SELECT (SELECT COUNT(*) FROM command_results),(SELECT COUNT(*) FROM events)",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap()
    };
    let saved_counts = counts();
    for seq in [0, -1, 1, 3, i64::MAX] {
        assert!(
            client
                .command_with_id(
                    "read-only".into(),
                    Command::TranscriptAnchor { seq, limit: 8 }
                )
                .await
                .is_err()
        );
    }
    let Reply::HistoryAnchor { seq, entries } = client
        .command_with_id(
            "read-only".into(),
            Command::TranscriptAnchor { seq: 2, limit: 0 },
        )
        .await
        .unwrap()
    else {
        panic!("expected anchor")
    };
    assert_eq!(seq, 2);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seq, 2);
    client
        .command_with_id(
            "read-only".into(),
            Command::SearchHistory {
                query: "saved".into(),
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(counts(), saved_counts);
    assert!(!snapshots.has_changed().unwrap());
    assert_eq!(client.snapshot().phase, before.phase);
    assert_eq!(client.snapshot().last_event, before.last_event);
    assert_eq!(client.snapshot().cursor, before.cursor);
    client
        .command_with_id("read-only".into(), Command::Pause)
        .await
        .unwrap();
    client.shutdown().await.unwrap();
}

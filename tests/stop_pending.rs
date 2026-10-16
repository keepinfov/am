use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, QueueState, Reply, Snapshot},
    storage::{Completion, Store},
};
use rusqlite::Connection;
use std::time::Duration;
use tokio::time::timeout;

fn stop(attempt: Option<&str>, paused: bool) -> Command {
    Command::StopPending {
        expected_attempt_id: attempt.map(str::to_owned),
        expected_paused: paused,
    }
}

fn assert_stopped(reply: Reply, paused: bool, cancelled: bool) {
    assert!(
        matches!(reply, Reply::Stopped { paused: p, cancelled: c } if p == paused && c == cancelled)
    );
}

async fn wait(client: &Client, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let mut updates = client.subscribe();
    timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = updates.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checked_stop_commits_pause_decision_and_automatically_starts_fifo_pending_work() {
    for paused_before in [false, true] {
        for pending in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.sqlite3");
            let mut config = Config::default();
            config.provider.scenario = Scenario::Waiting;
            let client = Client::start(&path, config).await.unwrap();
            client
                .command(Command::Send {
                    text: "active".into(),
                })
                .await
                .unwrap();
            let first = wait(&client, |s| s.active.is_some())
                .await
                .active
                .unwrap()
                .attempt_id;
            if paused_before {
                client.command(Command::Pause).await.unwrap();
            }
            if pending {
                for text in ["next", "last"] {
                    client
                        .command(Command::Send { text: text.into() })
                        .await
                        .unwrap();
                }
            }
            let command = stop(Some(&first), paused_before);
            let wire = serde_json::to_string(&command).unwrap();
            assert!(wire.starts_with("{\"StopPending\":"));
            let command: Command = serde_json::from_str(&wire).unwrap();
            assert_stopped(
                client
                    .command_with_id("stop".into(), command.clone())
                    .await
                    .unwrap(),
                !pending,
                true,
            );
            assert_eq!(client.snapshot().paused, !pending);
            assert!(
                client
                    .snapshot()
                    .active
                    .as_ref()
                    .is_none_or(|a| a.attempt_id != first)
            );
            if pending {
                let next = wait(&client, |s| s.active.is_some()).await;
                assert_ne!(next.active.as_ref().unwrap().attempt_id, first);
                assert_eq!(next.queue.len(), 1);
                assert_eq!(next.queue[0].text, "last");
                let Reply::History(history) = client
                    .command(Command::History {
                        before: None,
                        limit: 20,
                    })
                    .await
                    .unwrap()
                else {
                    panic!()
                };
                assert_eq!(history[1].status, "cancelled");
                assert_eq!(history[2].text, "next");
                let before = client.snapshot();
                // Both exact replay and a fresh stale selection must leave the successor alone.
                assert_stopped(
                    client
                        .command_with_id("stop".into(), command.clone())
                        .await
                        .unwrap(),
                    false,
                    true,
                );
                assert!(client.command(command.clone()).await.is_err());
                assert_eq!(
                    client.snapshot().active.unwrap().attempt_id,
                    before.active.unwrap().attempt_id
                );
                assert!(!client.snapshot().paused);
                assert_eq!(client.snapshot().cursor, before.cursor);
            } else {
                assert!(client.snapshot().active.is_none());
                let before = client.snapshot().cursor;
                assert_stopped(
                    client
                        .command_with_id("stop".into(), command.clone())
                        .await
                        .unwrap(),
                    true,
                    true,
                );
                assert_eq!(client.snapshot().cursor, before);
            }
            client.shutdown().await.unwrap();
            let reopened = Client::start(&path, Config::default()).await.unwrap();
            let before = reopened.snapshot();
            assert_stopped(
                reopened
                    .command_with_id("stop".into(), command)
                    .await
                    .unwrap(),
                !pending,
                true,
            );
            assert!(reopened.snapshot().paused);
            assert!(reopened.snapshot().active.is_none());
            assert_eq!(reopened.snapshot().cursor, before.cursor);
            assert_eq!(
                serde_json::to_value(reopened.snapshot().queue).unwrap(),
                serde_json::to_value(before.queue).unwrap()
            );
            reopened.shutdown().await.unwrap();
        }
    }
}

#[test]
fn stop_completion_pause_and_receipt_roll_back_together_on_failed_transactions() {
    for (table, operation) in [
        ("attempts", "UPDATE"),
        ("history", "UPDATE"),
        ("messages", "UPDATE"),
        ("artifacts", "UPDATE"),
        ("actions", "UPDATE"),
        ("events", "INSERT"),
        ("session", "UPDATE"),
        ("command_results", "INSERT"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply(
                "active",
                &Command::Send {
                    text: "active".into(),
                },
                &config,
                None,
            )
            .unwrap();
        let attempt = store.start_next(&config).unwrap().unwrap().attempt_id;
        store
            .append_output(&attempt, "buffered output", &config.limits)
            .unwrap();
        store
            .apply(
                "next",
                &Command::Send {
                    text: "next".into(),
                },
                &config,
                None,
            )
            .unwrap();
        store
            .apply("pause", &Command::Pause, &config, None)
            .unwrap();
        let cursor = store.cursor().unwrap();
        let history = store.transcript(None, None, 20).unwrap();
        let queue = serde_json::to_value(store.queue(20).unwrap()).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_stop BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'stop fault'); END;")).unwrap();
        let command = stop(Some(&attempt), true);
        let completion = || {
            Some(Completion {
                attempt_id: &attempt,
                text: "buffered output",
                status: "cancelled",
            })
        };
        assert!(
            store
                .apply("stop", &command, &config, completion())
                .is_err(),
            "{table}"
        );
        assert_eq!(store.cursor().unwrap(), cursor);
        assert_eq!(store.transcript(None, None, 20).unwrap(), history);
        assert_eq!(
            serde_json::to_value(store.queue(20).unwrap()).unwrap(),
            queue
        );
        assert!(store.paused().unwrap());
        assert!(store.previous("stop", &command).unwrap().is_none());
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT status FROM attempts WHERE id=?1",
                [&attempt],
                |r| r.get(0)
            )
            .unwrap(),
            "running"
        );
        db.execute_batch("DROP TRIGGER fail_stop").unwrap();
        assert_stopped(
            store
                .apply("stop", &command, &config, completion())
                .unwrap(),
            false,
            true,
        );
        assert!(!store.paused().unwrap());
        assert_eq!(store.history(None, 20).unwrap()[1].text, "buffered output");
        assert_eq!(store.history(None, 20).unwrap()[1].status, "cancelled");
    }
}

#[test]
fn durable_stop_checks_attempt_pause_and_completion_before_mutating_anything() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "send",
            &Command::Send {
                text: "active".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let attempt = store.start_next(&config).unwrap().unwrap().attempt_id;
    let cursor = store.cursor().unwrap();
    for command in [
        stop(None, false),
        stop(Some("finished"), false),
        stop(Some(""), false),
        stop(Some(&"x".repeat(129)), false),
        stop(Some(&attempt), true),
    ] {
        assert!(
            store
                .apply(
                    "reject",
                    &command,
                    &config,
                    Some(Completion {
                        attempt_id: &attempt,
                        text: "",
                        status: "cancelled"
                    })
                )
                .is_err()
        );
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(!store.paused().unwrap());
        assert!(store.previous("reject", &command).unwrap().is_none());
    }
    let command = stop(Some(&attempt), false);
    assert!(
        store
            .apply("no-completion", &command, &config, None)
            .is_err()
    );
    for (id, status) in [("wrong", "cancelled"), (attempt.as_str(), "completed")] {
        assert!(
            store
                .apply(
                    "bad-completion",
                    &command,
                    &config,
                    Some(Completion {
                        attempt_id: id,
                        text: "",
                        status
                    })
                )
                .is_err()
        );
    }
    assert_eq!(store.cursor().unwrap(), cursor);
}

#[test]
fn stop_is_not_new_admission_and_preserves_existing_queue_at_lowered_limits_and_full_artifact_quota()
 {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.queue_messages = 2;
    config.limits.output_bytes = 1024 * 1024;
    config.limits.artifact_bytes = 1024 * 1024;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "active",
            &Command::Send {
                text: "active".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let attempt = store.start_next(&config).unwrap().unwrap().attempt_id;
    for text in ["next", "last"] {
        store
            .apply(text, &Command::Send { text: text.into() }, &config, None)
            .unwrap();
    }
    store
        .append_output(
            &attempt,
            &"x".repeat(config.limits.output_bytes),
            &config.limits,
        )
        .unwrap();
    let before = serde_json::to_value(store.queue(20).unwrap()).unwrap();
    config.limits.queue_messages = 1;
    config.limits.message_bytes = 1;
    assert_stopped(
        store
            .apply(
                "stop",
                &stop(Some(&attempt), false),
                &config,
                Some(Completion {
                    attempt_id: &attempt,
                    text: "partial",
                    status: "cancelled",
                }),
            )
            .unwrap(),
        false,
        true,
    );
    assert_eq!(
        serde_json::to_value(store.queue(20).unwrap()).unwrap(),
        before
    );
    let output = store.actions(None, 20).unwrap()[0].output.clone().unwrap();
    assert_eq!(output.bytes, 1024 * 1024);
    assert!(output.incomplete);
    assert!(!store.paused().unwrap());
    // Eligibility does not bypass the existing frozen artifact admission quota.
    assert!(store.start_next(&config).is_err());
    assert_eq!(
        serde_json::to_value(store.queue(20).unwrap()).unwrap(),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checked_stop_preserves_edit_lease_and_global_dispatch_barrier() {
    for queued in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.scenario = Scenario::Waiting;
        let client = Client::start(&path, config).await.unwrap();
        client
            .command(Command::Send {
                text: "active".into(),
            })
            .await
            .unwrap();
        let attempt = wait(&client, |s| s.active.is_some())
            .await
            .active
            .unwrap()
            .attempt_id;
        client
            .command(Command::Send {
                text: "leased".into(),
            })
            .await
            .unwrap();
        let item = client.snapshot().queue[0].clone();
        let Reply::Editing(lease) = client
            .command(Command::BeginEdit {
                id: item.id,
                revision: item.revision,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        if queued {
            client
                .command(Command::Send {
                    text: "later".into(),
                })
                .await
                .unwrap();
        }
        assert_stopped(
            client.command(stop(Some(&attempt), false)).await.unwrap(),
            !queued,
            true,
        );
        assert!(client.snapshot().active.is_none());
        assert_eq!(client.snapshot().queue[0].state, QueueState::Editing);
        assert_eq!(client.snapshot().queue[0].revision, lease.revision);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(client.snapshot().active.is_none());
        client
            .command(Command::FinishPendingEdit {
                id: lease.id,
                revision: lease.revision,
                text: "edited first".into(),
                expected_paused: !queued,
            })
            .await
            .unwrap();
        wait(&client, |s| s.active.is_some()).await;
        let Reply::History(history) = client
            .command(Command::History {
                before: None,
                limit: 20,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(history[2].text, "edited first");
        client.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_stop_replay_after_reopen_returns_committed_reply_without_changing_recovery_pause() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    assert_stopped(
        store
            .apply("idle", &stop(None, false), &config, None)
            .unwrap(),
        true,
        false,
    );
    store
        .apply(
            "send",
            &Command::Send {
                text: "pending".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let command = stop(None, true);
    assert_stopped(
        store.apply("enable", &command, &config, None).unwrap(),
        false,
        false,
    );
    drop(store);
    let client = Client::start(&path, config).await.unwrap();
    let before = client.snapshot();
    assert!(before.paused);
    for _ in 0..2 {
        assert_stopped(
            client
                .command_with_id("enable".into(), command.clone())
                .await
                .unwrap(),
            false,
            false,
        );
    }
    assert!(client.snapshot().paused);
    assert!(client.snapshot().active.is_none());
    assert_eq!(client.snapshot().cursor, before.cursor);
    assert_eq!(
        serde_json::to_value(client.snapshot().queue).unwrap(),
        serde_json::to_value(before.queue).unwrap()
    );
    assert!(
        client
            .command_with_id("enable".into(), stop(None, false))
            .await
            .is_err()
    );
    client.shutdown().await.unwrap();
}

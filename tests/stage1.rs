use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, HistoryEntry, JournalEvent, Phase, QueueState, Reply, Snapshot},
};
use rusqlite::Connection;
use std::time::Duration;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

fn fixture(scenario: Scenario) -> Config {
    let mut config = Config::default();
    config.provider.scenario = scenario;
    config.provider.first_event_ms = 0;
    config.provider.delay_ms = 0;
    config.limits.checkpoint_ms = 10;
    config
}

async fn request(client: &Client, command: Command) -> anyhow::Result<Reply> {
    timeout(DEADLINE, client.command(command))
        .await
        .expect("command timed out")
}

async fn ack(client: &Client, command: Command) {
    assert!(matches!(
        request(client, command).await.unwrap(),
        Reply::Ack
    ));
}

async fn shutdown(client: &Client) {
    timeout(DEADLINE, client.shutdown())
        .await
        .expect("shutdown did not join the actor")
        .unwrap();
}

async fn wait_for(client: &Client, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let mut snapshots = client.subscribe();
    timeout(DEADLINE, async {
        loop {
            // Own the snapshot so a watch read lock never spans an await.
            let snapshot = snapshots.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            snapshots
                .changed()
                .await
                .expect("actor stopped while waiting");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("snapshot condition timed out: {:?}", client.snapshot()))
}

async fn assert_held(client: &Client, predicate: impl Fn(&Snapshot) -> bool) {
    let mut snapshots = client.subscribe();
    let result = timeout(Duration::from_millis(100), async {
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            assert!(predicate(&snapshot), "unexpected progress: {snapshot:?}");
            snapshots.changed().await.expect("actor stopped while held");
        }
    })
    .await;
    assert!(result.is_err());
    assert!(predicate(&client.snapshot()));
}

async fn idle(client: &Client) -> Snapshot {
    wait_for(client, |snapshot| {
        snapshot.phase == Phase::Idle && snapshot.active.is_none() && snapshot.queue.is_empty()
    })
    .await
}

async fn history(client: &Client) -> Vec<HistoryEntry> {
    match request(
        client,
        Command::History {
            before: None,
            limit: 200,
        },
    )
    .await
    .unwrap()
    {
        Reply::History(entries) => entries,
        reply => panic!("expected history, got {reply:?}"),
    }
}

async fn events(client: &Client) -> Vec<JournalEvent> {
    match request(
        client,
        Command::Events {
            after: 0,
            limit: 200,
        },
    )
    .await
    .unwrap()
    {
        Reply::Events(entries) => entries,
        reply => panic!("expected events, got {reply:?}"),
    }
}

fn send(text: &str) -> Command {
    Command::Send { text: text.into() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_queue_reopens_paused_and_shutdown_releases_writer_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = fixture(Scenario::Normal);
    let client = Client::start(&path, config.clone()).await.unwrap();
    let initial = client.snapshot();
    assert!(!initial.paused);
    assert!(!initial.recovered);
    assert!(Client::start(&path, config.clone()).await.is_err());
    ack(&client, Command::Pause).await;
    ack(&client, send("saved first")).await;
    ack(&client, send("saved second")).await;
    let saved = client.snapshot();
    assert_eq!(saved.queue.len(), 2);
    let clone = client.clone();
    let receiver = client.subscribe();
    shutdown(&client).await;

    // Live client clones and snapshot receivers must not retain the writer lock.
    let reopened = Client::start(&path, config).await.unwrap();
    let snapshot = reopened.snapshot();
    assert_eq!(snapshot.session_id, initial.session_id);
    assert_eq!(snapshot.agent_id, initial.agent_id);
    assert_eq!(snapshot.conversation_id, initial.conversation_id);
    assert!(snapshot.recovered);
    assert!(snapshot.paused);
    assert_eq!(snapshot.queue.len(), saved.queue.len());
    for (actual, expected) in snapshot.queue.iter().zip(&saved.queue) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.revision, expected.revision);
        assert_eq!(actual.state, QueueState::Queued);
    }
    assert_held(&reopened, |s| {
        s.paused && s.active.is_none() && s.queue.len() == 2
    })
    .await;
    assert!(history(&reopened).await.is_empty());
    assert!(
        !events(&reopened)
            .await
            .iter()
            .any(|event| event.kind == "attempt_started")
    );
    shutdown(&clone).await;
    drop(receiver);
    shutdown(&reopened).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_completion_has_durable_history_and_correlated_ids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = fixture(Scenario::Normal);
    let client = Client::start(&path, config.clone()).await.unwrap();
    ack(&client, Command::Pause).await;
    ack(&client, send("normal prompt")).await;
    let queued = client.snapshot();
    let message_id = queued.queue[0].id.clone();
    for id in [
        &queued.session_id,
        &queued.agent_id,
        &queued.conversation_id,
        &message_id,
    ] {
        uuid::Uuid::parse_str(id).unwrap();
    }
    assert_ne!(queued.session_id, queued.agent_id);
    assert_ne!(queued.agent_id, queued.conversation_id);
    ack(&client, Command::Resume).await;
    idle(&client).await;
    let entries = history(&client).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, "user");
    assert_eq!(entries[0].text, "normal prompt");
    assert_eq!(entries[0].status, "accepted");
    assert_eq!(entries[1].kind, "assistant");
    assert_eq!(entries[1].status, "completed");
    assert!(entries[1].text.starts_with("[test provider]"));
    assert!(entries[0].seq < entries[1].seq);
    let journal = events(&client).await;
    let started: Vec<_> = journal
        .iter()
        .filter(|event| event.kind == "attempt_started")
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].payload, message_id);
    let attempt_id = started[0].attempt_id.as_ref().unwrap();
    uuid::Uuid::parse_str(attempt_id).unwrap();
    assert_ne!(attempt_id, &message_id);
    let finished: Vec<_> = journal
        .iter()
        .filter(|event| event.kind == "attempt_finished")
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].attempt_id.as_ref(), Some(attempt_id));
    assert_eq!(finished[0].payload, "completed");
    let Reply::History(page) = request(
        &client,
        Command::History {
            before: Some(entries[1].seq),
            limit: 1,
        },
    )
    .await
    .unwrap() else {
        panic!("expected history page")
    };
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].seq, entries[0].seq);
    shutdown(&client).await;
    let reopened = Client::start(&path, config).await.unwrap();
    let restored = history(&reopened).await;
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(entries).unwrap()
    );
    shutdown(&reopened).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_lease_blocks_the_entire_queue_until_finished_or_cancelled() {
    for finish in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let client = Client::start(&path, fixture(Scenario::Normal))
            .await
            .unwrap();
        ack(&client, Command::Pause).await;
        ack(&client, send("first queued")).await;
        ack(&client, send("original second")).await;
        let queue = client.snapshot().queue;
        let item = &queue[1];
        let Reply::Editing(lease) = request(
            &client,
            Command::BeginEdit {
                id: item.id.clone(),
                revision: item.revision,
            },
        )
        .await
        .unwrap() else {
            panic!("expected edit lease")
        };
        assert_eq!(lease.id, item.id);
        assert_eq!(lease.text, item.text);
        assert_eq!(lease.state, QueueState::Editing);
        assert!(lease.revision > item.revision);
        assert!(
            request(
                &client,
                Command::BeginEdit {
                    id: queue[0].id.clone(),
                    revision: queue[0].revision,
                }
            )
            .await
            .is_err()
        );
        assert!(
            request(
                &client,
                Command::FinishEdit {
                    id: item.id.clone(),
                    revision: item.revision,
                    text: "stale".into(),
                }
            )
            .await
            .is_err()
        );
        assert!(
            request(
                &client,
                Command::CancelEdit {
                    id: item.id.clone(),
                    revision: item.revision,
                }
            )
            .await
            .is_err()
        );
        ack(&client, Command::Resume).await;
        assert_held(&client, |s| {
            !s.paused
                && s.active.is_none()
                && s.queue.len() == 2
                && s.queue[0].state == QueueState::Queued
                && s.queue[1].state == QueueState::Editing
        })
        .await;
        assert!(history(&client).await.is_empty());
        if finish {
            ack(
                &client,
                Command::FinishEdit {
                    id: lease.id,
                    revision: lease.revision,
                    text: "edited second".into(),
                },
            )
            .await;
        } else {
            ack(
                &client,
                Command::CancelEdit {
                    id: lease.id,
                    revision: lease.revision,
                },
            )
            .await;
        }
        idle(&client).await;
        let entries = history(&client).await;
        let prompts: Vec<_> = entries
            .iter()
            .filter(|entry| entry.kind == "user")
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(
            prompts,
            [
                "first queued",
                if finish {
                    "edited second"
                } else {
                    "original second"
                }
            ]
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.status == "completed")
                .count(),
            2
        );
        shutdown(&client).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_revision_conflicts_do_not_mutate_and_cancel_removes_only_selected_item() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, fixture(Scenario::Normal))
        .await
        .unwrap();
    ack(&client, Command::Pause).await;
    ack(&client, send("cancel me")).await;
    ack(&client, send("keep me")).await;
    let original = client.snapshot().queue[0].clone();
    assert!(
        request(
            &client,
            Command::BeginEdit {
                id: original.id.clone(),
                revision: original.revision + 1,
            }
        )
        .await
        .is_err()
    );
    let Reply::Editing(lease) = request(
        &client,
        Command::BeginEdit {
            id: original.id.clone(),
            revision: original.revision,
        },
    )
    .await
    .unwrap() else {
        panic!("expected edit lease")
    };
    ack(
        &client,
        Command::FinishEdit {
            id: lease.id,
            revision: lease.revision,
            text: "updated cancelled prompt".into(),
        },
    )
    .await;
    let edited = client.snapshot().queue[0].clone();
    assert!(edited.revision > original.revision);
    let cursor = client.snapshot().cursor;
    assert!(
        request(
            &client,
            Command::Cancel {
                id: original.id.clone(),
                revision: original.revision,
            }
        )
        .await
        .is_err()
    );
    assert!(
        request(
            &client,
            Command::BeginEdit {
                id: original.id.clone(),
                revision: original.revision,
            }
        )
        .await
        .is_err()
    );
    assert_eq!(client.snapshot().cursor, cursor);
    assert_eq!(client.snapshot().queue[0].text, edited.text);
    assert_eq!(client.snapshot().queue[0].revision, edited.revision);
    ack(
        &client,
        Command::Cancel {
            id: edited.id.clone(),
            revision: edited.revision,
        },
    )
    .await;
    assert_eq!(client.snapshot().queue.len(), 1);
    assert_eq!(client.snapshot().queue[0].text, "keep me");
    assert!(
        request(
            &client,
            Command::Cancel {
                id: edited.id,
                revision: edited.revision
            }
        )
        .await
        .is_err()
    );
    ack(&client, Command::Resume).await;
    idle(&client).await;
    let entries = history(&client).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].text, "keep me");
    shutdown(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_saves_partial_output_and_stale_attempt_cannot_cancel_the_next() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = fixture(Scenario::Normal);
    // The first delta is immediate; later deltas cannot race the interrupt assertions.
    config.provider.delay_ms = 60_000;
    let client = Client::start(&path, config).await.unwrap();
    ack(&client, Command::Pause).await;
    ack(&client, send("interrupt first")).await;
    ack(&client, send("next attempt")).await;
    ack(&client, Command::Resume).await;
    let first = wait_for(&client, |s| {
        s.active.as_ref().is_some_and(|a| !a.text.is_empty())
    })
    .await;
    let first = first.active.unwrap();
    let interrupt = Command::Interrupt {
        attempt_id: first.attempt_id.clone(),
    };
    assert!(matches!(
        client
            .command_with_id("interrupt-first".into(), interrupt.clone())
            .await
            .unwrap(),
        Reply::Ack
    ));
    let next = wait_for(&client, |s| {
        s.active
            .as_ref()
            .is_some_and(|a| a.attempt_id != first.attempt_id && !a.text.is_empty())
    })
    .await
    .active
    .unwrap();
    assert!(request(&client, interrupt.clone()).await.is_err());
    assert!(matches!(
        client
            .command_with_id("interrupt-first".into(), interrupt)
            .await
            .unwrap(),
        Reply::Ack
    ));
    assert_eq!(
        client.snapshot().active.unwrap().attempt_id,
        next.attempt_id
    );
    let entries = history(&client).await;
    let partial = entries
        .iter()
        .find(|entry| entry.kind == "assistant")
        .unwrap();
    assert_eq!(partial.status, "cancelled");
    assert_eq!(partial.text, first.text);
    let journal = events(&client).await;
    let interrupts: Vec<_> = journal
        .iter()
        .filter(|event| event.kind == "interrupt_acknowledged")
        .collect();
    assert_eq!(interrupts.len(), 1);
    assert_eq!(interrupts[0].attempt_id.as_ref(), Some(&first.attempt_id));
    ack(
        &client,
        Command::Interrupt {
            attempt_id: next.attempt_id,
        },
    )
    .await;
    idle(&client).await;
    shutdown(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_id_is_durable_and_rejects_changed_payload_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = fixture(Scenario::Normal);
    let client = Client::start(&path, config.clone()).await.unwrap();
    ack(&client, Command::Pause).await;
    let command = send("exactly once");
    let command_id = "durable-send".to_owned();
    assert!(matches!(
        client
            .command_with_id(command_id.clone(), command.clone())
            .await
            .unwrap(),
        Reply::Ack
    ));
    let saved = client.snapshot();
    assert!(matches!(
        client
            .command_with_id(command_id.clone(), command.clone())
            .await
            .unwrap(),
        Reply::Ack
    ));
    assert!(
        client
            .command_with_id(command_id.clone(), send("different payload"))
            .await
            .is_err()
    );
    assert_eq!(client.snapshot().queue.len(), 1);
    assert_eq!(client.snapshot().cursor, saved.cursor);
    shutdown(&client).await;
    let reopened = Client::start(&path, config).await.unwrap();
    let cursor = reopened.snapshot().cursor;
    assert!(matches!(
        reopened
            .command_with_id(command_id.clone(), command)
            .await
            .unwrap(),
        Reply::Ack
    ));
    assert!(
        reopened
            .command_with_id(command_id, send("different payload"))
            .await
            .is_err()
    );
    let snapshot = reopened.snapshot();
    assert_eq!(snapshot.queue.len(), 1);
    assert_eq!(snapshot.queue[0].id, saved.queue[0].id);
    assert_eq!(snapshot.queue[0].text, "exactly once");
    assert_eq!(snapshot.cursor, cursor);
    assert_eq!(
        events(&reopened)
            .await
            .iter()
            .filter(|event| event.kind == "message_queued")
            .count(),
        1
    );
    ack(&reopened, Command::Resume).await;
    idle(&reopened).await;
    assert_eq!(history(&reopened).await.len(), 2);
    shutdown(&reopened).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_and_dropped_snapshot_receivers_do_not_block_commands_or_completion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = fixture(Scenario::Burst);
    config.provider.chunks = 128;
    let client = Client::start(&path, config).await.unwrap();
    let unread = client.subscribe();
    let dropped = client.subscribe();
    drop(dropped);
    ack(&client, Command::Pause).await;
    for prompt in ["one", "two", "three"] {
        ack(&client, send(prompt)).await;
    }
    ack(&client, Command::Resume).await;
    let completed = idle(&client).await;
    let entries = history(&client).await;
    assert_eq!(entries.len(), 6);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.status == "completed")
            .count(),
        3
    );
    assert_eq!(unread.borrow().cursor, completed.cursor);
    drop(unread);
    ack(&client, send("after subscribers dropped")).await;
    idle(&client).await;
    assert_eq!(history(&client).await.len(), 8);
    shutdown(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_response_is_truncated_at_the_byte_cap_and_stays_durable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = fixture(Scenario::Burst);
    config.provider.chunks = 10_000;
    config.limits.response_bytes = 67;
    let client = Client::start(&path, config.clone()).await.unwrap();
    ack(&client, send("bounded burst")).await;
    idle(&client).await;
    let entries = history(&client).await;
    assert_eq!(entries.len(), 2);
    let response = &entries[1];
    assert_eq!(response.kind, "assistant");
    assert_eq!(response.status, "truncated");
    assert_eq!(response.text.len(), config.limits.response_bytes);
    assert!(response.text.starts_with("fixture block"));
    assert!(
        events(&client)
            .await
            .iter()
            .any(|event| event.kind == "attempt_finished" && event.payload == "truncated")
    );
    shutdown(&client).await;
    let reopened = Client::start(&path, config).await.unwrap();
    let restored = history(&reopened).await;
    assert_eq!(restored[1].seq, response.seq);
    assert_eq!(restored[1].text, response.text);
    assert_eq!(restored[1].status, "truncated");
    shutdown(&reopened).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_error_and_stream_interruption_save_terminal_results() {
    for scenario in [Scenario::Error, Scenario::Interrupted] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = fixture(scenario);
        let client = Client::start(&path, config.clone()).await.unwrap();
        ack(&client, send("failing fixture")).await;
        let snapshot = idle(&client).await;
        assert!(
            snapshot
                .error
                .as_ref()
                .is_some_and(|error| error.contains("fixture:"))
        );
        let entries = history(&client).await;
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].text, "failing fixture");
        assert_eq!(entries[1].kind, "assistant");
        assert_eq!(entries[1].status, "failed");
        assert_eq!(entries[2].kind, "notice");
        assert_eq!(entries[2].status, "error");
        assert_eq!(entries[2].attempt_id, entries[1].attempt_id);
        assert_eq!(
            entries[2].text,
            format!("error: {}", snapshot.error.unwrap())
        );
        if scenario == Scenario::Error {
            assert!(entries[1].text.is_empty());
        } else {
            assert!(entries[1].text.starts_with("[test provider]"));
            assert!(!entries[1].text.is_empty());
        }
        shutdown(&client).await;
        let reopened = Client::start(&path, config).await.unwrap();
        assert_eq!(
            serde_json::to_value(history(&reopened).await).unwrap(),
            serde_json::to_value(entries).unwrap()
        );
        let Reply::History(page) = request(
            &reopened,
            Command::Transcript {
                before: None,
                after: None,
                limit: 1,
            },
        )
        .await
        .unwrap() else {
            panic!("expected error notice page")
        };
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].kind, "notice");
        assert_eq!(page[0].status, "error");
        assert!(page[0].text.starts_with("error: fixture:"));
        shutdown(&reopened).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_insert_failure_returns_no_ack_and_pauses_until_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = fixture(Scenario::Normal);
    let client = Client::start(&path, config.clone()).await.unwrap();
    assert!(!client.snapshot().paused);
    let connection = Connection::open(&path).unwrap();
    connection.busy_timeout(DEADLINE).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_message_insert BEFORE INSERT ON messages
         BEGIN SELECT RAISE(ABORT, 'stage1 injected write failure'); END;",
        )
        .unwrap();
    let command_id = "failed-send".to_owned();
    let result = timeout(
        DEADLINE,
        client.command_with_id(command_id.clone(), send("not acknowledged")),
    )
    .await
    .expect("faulted command timed out");
    assert!(result.is_err(), "failed persistence must not ACK");
    let snapshot = wait_for(&client, |s| s.phase == Phase::StorageError).await;
    assert!(snapshot.paused);
    assert!(snapshot.active.is_none());
    assert!(snapshot.queue.is_empty());
    assert!(
        snapshot
            .error
            .as_ref()
            .is_some_and(|error| error.contains("stage1 injected write failure"))
    );
    let messages: i64 = connection
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    let saved_results: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM command_results WHERE id=?1",
            [&command_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(messages, 0);
    assert_eq!(saved_results, 0);
    assert!(
        !events(&client)
            .await
            .iter()
            .any(|event| event.kind == "message_queued")
    );
    assert!(request(&client, Command::Resume).await.is_err());
    connection
        .execute_batch("DROP TRIGGER fail_message_insert")
        .unwrap();
    assert!(
        request(&client, send("still paused after repair"))
            .await
            .is_err()
    );
    assert_eq!(client.snapshot().phase, Phase::StorageError);
    shutdown(&client).await;
    let reopened = Client::start(&path, config).await.unwrap();
    assert!(reopened.snapshot().paused);
    assert!(reopened.snapshot().queue.is_empty());
    assert!(matches!(
        reopened
            .command_with_id(command_id, send("not acknowledged"))
            .await
            .unwrap(),
        Reply::Ack
    ));
    assert_eq!(reopened.snapshot().queue.len(), 1);
    ack(&reopened, Command::Resume).await;
    idle(&reopened).await;
    assert_eq!(history(&reopened).await.len(), 2);
    shutdown(&reopened).await;
}

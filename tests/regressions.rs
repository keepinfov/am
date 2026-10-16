use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, Phase, Reply, Snapshot},
};
use rusqlite::Connection;
use std::{path::Path, time::Duration};
use tokio::time::{sleep, timeout};

fn waiting() -> Config {
    let mut config = Config::default();
    config.provider.scenario = Scenario::Waiting;
    config.provider.first_event_ms = 5000;
    config
}

async fn wait(client: &Client, predicate: impl Fn(&Snapshot) -> bool) {
    let mut updates = client.subscribe();
    timeout(Duration::from_secs(10), async {
        loop {
            if predicate(&client.snapshot()) {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

async fn send(client: &Client) {
    client
        .command(Command::Send {
            text: "fixture".into(),
        })
        .await
        .unwrap();
    wait(client, |s| s.active.is_some()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_and_replayed_shutdown_do_not_terminate_a_new_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = waiting();
    let client = Client::start(&path, config.clone()).await.unwrap();
    client
        .command_with_id(
            "used".into(),
            Command::Send {
                text: "fixture".into(),
            },
        )
        .await
        .unwrap();
    wait(&client, |s| s.active.is_some()).await;
    assert!(
        client
            .command_with_id("used".into(), Command::Shutdown)
            .await
            .is_err()
    );
    assert!(client.snapshot().active.is_some());
    assert!(matches!(
        client
            .command(Command::History {
                before: None,
                limit: 10
            })
            .await
            .unwrap(),
        Reply::History(_)
    ));
    client
        .command_with_id("old-shutdown".into(), Command::Shutdown)
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Resume).await.unwrap();
    send(&client).await;
    let attempt = client.snapshot().active.unwrap().attempt_id;
    client
        .command_with_id("old-shutdown".into(), Command::Shutdown)
        .await
        .unwrap();
    assert_eq!(client.snapshot().active.unwrap().attempt_id, attempt);
    client.command(Command::Pause).await.unwrap();
    client.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_link_cannot_bypass_the_writer_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let alias = directory.path().join("alias.sqlite3");
    let client = Client::start(&path, waiting()).await.unwrap();
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(Client::start(&alias, waiting()).await.is_err());
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_fault_is_published_even_without_a_mutating_request() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, waiting()).await.unwrap();
    send(&client).await;
    let database = Connection::open(&path).unwrap();
    database
        .execute_batch("ALTER TABLE history RENAME TO hidden_history")
        .unwrap();
    assert!(
        client
            .command(Command::History {
                before: None,
                limit: 10
            })
            .await
            .is_err()
    );
    wait(&client, |s| s.phase == Phase::StorageError).await;
    assert!(client.snapshot().paused);
    database
        .execute_batch("ALTER TABLE hidden_history RENAME TO history")
        .unwrap();
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_shutdown_keeps_the_join_handle_and_lock_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, waiting()).await.unwrap();
    send(&client).await;
    let database = Connection::open(&path).unwrap();
    database.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(
        timeout(Duration::from_millis(40), client.shutdown())
            .await
            .is_err()
    );
    database.execute_batch("ROLLBACK").unwrap();
    timeout(Duration::from_secs(5), client.shutdown())
        .await
        .unwrap()
        .unwrap();
    let reopened = Client::start(&path, waiting()).await.unwrap();
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_commands_are_rejected_before_waiting_for_a_busy_actor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, waiting()).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    let database = Connection::open(&path).unwrap();
    database.execute_batch("BEGIN IMMEDIATE").unwrap();
    let busy = client.clone();
    let mutation = tokio::spawn(async move { busy.command(Command::Resume).await });
    sleep(Duration::from_millis(20)).await;
    let result = timeout(
        Duration::from_millis(100),
        client.command(Command::Send {
            text: "x".repeat(32 * 1024),
        }),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    let result = timeout(
        Duration::from_millis(100),
        client.command(Command::Interrupt {
            attempt_id: "x".repeat(1024),
        }),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    database.execute_batch("ROLLBACK").unwrap();
    mutation.await.unwrap().unwrap();
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_wal_stops_writes_instead_of_growing_without_a_budget() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let client = Client::start(&path, waiting()).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    let database = Connection::open(&path).unwrap();
    database
        .execute_batch("BEGIN; SELECT * FROM session;")
        .unwrap();
    let mut failed = false;
    for _ in 0..1000 {
        if client.command(Command::Pause).await.is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "reader-pinned WAL must stop new writes");
    wait(&client, |s| s.phase == Phase::StorageError).await;
    let bytes = std::fs::metadata(path.with_file_name("session.sqlite3-wal"))
        .unwrap()
        .len();
    assert!(bytes < 2 * 1024 * 1024, "unexpected WAL size: {bytes}");
    database.execute_batch("ROLLBACK").unwrap();
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_page_quota_fails_without_acknowledging_a_lost_message() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = waiting();
    config.limits.storage_bytes = 1024 * 1024;
    config.limits.queue_messages = 100;
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    let mut accepted = 0;
    loop {
        if client
            .command(Command::Send {
                text: "x".repeat(16 * 1024),
            })
            .await
            .is_err()
        {
            break;
        }
        accepted += 1;
        assert!(accepted < 100, "SQLite page quota was ignored");
    }
    wait(&client, |s| s.phase == Phase::StorageError).await;
    let database = Connection::open(&path).unwrap();
    let stored: i64 = database
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, accepted);
    // The quota cannot accommodate another shutdown receipt. Dropping all clients
    // must nevertheless release the actor and lock, without claiming a saved exit.
    drop(client);
    sleep(Duration::from_millis(100)).await;
}

async fn database_wait(path: &Path, query: &str) {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(db) = Connection::open(path)
                && db
                    .query_row::<i64, _, _>(query, [], |r| r.get(0))
                    .unwrap_or(0)
                    > 0
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_checkpoint_and_completion_stop_the_producer_without_a_false_result() {
    for checkpoint_ms in [10, 5000] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.first_event_ms = 100;
        config.provider.delay_ms = 1;
        config.limits.checkpoint_ms = checkpoint_ms;
        let client = Client::start(&path, config.clone()).await.unwrap();
        send(&client).await;
        let database = Connection::open(&path).unwrap();
        database.execute_batch("CREATE TRIGGER fail_response BEFORE UPDATE ON history BEGIN SELECT RAISE(ABORT,'injected response failure'); END;").unwrap();
        wait(&client, |s| s.phase == Phase::StorageError).await;
        assert!(client.snapshot().paused);
        assert_eq!(
            database
                .query_row::<String, _, _>("SELECT status FROM attempts", [], |r| r.get(0))
                .unwrap(),
            "running"
        );
        assert!(!client.snapshot().active.unwrap().text.is_empty());
        database
            .execute_batch("DROP TRIGGER fail_response")
            .unwrap();
        client.shutdown().await.unwrap();
    }
}

#[test]
fn unfinished_edit_recovers_original_text_and_invalidates_the_lease_revision() {
    use am::{runtime::QueueState, storage::Store};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    store
        .apply(
            "send",
            &Command::Send {
                text: "original".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let item = store.queue(10).unwrap().pop().unwrap();
    let Reply::Editing(lease) = store
        .apply(
            "edit",
            &Command::BeginEdit {
                id: item.id.clone(),
                revision: item.revision,
            },
            &config,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    drop(store); // No graceful-shutdown lease cleanup: exercise recovery instead.
    let mut store = Store::open(&path, &config).unwrap();
    assert!(store.paused().unwrap());
    let recovered = store.queue(10).unwrap().pop().unwrap();
    assert_eq!(recovered.text, "original");
    assert_eq!(recovered.state, QueueState::Queued);
    assert!(recovered.revision > lease.revision);
    assert!(
        store
            .apply(
                "stale",
                &Command::FinishEdit {
                    id: item.id,
                    revision: lease.revision,
                    text: "unfinished".into()
                },
                &config,
                None
            )
            .is_err()
    );
    assert!(store.start_next(&config).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_process_recovers_partial_history_without_replaying_the_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config_path = directory.path().join("fixture.toml");
    let mut config = Config::default();
    config.provider.delay_ms = 1000;
    config.provider.first_event_ms = 0;
    config.limits.checkpoint_ms = 10;
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        std::process::Command::new(env!("CARGO_BIN_EXE_am"))
            .args(["--headless", "--prompt", "crash fixture", "--session"])
            .arg(&path)
            .arg("--config")
            .arg(&config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    database_wait(
        &path,
        "SELECT COUNT(*) FROM history WHERE kind='assistant' AND length(text)>0",
    )
    .await;
    assert!(
        Client::start(&path, config.clone()).await.is_err(),
        "live subprocess writer lock must reject another client"
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let client = Client::start(&path, config).await.unwrap();
    assert!(client.snapshot().paused);
    assert!(client.snapshot().active.is_none());
    assert!(client.snapshot().queue.is_empty());
    let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 10,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].status, "interrupted");
    assert!(!history[1].text.is_empty());
    sleep(Duration::from_millis(100)).await;
    let database = Connection::open(&path).unwrap();
    assert_eq!(
        database
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM attempts", [], |r| r.get(0))
            .unwrap(),
        1
    );
    client.shutdown().await.unwrap();
}

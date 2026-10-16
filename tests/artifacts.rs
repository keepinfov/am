use am::{
    artifacts::{ArtifactRecord, MAX_READ_BYTES},
    config::{Config, Scenario},
    runtime::{Client, Command, NoticeLevel, Reply},
    storage::{Completion, Store},
};
use std::time::Duration;
use tokio::time::{sleep, timeout};

#[test]
fn explicit_action_categories_group_semantically_and_replay_without_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let first = Command::RecordAction {
        text: "saved".into(),
        level: NoticeLevel::Info,
        category: "settings".into(),
    };
    let Reply::Notice(saved) = store.apply("settings-1", &first, &config, None).unwrap() else {
        panic!()
    };
    for (receipt, category, level) in [
        ("settings-2", "settings", NoticeLevel::Status),
        ("queue", "queue", NoticeLevel::Info),
        ("drafts", "drafts", NoticeLevel::Info),
        ("bindings", "bindings", NoticeLevel::Info),
        ("delivery", "delivery", NoticeLevel::Status),
        ("error", "error", NoticeLevel::Error),
    ] {
        store
            .apply(
                receipt,
                &Command::RecordAction {
                    text: "saved".into(),
                    level,
                    category: category.into(),
                },
                &config,
                None,
            )
            .unwrap();
    }
    let rows = store.actions(None, 200).unwrap();
    assert_eq!(
        rows.iter().map(|r| r.category.as_str()).collect::<Vec<_>>(),
        [
            "settings", "settings", "queue", "drafts", "bindings", "delivery", "error"
        ]
    );
    assert_eq!(rows[0].group_id, rows[1].group_id);
    assert_eq!(rows[0].status, "info");
    assert_eq!(rows[1].status, "status");
    for pair in rows[1..].windows(2) {
        assert_ne!(pair[0].group_id, pair[1].group_id);
    }
    assert_eq!(store.history(None, 200).unwrap()[0], saved);
    let cursor = store.cursor().unwrap();
    assert!(
        matches!(store.apply("settings-1", &first, &config, None).unwrap(), Reply::Notice(row) if row==saved)
    );
    let changed = Command::RecordAction {
        text: "saved".into(),
        level: NoticeLevel::Info,
        category: "queue".into(),
    };
    assert!(store.apply("settings-1", &changed, &config, None).is_err());
    assert_eq!(store.cursor().unwrap(), cursor);
    assert_eq!(store.actions(None, 200).unwrap(), rows);
    drop(store);

    let mut reopened = Store::open(&path, &config).unwrap();
    let cursor = reopened.cursor().unwrap();
    reopened.apply("settings-1", &first, &config, None).unwrap();
    assert_eq!(reopened.cursor().unwrap(), cursor);
    assert_eq!(reopened.actions(None, 200).unwrap(), rows);
    reopened
        .apply(
            "error-2",
            &Command::RecordAction {
                text: "failed after reopen".into(),
                level: NoticeLevel::Error,
                category: "error".into(),
            },
            &config,
            None,
        )
        .unwrap();
    assert_eq!(
        reopened.actions(None, 1).unwrap()[0].group_id,
        rows.last().unwrap().group_id
    );
}

#[test]
fn explicit_action_notice_metadata_event_and_receipt_commit_atomically() {
    for table in ["history", "actions", "events", "command_results"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        let command = Command::RecordAction {
            text: "saved binding".into(),
            level: NoticeLevel::Info,
            category: "bindings".into(),
        };
        let db = rusqlite::Connection::open(&path).unwrap();
        let cursor = store.cursor().unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_action BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'action fault'); END;")).unwrap();
        assert!(store.apply("action", &command, &config, None).is_err());
        assert!(store.history(None, 200).unwrap().is_empty());
        assert!(store.actions(None, 200).unwrap().is_empty());
        assert!(store.previous("action", &command).unwrap().is_none());
        assert_eq!(store.cursor().unwrap(), cursor);
        db.execute_batch("DROP TRIGGER fail_action").unwrap();
        assert!(matches!(
            store.apply("action", &command, &config, None).unwrap(),
            Reply::Notice(_)
        ));
        assert_eq!(store.history(None, 200).unwrap().len(), 1);
        assert_eq!(store.actions(None, 200).unwrap().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_action_categories_and_text_reject_before_a_busy_actor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let client = Client::start(&path, config.clone()).await.unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let busy = client.clone();
    let pending = tokio::spawn(async move { busy.command(Command::Pause).await });
    sleep(Duration::from_millis(20)).await;
    for (text, category) in [
        ("saved", "info"),
        ("saved", "Settings"),
        ("saved", "tool"),
        ("saved", ""),
        ("bad\nnotice", "queue"),
        ("\x1b[31m", "error"),
        ("", "settings"),
    ] {
        let command = Command::RecordAction {
            text: text.into(),
            level: NoticeLevel::Info,
            category: category.into(),
        };
        assert!(
            timeout(Duration::from_millis(100), client.command(command))
                .await
                .unwrap()
                .is_err()
        );
    }
    assert!(
        timeout(
            Duration::from_millis(100),
            client.command(Command::RecordAction {
                text: "valid".into(),
                level: NoticeLevel::Info,
                category: "x".repeat(1024)
            })
        )
        .await
        .unwrap()
        .is_err()
    );
    db.execute_batch("ROLLBACK").unwrap();
    pending.await.unwrap().unwrap();
    client.shutdown().await.unwrap();
    let mut store = Store::open(&path, &config).unwrap();
    assert!(
        store
            .apply(
                "invalid",
                &Command::RecordAction {
                    text: "saved".into(),
                    level: NoticeLevel::Info,
                    category: "tool".into()
                },
                &config,
                None
            )
            .is_err()
    );
    assert!(store.actions(None, 200).unwrap().is_empty());
    assert!(store.history(None, 200).unwrap().is_empty());
}

fn start(store: &mut Store, config: &Config, receipt: &str) -> String {
    store
        .apply(
            receipt,
            &Command::Send {
                text: receipt.into(),
            },
            config,
            None,
        )
        .unwrap();
    store.start_next(config).unwrap().unwrap().attempt_id
}

fn output(store: &Store) -> ArtifactRecord {
    store
        .actions(None, 200)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|a| a.output)
        .unwrap()
}

#[test]
fn action_categories_groups_and_pagination_survive_reopen_without_phrase_parsing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    for (receipt, text, level) in [
        ("a", "tool executed shell", NoticeLevel::Info),
        ("b", "unrelated preference saved", NoticeLevel::Info),
        ("c", "tool executed shell", NoticeLevel::Error),
        ("d", "another info", NoticeLevel::Info),
    ] {
        store
            .apply(
                receipt,
                &Command::RecordNotice {
                    text: text.into(),
                    level,
                },
                &config,
                None,
            )
            .unwrap();
    }
    let rows = store.actions(None, 200).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].category, "info");
    assert_eq!(rows[0].group_id, rows[1].group_id);
    assert_eq!(rows[2].category, "error");
    assert_ne!(rows[1].group_id, rows[2].group_id);
    assert_ne!(rows[0].group_id, rows[3].group_id);
    assert!(rows.iter().all(|r| r.output.is_none()));
    assert_eq!(store.actions(Some(rows[3].seq), 2).unwrap(), rows[1..3]);
    assert!(store.actions(Some(rows[0].seq), 2).unwrap().is_empty());
    drop(store);
    let mut reopened = Store::open(&path, &config).unwrap();
    assert_eq!(reopened.actions(None, 200).unwrap(), rows);
    reopened
        .apply(
            "e",
            &Command::RecordNotice {
                text: "after recovery".into(),
                level: NoticeLevel::Info,
            },
            &config,
            None,
        )
        .unwrap();
    assert_eq!(
        reopened.actions(None, 1).unwrap()[0].group_id,
        rows[3].group_id
    );
}

#[test]
fn output_pages_are_bounded_utf8_safe_and_have_private_generated_paths() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "send");
    let text = "a\u{e9}\u{1f642}b\n\x1b[31m";
    store.append_output(&attempt, text, &config.limits).unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt,
            text,
            status: "completed",
        })
        .unwrap();
    let record = output(&store);
    assert_eq!(record.bytes, text.len() as u64);
    assert!(!record.incomplete);
    let cursor = store.cursor().unwrap();
    assert!(
        store
            .append_output(&attempt, "late output", &config.limits)
            .is_err()
    );
    assert!(store.checkpoint(&attempt, "late checkpoint").is_err());
    assert_eq!(output(&store), record);
    assert_eq!(store.cursor().unwrap(), cursor);
    uuid::Uuid::parse_str(&record.id).unwrap();
    let expected = path
        .with_file_name("session.sqlite3.artifacts")
        .join(format!("{}.txt", record.id));
    assert_eq!(std::path::Path::new(&record.path), expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&record.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(expected.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let mut offset = 0;
    let mut restored = String::new();
    loop {
        let chunk = store.read_artifact(&record.id, offset, 4).unwrap();
        assert!(chunk.text.len() <= 4);
        restored.push_str(&chunk.text);
        assert_eq!(chunk.next_offset, offset + chunk.text.len() as u64);
        offset = chunk.next_offset;
        if chunk.eof {
            break;
        }
    }
    // The artifact API returns data, not terminal output; rendering sanitizes it.
    assert_eq!(restored, text);
    assert!(store.read_artifact(&record.id, 2, 4).is_err());
    assert!(store.read_artifact(&record.id, 3, 1).is_err());
    assert!(
        store
            .read_artifact(&record.id, record.bytes + 1, 4)
            .is_err()
    );
    assert!(store.read_artifact("../session.sqlite3", 0, 4).is_err());
    assert!(
        store
            .read_artifact(&record.id, record.bytes, 0)
            .unwrap()
            .eof
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_read_commands_preserve_permissions_ctime_content_and_durable_state() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "read-only");
    let text = "retained \u{1f642} output";
    store.append_output(&attempt, text, &config.limits).unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt,
            text,
            status: "completed",
        })
        .unwrap();
    let expected = store.actions(None, 200).unwrap();
    let record = expected[0].output.as_ref().unwrap();
    drop(store);

    // Startup/recovery writes are outside the read-only command boundary.
    let client = Client::start(&path, config).await.unwrap();
    assert!(client.snapshot().active.is_none());
    assert!(client.snapshot().queue.is_empty());
    let db = rusqlite::Connection::open(&path).unwrap();
    let durable_state = || {
        [
            "SELECT * FROM events ORDER BY seq",
            "SELECT * FROM command_results ORDER BY id",
        ]
        .map(|sql| {
            db.prepare(sql)
                .unwrap()
                .query_map([], |row| {
                    (0..row.as_ref().column_count())
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        })
    };
    let saved = durable_state();
    let cursor = client.snapshot().cursor;
    std::fs::set_permissions(&record.path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let before = std::fs::metadata(&record.path).unwrap();
    assert_eq!(before.mode() & 0o777, 0o400);
    let bytes = std::fs::read(&record.path).unwrap();
    assert_eq!(bytes, text.as_bytes());

    for command in [
        Command::Actions {
            before: None,
            limit: 200,
        },
        Command::ActionsAfter {
            after: 0,
            limit: 200,
        },
        Command::ReadArtifact {
            id: record.id.clone(),
            offset: 0,
            limit: MAX_READ_BYTES,
        },
        Command::ReadArtifactBefore {
            id: record.id.clone(),
            before: record.bytes,
            limit: MAX_READ_BYTES,
        },
    ] {
        match client.command(command).await.unwrap() {
            Reply::Actions(rows) => assert_eq!(rows, expected),
            Reply::Artifact(chunk) | Reply::ArtifactPage { offset: 0, chunk } => {
                assert_eq!(chunk.text, text);
                assert_eq!(chunk.next_offset, record.bytes);
                assert!(chunk.eof);
                assert!(!chunk.incomplete);
            }
            reply => panic!("unexpected read reply: {reply:?}"),
        }
        let after = std::fs::metadata(&record.path).unwrap();
        assert_eq!(after.mode(), before.mode());
        assert_eq!(after.ctime(), before.ctime());
        assert_eq!(after.ctime_nsec(), before.ctime_nsec());
        assert_eq!(std::fs::read(&record.path).unwrap(), bytes);
        assert_eq!(durable_state(), saved);
        assert_eq!(client.snapshot().cursor, cursor);
    }
    client.shutdown().await.unwrap();
}

#[cfg(unix)]
#[test]
fn artifact_creation_is_private_and_explicit_append_hardens_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "write");
    let record = output(&store);
    assert_eq!(std::fs::metadata(&record.path).unwrap().len(), 0);
    assert_eq!(
        std::fs::metadata(&record.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::fs::set_permissions(&record.path, std::fs::Permissions::from_mode(0o644)).unwrap();
    store
        .append_output(&attempt, "explicit append", &config.limits)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&record.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(std::fs::read(&record.path).unwrap(), b"explicit append");
}

#[test]
fn output_and_session_quotas_mark_incomplete_without_deleting_results() {
    for session_quota in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.limits.output_bytes = 1024 * 1024;
        config.limits.artifact_bytes = if session_quota {
            1024 * 1024
        } else {
            2 * 1024 * 1024
        };
        config.validate().unwrap();
        let mut store = Store::open(&path, &config).unwrap();
        let block = "x".repeat(64 * 1024);
        let first = start(&mut store, &config, "first");
        for _ in 0..if session_quota { 10 } else { 20 } {
            store.append_output(&first, &block, &config.limits).unwrap();
        }
        store
            .finish(Completion {
                attempt_id: &first,
                text: "fixture",
                status: "completed",
            })
            .unwrap();
        let first_record = output(&store);
        assert_eq!(first_record.incomplete, !session_quota);
        if session_quota {
            let second = start(&mut store, &config, "second");
            for _ in 0..10 {
                store
                    .append_output(&second, &block, &config.limits)
                    .unwrap();
            }
            store
                .finish(Completion {
                    attempt_id: &second,
                    text: "fixture",
                    status: "completed",
                })
                .unwrap();
            let second_record = output(&store);
            assert!(second_record.incomplete);
            assert_eq!(
                first_record.bytes + second_record.bytes,
                config.limits.artifact_bytes
            );
            assert!(std::path::Path::new(&first_record.path).exists());
        } else {
            assert_eq!(first_record.bytes, config.limits.output_bytes as u64);
        }
        let latest = output(&store);
        assert!(
            store
                .read_artifact(&latest.id, 0, usize::MAX)
                .unwrap()
                .text
                .len()
                <= MAX_READ_BYTES
        );
        drop(store);
        let reopened = Store::open(&path, &config).unwrap();
        assert_eq!(output(&reopened), latest);
        assert!(std::path::Path::new(&first_record.path).exists());
    }
}

#[test]
fn interrupted_and_uncommitted_artifacts_are_recovered_with_authoritative_actions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "running");
    store
        .append_output(&attempt, "durable partial", &config.limits)
        .unwrap();
    store.checkpoint(&attempt, "durable partial").unwrap();
    let original = output(&store);
    drop(store);
    let mut store = Store::open(&path, &config).unwrap();
    assert!(store.paused().unwrap());
    assert_eq!(store.actions(None, 200).unwrap()[0].status, "interrupted");
    assert_eq!(output(&store).id, original.id);
    let chunk = store.read_artifact(&original.id, 0, 100).unwrap();
    assert_eq!(chunk.text, "durable partial");
    assert!(chunk.incomplete);
    store
        .apply("resume", &Command::Resume, &config, None)
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
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_action BEFORE INSERT ON actions BEGIN SELECT RAISE(ABORT,'action fault'); END;").unwrap();
    assert!(store.start_next(&config).is_err());
    db.execute_batch("DROP TRIGGER fail_action").unwrap();
    drop(store);
    let store = Store::open(&path, &config).unwrap();
    let rows = store.actions(None, 200).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].category, "recovery");
    assert_eq!(rows[1].status, "interrupted");
    assert!(rows[1].output.as_ref().unwrap().incomplete);
    assert_eq!(store.queue(10).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_completion_and_explicit_cancellation_register_result_artifacts() {
    for cancelled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.first_event_ms = 0;
        config.provider.delay_ms = 0;
        if cancelled {
            config.provider.scenario = Scenario::Waiting;
        }
        let client = Client::start(&path, config.clone()).await.unwrap();
        client
            .command(Command::Send {
                text: "fixture".into(),
            })
            .await
            .unwrap();
        let mut updates = client.subscribe();
        timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.borrow_and_update().clone();
                if if cancelled {
                    snapshot.active.is_some()
                } else {
                    snapshot.active.is_none() && snapshot.queue.is_empty()
                } {
                    break;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        if cancelled {
            client
                .command(Command::Interrupt {
                    attempt_id: client.snapshot().active.unwrap().attempt_id,
                })
                .await
                .unwrap();
        }
        let Reply::Actions(rows) = client
            .command(Command::Actions {
                before: None,
                limit: 200,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let record = rows[0].output.clone().unwrap();
        assert_eq!(
            rows[0].status,
            if cancelled { "cancelled" } else { "completed" }
        );
        assert_eq!(record.incomplete, cancelled);
        let Reply::Artifact(chunk) = client
            .command(Command::ReadArtifact {
                id: record.id.clone(),
                offset: 0,
                limit: usize::MAX,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(chunk.eof);
        assert_eq!(chunk.incomplete, cancelled);
        if !cancelled {
            assert!(chunk.text.starts_with("[test provider]"));
        }
        client.shutdown().await.unwrap();
        let reopened = Client::start(&path, config).await.unwrap();
        assert!(
            matches!(reopened.command(Command::Actions { before:None,limit:200 }).await.unwrap(),Reply::Actions(restored) if restored==rows)
        );
        reopened.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
#[test]
fn symlinked_artifact_files_and_directories_are_rejected() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "send");
    store
        .finish(Completion {
            attempt_id: &attempt,
            text: "result",
            status: "completed",
        })
        .unwrap();
    let record = output(&store);
    let moved = directory.path().join("moved.txt");
    std::fs::rename(&record.path, &moved).unwrap();
    symlink(&moved, &record.path).unwrap();
    assert!(store.read_artifact(&record.id, 0, 100).is_err());
    drop(store);
    assert!(Store::open(&path, &config).is_err());
    let other = directory.path().join("other.sqlite3");
    symlink(
        directory.path(),
        directory.path().join("other.sqlite3.artifacts"),
    )
    .unwrap();
    assert!(Store::open(&other, &config).is_err());
}

#[test]
fn quota_truncation_keeps_a_contiguous_prefix_and_rejects_new_dispatch_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.output_bytes = 1024 * 1024;
    config.limits.artifact_bytes = 1024 * 1024;
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "first");
    let prefix = "x".repeat(config.limits.output_bytes - 1);
    store
        .append_output(&attempt, &prefix, &config.limits)
        .unwrap();
    store
        .append_output(&attempt, "\u{e9}", &config.limits)
        .unwrap();
    store.append_output(&attempt, "z", &config.limits).unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt,
            text: "fixture",
            status: "completed",
        })
        .unwrap();
    let first = output(&store);
    assert_eq!(first.bytes, prefix.len() as u64);
    assert!(first.incomplete);
    assert_eq!(
        store
            .read_artifact(&first.id, first.bytes - 1, 100)
            .unwrap()
            .text,
        "x"
    );
    let second = start(&mut store, &config, "second");
    store.append_output(&second, "z", &config.limits).unwrap();
    store
        .finish(Completion {
            attempt_id: &second,
            text: "z",
            status: "completed",
        })
        .unwrap();
    assert_eq!(output(&store).bytes, 1);
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    let Reply::Draft(saved) = store
        .apply(
            "draft",
            &Command::SaveDraft {
                text: "retained".into(),
            },
            &config,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    store
        .apply(
            "composer",
            &Command::SaveComposer {
                text: "retained".into(),
                cursor: 0,
                revision: 0,
            },
            &config,
            None,
        )
        .unwrap();
    let cursor = store.cursor().unwrap();
    for (receipt, command) in [
        (
            "send",
            Command::Send {
                text: "retained".into(),
            },
        ),
        (
            "submit",
            Command::Submit {
                text: "retained".into(),
                mode: am::runtime::DeliveryMode::After,
                resume: true,
            },
        ),
        (
            "dispatch",
            Command::DispatchDraft {
                id: saved.id.clone(),
                revision: saved.revision,
                mode: am::runtime::DeliveryMode::Force,
                resume: true,
            },
        ),
    ] {
        let error = store.apply(receipt, &command, &config, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("artifact session quota exhausted")
        );
        assert!(store.previous(receipt, &command).unwrap().is_none());
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(store.paused().unwrap());
        assert!(store.queue(10).unwrap().is_empty());
        assert_eq!(store.composer().unwrap().text, "retained");
        assert_eq!(store.drafts().unwrap(), std::slice::from_ref(&saved));
    }
    let old = Command::Send {
        text: "first".into(),
    };
    store.apply("first", &old, &config, None).unwrap();
    assert_eq!(store.cursor().unwrap(), cursor);
    drop(store);
    let mut reopened = Store::open(&path, &config).unwrap();
    assert!(
        reopened
            .apply(
                "overflow",
                &Command::Send { text: "new".into() },
                &config,
                None
            )
            .is_err()
    );
    assert!(std::path::Path::new(&first.path).exists());
}

#[test]
fn incomplete_utf8_tail_is_visible_and_pages_advance_by_source_byte_offsets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "send");
    let text = "abc\u{1f642}";
    store.append_output(&attempt, text, &config.limits).unwrap();
    store.checkpoint(&attempt, text).unwrap();
    let record = output(&store);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&record.path)
        .unwrap()
        .set_len(5)
        .unwrap();
    drop(store);
    let reopened = Store::open(&path, &config).unwrap();
    let first = reopened.read_artifact(&record.id, 0, 4).unwrap();
    assert_eq!(first.text, "abc");
    assert_eq!(first.next_offset, 3);
    assert!(!first.eof);
    let last = reopened
        .read_artifact(&record.id, first.next_offset, 4)
        .unwrap();
    assert_eq!(last.text, "\u{fffd}");
    assert_eq!(last.next_offset, 5);
    assert!(last.eof);
    assert!(last.incomplete);
    assert!(reopened.read_artifact(&record.id, 4, 4).is_err());
    assert!(reopened.read_artifact(&record.id, 3, 1).is_err());
    let all = reopened.read_artifact(&record.id, 0, 5).unwrap();
    assert_eq!(all.text, "abc");
    assert_eq!(all.next_offset, 3);
    assert!(!all.eof);
}

#[test]
fn checkpoint_completion_cancellation_and_recovery_use_frozen_artifact_limits() {
    for outcome in ["completed", "cancelled", "recovery"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut original = Config::default();
        original.limits.response_bytes = 2 * 1024 * 1024;
        original.limits.output_bytes = 1024 * 1024;
        original.limits.artifact_bytes = 1024 * 1024;
        let mut store = Store::open(&path, &original).unwrap();
        store
            .apply(
                "send",
                &Command::Send {
                    text: "frozen".into(),
                },
                &original,
                None,
            )
            .unwrap();
        drop(store);
        let mut changed = Config::default();
        changed.limits.output_bytes = 2 * 1024 * 1024;
        changed.limits.artifact_bytes = 2 * 1024 * 1024;
        let mut store = Store::open(&path, &changed).unwrap();
        store
            .apply("resume", &Command::Resume, &changed, None)
            .unwrap();
        let attempt = store.start_next(&changed).unwrap().unwrap().attempt_id;
        let text = "x".repeat(original.limits.output_bytes + 64);
        if outcome == "recovery" {
            // An upgraded session or interrupted file write can have a durable
            // checkpoint but an empty artifact; recovery must not use new defaults.
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("UPDATE history SET text=?1 WHERE kind='assistant'", [&text])
                .unwrap();
            drop(store);
            store = Store::open(&path, &changed).unwrap();
            assert!(store.paused().unwrap());
            assert_eq!(store.actions(None, 1).unwrap()[0].status, "interrupted");
        } else {
            store.checkpoint(&attempt, &text).unwrap();
            assert_eq!(output(&store).bytes, original.limits.output_bytes as u64);
            let completion = Completion {
                attempt_id: &attempt,
                text: &text,
                status: outcome,
            };
            if outcome == "cancelled" {
                store
                    .apply(
                        "cancel",
                        &Command::Interrupt {
                            attempt_id: attempt.clone(),
                        },
                        &changed,
                        Some(completion),
                    )
                    .unwrap();
            } else {
                store.finish(completion).unwrap();
            }
        }
        let record = output(&store);
        assert_eq!(record.bytes, original.limits.output_bytes as u64);
        assert!(record.incomplete);
    }
}

#[test]
fn queued_work_is_not_started_without_artifact_capacity_and_keeps_its_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.output_bytes = 1024 * 1024;
    config.limits.artifact_bytes = 1024 * 1024;
    let mut store = Store::open(&path, &config).unwrap();
    let first = start(&mut store, &config, "first");
    let queued = Command::Send {
        text: "accepted before capacity exhausted".into(),
    };
    store.apply("queued", &queued, &config, None).unwrap();
    store
        .append_output(
            &first,
            &"x".repeat(config.limits.output_bytes),
            &config.limits,
        )
        .unwrap();
    store
        .finish(Completion {
            attempt_id: &first,
            text: "fixture",
            status: "completed",
        })
        .unwrap();
    let cursor = store.cursor().unwrap();
    let actions = store.actions(None, 200).unwrap();
    let queued_id = store.queue(10).unwrap()[0].id.clone();
    assert!(
        store
            .start_next(&config)
            .err()
            .unwrap()
            .to_string()
            .contains("artifact session quota exhausted")
    );
    assert_eq!(store.cursor().unwrap(), cursor);
    assert_eq!(store.actions(None, 200).unwrap(), actions);
    assert_eq!(store.queue(10).unwrap()[0].id, queued_id);
    store.apply("queued", &queued, &config, None).unwrap();
    assert_eq!(store.cursor().unwrap(), cursor);
    drop(store);
    let mut increased = config.clone();
    increased.limits.artifact_bytes *= 2;
    let mut reopened = Store::open(&path, &increased).unwrap();
    assert!(reopened.paused().unwrap());
    reopened
        .apply("resume", &Command::Resume, &increased, None)
        .unwrap();
    assert!(
        reopened.start_next(&increased).is_err(),
        "queued admission must use its frozen session limit"
    );
    assert_eq!(reopened.queue(10).unwrap()[0].id, queued_id);
}

#[test]
fn conversation_boundaries_break_settings_groups_and_persist_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let settings = Command::RecordAction {
        text: "settings saved".into(),
        level: NoticeLevel::Info,
        category: "settings".into(),
    };
    store.apply("before", &settings, &config, None).unwrap();
    let before = store.actions(None, 1).unwrap()[0].clone();
    let attempt = start(&mut store, &config, "conversation");
    store.apply("during", &settings, &config, None).unwrap();
    let during = store.actions(None, 1).unwrap()[0].clone();
    assert_ne!(before.group_id, during.group_id);

    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_finish BEFORE INSERT ON events WHEN NEW.kind='attempt_finished' BEGIN SELECT RAISE(ABORT,'finish fault'); END;").unwrap();
    assert!(
        store
            .finish(Completion {
                attempt_id: &attempt,
                text: "response",
                status: "completed"
            })
            .is_err()
    );
    store
        .apply("still-during", &settings, &config, None)
        .unwrap();
    assert_eq!(store.actions(None, 1).unwrap()[0].group_id, during.group_id);
    db.execute_batch("DROP TRIGGER fail_finish").unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt,
            text: "response",
            status: "completed",
        })
        .unwrap();
    let existing = store.actions(None, 200).unwrap();
    drop(store);

    let mut reopened = Store::open(&path, &config).unwrap();
    assert_eq!(reopened.actions(None, 200).unwrap(), existing);
    reopened.apply("after", &settings, &config, None).unwrap();
    let after = reopened.actions(None, 1).unwrap()[0].clone();
    assert_ne!(after.group_id, during.group_id);
    reopened
        .apply("adjacent-after", &settings, &config, None)
        .unwrap();
    assert_eq!(
        reopened.actions(None, 1).unwrap()[0].group_id,
        after.group_id
    );
    let cursor = reopened.cursor().unwrap();
    reopened.apply("during", &settings, &config, None).unwrap();
    assert_eq!(reopened.cursor().unwrap(), cursor);
    reopened
        .apply("after-replay", &settings, &config, None)
        .unwrap();
    assert_eq!(
        reopened.actions(None, 1).unwrap()[0].group_id,
        after.group_id
    );
}

#[test]
fn pre_boundary_sessions_initialize_groups_from_structural_events_without_rewriting_ids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "conversation");
    let settings = Command::RecordAction {
        text: "settings saved".into(),
        level: NoticeLevel::Info,
        category: "settings".into(),
    };
    store.apply("during", &settings, &config, None).unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt,
            text: "response",
            status: "completed",
        })
        .unwrap();
    let existing = store.actions(None, 200).unwrap();
    let group = existing.last().unwrap().group_id.clone();
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE action_group_boundary")
        .unwrap();

    let mut reopened = Store::open(&path, &config).unwrap();
    assert_eq!(reopened.actions(None, 200).unwrap(), existing);
    reopened.apply("after", &settings, &config, None).unwrap();
    assert_ne!(reopened.actions(None, 1).unwrap()[0].group_id, group);
    let after = reopened.actions(None, 1).unwrap()[0].group_id.clone();
    drop(reopened);
    let mut reopened = Store::open(&path, &config).unwrap();
    reopened
        .apply("adjacent", &settings, &config, None)
        .unwrap();
    assert_eq!(reopened.actions(None, 1).unwrap()[0].group_id, after);
}

#[test]
fn large_backward_artifact_traversal_is_contiguous_bounded_and_utf8_aligned() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "large");
    let text = "a\u{e9}\u{1f642}z\n".repeat(40_000);
    for block in text.as_bytes().chunks(9 * 1024) {
        store
            .append_output(
                &attempt,
                std::str::from_utf8(block).unwrap(),
                &config.limits,
            )
            .unwrap();
    }
    store
        .finish(Completion {
            attempt_id: &attempt,
            text: "fixture result",
            status: "completed",
        })
        .unwrap();
    let record = output(&store);
    assert!(record.bytes > MAX_READ_BYTES as u64);
    let cursor = store.cursor().unwrap();
    for requested in [4097, 8192, MAX_READ_BYTES, usize::MAX] {
        let limit = requested.clamp(4, MAX_READ_BYTES);
        let mut before = record.bytes;
        let mut pages = Vec::new();
        while before > 0 {
            let (offset, chunk) = store
                .read_artifact_before(&record.id, before, requested)
                .unwrap();
            assert!(offset < before);
            assert!(text.is_char_boundary(offset as usize));
            assert_eq!(chunk.next_offset, before);
            assert_eq!(chunk.eof, before == record.bytes);
            assert!(!chunk.incomplete);
            assert_eq!(chunk.text, text[offset as usize..before as usize]);
            assert!(chunk.text.len() <= limit);
            let raw_start = before.saturating_sub(limit as u64);
            assert!(offset - raw_start <= 3);
            pages.push(chunk.text);
            before = offset;
        }
        pages.reverse();
        assert_eq!(pages.concat(), text);
    }
    let (offset, empty) = store.read_artifact_before(&record.id, 0, 8192).unwrap();
    assert_eq!(offset, 0);
    assert!(empty.text.is_empty());
    assert_eq!(empty.next_offset, 0);
    assert!(!empty.eof);
    assert!(store.read_artifact_before(&record.id, 2, 8192).is_err());
    assert!(
        store
            .read_artifact_before(&record.id, record.bytes + 1, 8192)
            .is_err()
    );
    assert_eq!(store.cursor().unwrap(), cursor);
}

#[test]
fn backward_pages_make_progress_for_small_limits_and_incomplete_unicode_tails() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "unicode");
    let text = "\u{1f642}a\u{e9}\u{1f642}";
    store.append_output(&attempt, text, &config.limits).unwrap();
    store.checkpoint(&attempt, text).unwrap();
    let record = output(&store);
    for limit in [0, 1, 3, 4, 5, 7] {
        let mut before = record.bytes;
        let mut pages = Vec::new();
        while before > 0 {
            let (offset, chunk) = store
                .read_artifact_before(&record.id, before, limit)
                .unwrap();
            assert!(offset < before);
            assert_eq!(chunk.next_offset, before);
            pages.push(chunk.text);
            before = offset;
        }
        pages.reverse();
        assert_eq!(pages.concat(), text);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(&record.path)
        .unwrap()
        .set_len(record.bytes - 2)
        .unwrap();
    drop(store);
    let reopened = Store::open(&path, &config).unwrap();
    let (offset, chunk) = reopened
        .read_artifact_before(&record.id, record.bytes - 2, MAX_READ_BYTES)
        .unwrap();
    assert_eq!(offset, 0);
    assert_eq!(chunk.text, "\u{1f642}a\u{e9}\u{fffd}");
    assert_eq!(chunk.next_offset, record.bytes - 2);
    assert!(chunk.incomplete);
    assert!(chunk.eof);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backward_artifact_command_returns_explicit_offset_without_durable_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    let attempt = start(&mut store, &config, "artifact");
    let text = "prefix \u{1f642} suffix";
    store.append_output(&attempt, text, &config.limits).unwrap();
    store
        .finish(Completion {
            attempt_id: &attempt,
            text,
            status: "completed",
        })
        .unwrap();
    let record = output(&store);
    drop(store);
    let client = Client::start(&path, config).await.unwrap();
    let cursor = client.snapshot().cursor;
    let db = rusqlite::Connection::open(&path).unwrap();
    let receipts: i64 = db
        .query_row("SELECT COUNT(*) FROM command_results", [], |r| r.get(0))
        .unwrap();
    let Reply::ArtifactPage { offset, chunk } = client
        .command(Command::ReadArtifactBefore {
            id: record.id.clone(),
            before: record.bytes,
            limit: 8,
        })
        .await
        .unwrap()
    else {
        panic!("expected backward artifact page")
    };
    assert_eq!(chunk.next_offset, record.bytes);
    assert_eq!(chunk.text, text[offset as usize..]);
    let Reply::ArtifactPage {
        offset: previous,
        chunk,
    } = client
        .command(Command::ReadArtifactBefore {
            id: record.id,
            before: offset,
            limit: 8,
        })
        .await
        .unwrap()
    else {
        panic!("expected backward artifact page")
    };
    assert_eq!(chunk.next_offset, offset);
    assert_eq!(chunk.text, text[previous as usize..offset as usize]);
    assert_eq!(client.snapshot().cursor, cursor);
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT COUNT(*) FROM command_results", [], |r| r.get(0))
            .unwrap(),
        receipts
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_action_pages_catch_up_without_skipping_records_or_writing_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.history_page = 7;
    let mut store = Store::open(&path, &config).unwrap();
    for i in 0..29 {
        store
            .apply(
                &format!("action-{i}"),
                &Command::RecordAction {
                    text: format!("setting {i}"),
                    level: NoticeLevel::Info,
                    category: "settings".into(),
                },
                &config,
                None,
            )
            .unwrap();
    }
    let expected = store.actions(None, 200).unwrap();
    assert_eq!(
        store.actions_after(expected[6].seq, 7).unwrap(),
        expected[7..14]
    );
    drop(store);
    let client = Client::start(&path, config).await.unwrap();
    let cursor = client.snapshot().cursor;
    let db = rusqlite::Connection::open(&path).unwrap();
    let receipts: i64 = db
        .query_row("SELECT COUNT(*) FROM command_results", [], |r| r.get(0))
        .unwrap();
    let mut after = 0;
    let mut restored = Vec::new();
    loop {
        let Reply::Actions(page) = client
            .command(Command::ActionsAfter {
                after,
                limit: usize::MAX,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(page.len() <= 7);
        let Some(last) = page.last() else { break };
        after = last.seq;
        restored.extend(page);
    }
    assert_eq!(restored, expected);
    assert_eq!(client.snapshot().cursor, cursor);
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT COUNT(*) FROM command_results", [], |r| r.get(0))
            .unwrap(),
        receipts
    );
    client.shutdown().await.unwrap();
}

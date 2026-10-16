use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, ComposerState, DeliveryMode, QueueState, Reply, Snapshot},
    storage::{Completion, Store},
};
use rusqlite::Connection;
use std::time::Duration;
use tokio::time::{sleep, timeout};

fn accumulated(source: &str, revision: i64, paused: bool, resume: bool) -> Command {
    Command::SubmitInputAccumulated {
        source: source.into(),
        text: source
            .strip_prefix("//")
            .map_or(source, |_| &source[1..])
            .into(),
        composer_revision: revision,
        mode: DeliveryMode::After,
        resume,
        expected_paused: paused,
    }
}

async fn wait(client: &Client, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let mut updates = client.subscribe();
    timeout(Duration::from_secs(8), async {
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

#[test]
fn append_preserves_identity_fifo_and_frozen_config_even_at_queue_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.queue_messages = 1;
    config.provider.first_event_ms = 543;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    let first = accumulated("first", 0, true, false);
    assert!(matches!(
        store.apply("first", &first, &config, None).unwrap(),
        Reply::Ack
    ));
    let item = store.queue(10).unwrap().remove(0);
    let mut changed = config.clone();
    changed.provider.first_event_ms = 987;
    let second = accumulated("//second", 1, true, false);
    let wire = serde_json::to_string(&second).unwrap();
    assert!(wire.starts_with("{\"SubmitInputAccumulated\":"));
    let decoded: Command = serde_json::from_str(&wire).unwrap();
    assert!(matches!(
        store.apply("second", &decoded, &changed, None).unwrap(),
        Reply::Ack
    ));
    let queue = store.queue(10).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, item.id);
    assert_eq!(queue[0].revision, item.revision + 1);
    assert_eq!(queue[0].text, "first\n\n/second");
    assert_eq!(store.inputs(10).unwrap(), ["first\n\n/second"]);
    assert!(store.paused().unwrap());
    assert!(store.start_next(&changed).unwrap().is_none());
    let events = store.events(0, 200).unwrap();
    assert_eq!(
        events.iter().filter(|e| e.kind == "message_queued").count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "message_appended")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().payload, item.id);
    store
        .apply("resume", &Command::Resume, &changed, None)
        .unwrap();
    let started = store.start_next(&changed).unwrap().unwrap();
    assert_eq!(started.message_id, item.id);
    assert_eq!(started.config.provider.first_event_ms, 543);
    assert_eq!(store.history(None, 10).unwrap()[0].text, "first\n\n/second");
    store
        .apply(
            "third",
            &accumulated("third", 2, false, false),
            &changed,
            None,
        )
        .unwrap();
    let next = store.queue(10).unwrap().remove(0);
    assert_ne!(next.id, item.id);
    assert_eq!(next.revision, 0);
    assert_eq!(next.text, "third");
}

#[test]
fn create_and_append_roll_back_all_durable_state_on_any_transaction_failure() {
    for append in [false, true] {
        for (table, operation) in [
            ("messages", if append { "UPDATE" } else { "INSERT" }),
            ("composer", "UPDATE"),
            ("session", "UPDATE"),
            ("events", "INSERT"),
            ("command_results", "INSERT"),
            ("dispatch_configs", "INSERT"),
        ] {
            if append && table == "dispatch_configs" {
                continue;
            }
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.sqlite3");
            let config = Config::default();
            let mut store = Store::open(&path, &config).unwrap();
            store
                .apply("pause", &Command::Pause, &config, None)
                .unwrap();
            if append {
                store
                    .apply(
                        "first",
                        &accumulated("first", 0, true, false),
                        &config,
                        None,
                    )
                    .unwrap();
            }
            let revision = store.composer().unwrap().revision;
            store
                .apply(
                    "save",
                    &Command::SaveComposer {
                        text: "saved earlier".into(),
                        cursor: 3,
                        revision,
                    },
                    &config,
                    None,
                )
                .unwrap();
            let composer = store.composer().unwrap();
            let before = serde_json::to_value(store.queue(10).unwrap()).unwrap();
            let cursor = store.cursor().unwrap();
            let db = Connection::open(&path).unwrap();
            let configs: i64 = db
                .query_row("SELECT COUNT(*) FROM dispatch_configs", [], |r| r.get(0))
                .unwrap();
            db.execute_batch(&format!("CREATE TRIGGER fail_batch BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'batch fault'); END;")).unwrap();
            let command = accumulated("//new editor text", composer.revision, true, true);
            assert!(
                store.apply("batch", &command, &config, None).is_err(),
                "{append}: {table}"
            );
            assert_eq!(store.composer().unwrap(), composer);
            assert_eq!(
                serde_json::to_value(store.queue(10).unwrap()).unwrap(),
                before
            );
            assert_eq!(store.cursor().unwrap(), cursor);
            assert!(store.paused().unwrap());
            assert!(store.previous("batch", &command).unwrap().is_none());
            assert_eq!(
                db.query_row::<i64, _, _>("SELECT COUNT(*) FROM dispatch_configs", [], |r| r
                    .get(0))
                    .unwrap(),
                configs
            );
            db.execute_batch("DROP TRIGGER fail_batch").unwrap();
            assert!(matches!(
                store.apply("batch", &command, &config, None).unwrap(),
                Reply::Ack
            ));
            assert!(!store.paused().unwrap());
            assert_eq!(
                store.composer().unwrap(),
                ComposerState {
                    text: String::new(),
                    cursor: 0,
                    revision: composer.revision + 1
                }
            );
            let queue = store.queue(10).unwrap();
            assert_eq!(queue.len(), 1);
            assert_eq!(
                queue[0].text,
                if append {
                    "first\n\n/new editor text"
                } else {
                    "/new editor text"
                }
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lowered_queue_limit_keeps_legacy_rows_visible_and_accumulates_latest_in_fifo_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.queue_messages = 2;
    config.provider.scenario = Scenario::Waiting;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    for text in ["older", "latest"] {
        store
            .apply(
                text,
                &Command::Submit {
                    text: text.into(),
                    mode: DeliveryMode::After,
                    resume: false,
                },
                &config,
                None,
            )
            .unwrap();
    }
    let original = store.queue(1024).unwrap();
    drop(store);

    config.limits.queue_messages = 1;
    let client = Client::start(&path, config).await.unwrap();
    let before = client.snapshot();
    assert!(before.paused);
    assert!(before.active.is_none());
    assert_eq!(before.queue.len(), 2);
    assert_eq!(
        serde_json::to_value(&before.queue).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert!(
        client
            .command(Command::Submit {
                text: "new row".into(),
                mode: DeliveryMode::After,
                resume: false,
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("queue limit")
    );
    assert_eq!(client.snapshot().cursor, before.cursor);

    client
        .command(accumulated("fragment", 0, true, false))
        .await
        .unwrap();
    let updated = client.snapshot();
    assert!(updated.paused);
    assert!(updated.active.is_none());
    assert_eq!(updated.queue.len(), 2);
    assert_eq!(
        serde_json::to_value(&updated.queue[0]).unwrap(),
        serde_json::to_value(&original[0]).unwrap()
    );
    assert_eq!(updated.queue[1].id, original[1].id);
    assert_eq!(updated.queue[1].revision, original[1].revision + 1);
    assert_eq!(updated.queue[1].text, "latest\n\nfragment");
    assert_eq!(updated.queue[1].state, QueueState::Queued);

    client.command(Command::Resume).await.unwrap();
    let first = wait(&client, |s| s.active.is_some()).await;
    assert_eq!(first.queue.len(), 1);
    assert_eq!(first.queue[0].id, original[1].id);
    let attempt_id = first.active.unwrap().attempt_id;
    client
        .command(Command::Interrupt {
            attempt_id: attempt_id.clone(),
        })
        .await
        .unwrap();
    wait(&client, |s| {
        s.queue.is_empty()
            && s.active
                .as_ref()
                .is_some_and(|a| a.attempt_id != attempt_id)
    })
    .await;
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
    assert_eq!(
        history
            .iter()
            .filter(|entry| entry.kind == "user")
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["older", "latest\n\nfragment"]
    );
    client.shutdown().await.unwrap();
}

#[test]
fn rejected_append_validates_combined_bytes_source_revision_and_pause_without_consuming_input() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.message_bytes = 10;
    config.limits.queue_messages = 1;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    store
        .apply(
            "first",
            &accumulated("first", 0, true, false),
            &config,
            None,
        )
        .unwrap();
    store
        .apply(
            "save",
            &Command::SaveComposer {
                text: "retained".into(),
                cursor: 4,
                revision: 1,
            },
            &config,
            None,
        )
        .unwrap();
    let before = store.composer().unwrap();
    let queue = serde_json::to_value(store.queue(10).unwrap()).unwrap();
    let cursor = store.cursor().unwrap();
    let mut bad_payload = accumulated("//x", 2, true, true);
    if let Command::SubmitInputAccumulated { text, .. } = &mut bad_payload {
        *text = "//x".into();
    }
    for command in [
        accumulated("\u{e9}\u{e9}", 2, true, true), // 5 + 2 + 4 bytes exceeds 10.
        accumulated("x", 1, true, true),
        accumulated("x", 2, false, true),
        accumulated(" ", 2, true, true),
        accumulated("12345678901", 2, true, true),
        bad_payload,
    ] {
        assert!(store.apply("reject", &command, &config, None).is_err());
        assert!(store.previous("reject", &command).unwrap().is_none());
        assert_eq!(store.composer().unwrap(), before);
        assert_eq!(
            serde_json::to_value(store.queue(10).unwrap()).unwrap(),
            queue
        );
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(store.paused().unwrap());
    }
    store
        .apply("exact", &accumulated("abc", 2, true, false), &config, None)
        .unwrap();
    assert_eq!(store.queue(10).unwrap()[0].text, "first\n\nabc");
}

#[test]
fn editing_blocks_accumulation_and_cancelled_latest_input_never_reuses_older_queue_items() {
    for editing in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.limits.queue_messages = 2;
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply("pause", &Command::Pause, &config, None)
            .unwrap();
        for text in ["older", "latest"] {
            store
                .apply(text, &Command::Send { text: text.into() }, &config, None)
                .unwrap();
        }
        let latest = store.queue(10).unwrap().remove(1);
        let command = if editing {
            Command::BeginEdit {
                id: latest.id.clone(),
                revision: latest.revision,
            }
        } else {
            Command::Cancel {
                id: latest.id.clone(),
                revision: latest.revision,
            }
        };
        store.apply("change", &command, &config, None).unwrap();
        let command = accumulated("new", 0, true, false);
        if editing {
            let cursor = store.cursor().unwrap();
            assert!(
                store
                    .apply("new", &command, &config, None)
                    .unwrap_err()
                    .to_string()
                    .contains("pending edit")
            );
            assert_eq!(store.cursor().unwrap(), cursor);
            assert_eq!(store.composer().unwrap().revision, 0);
            assert_eq!(store.queue(10).unwrap()[1].state, QueueState::Editing);
            assert_eq!(store.queue(10).unwrap()[0].text, "older");
            continue;
        }
        store.apply("new", &command, &config, None).unwrap();
        let queue = store.queue(10).unwrap();
        assert_eq!(queue[0].text, "older");
        assert_eq!(queue[0].revision, 0);
        assert_eq!(queue.last().unwrap().text, "new");
        assert_ne!(queue.last().unwrap().id, latest.id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accumulated_receipt_replays_after_reopen_and_lowered_limit_without_duplicate_or_resume() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client
        .command(accumulated("first", 0, true, false))
        .await
        .unwrap();
    let command = accumulated("//second text", 1, true, true);
    let reply = client
        .command_with_id("append".into(), command.clone())
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    let mut changed = Config::default();
    changed.limits.message_bytes = 4;
    let reopened = Client::start(&path, changed).await.unwrap();
    let state = reopened.snapshot();
    let Reply::Composer(composer) = reopened.command(Command::Composer).await.unwrap() else {
        panic!()
    };
    for _ in 0..2 {
        let replay = reopened
            .command_with_id("append".into(), command.clone())
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(&reply).unwrap()
        );
    }
    assert!(
        reopened
            .command_with_id("different".into(), command.clone())
            .await
            .is_err()
    );
    assert!(
        reopened
            .command_with_id("append".into(), accumulated("other", 2, true, false))
            .await
            .is_err()
    );
    assert!(reopened.snapshot().paused);
    assert!(reopened.snapshot().active.is_none());
    assert_eq!(reopened.snapshot().cursor, state.cursor);
    assert_eq!(
        serde_json::to_value(reopened.snapshot().queue).unwrap(),
        serde_json::to_value(state.queue).unwrap()
    );
    assert!(
        matches!(reopened.command(Command::Composer).await.unwrap(), Reply::Composer(state) if state == composer)
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_fragments_coalesce_wait_for_full_completion_and_legacy_stop_retains_pending() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.first_event_ms = 0;
    config.provider.delay_ms = 100;
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
    timeout(Duration::from_secs(5), async {
        loop {
            let Reply::Events(events) = client
                .command(Command::Events {
                    after: 0,
                    limit: 200,
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            if events.iter().any(|e| e.kind == "fixture_tool_started") {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    client
        .command(accumulated("one", 0, false, false))
        .await
        .unwrap();
    let item = client.snapshot().queue[0].clone();
    client
        .command(accumulated("two", 1, false, false))
        .await
        .unwrap();
    assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
    assert_eq!(client.snapshot().queue.len(), 1);
    assert_eq!(client.snapshot().queue[0].id, item.id);
    assert_eq!(client.snapshot().queue[0].revision, item.revision + 1);
    assert_eq!(client.snapshot().queue[0].text, "one\n\ntwo");
    wait(&client, |s| {
        s.active.as_ref().is_some_and(|a| a.attempt_id != first)
    })
    .await;
    let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(history[1].status, "completed");
    assert!(history[1].text.ends_with("Streaming chunk 23. "));
    assert_eq!(history[2].text, "one\n\ntwo");
    let Reply::Events(events) = client
        .command(Command::Events {
            after: 0,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let tool_finished = events
        .iter()
        .position(|e| e.kind == "fixture_tool_finished" && e.attempt_id.as_deref() == Some(&first))
        .unwrap();
    let generation_finished = events
        .iter()
        .position(|e| e.kind == "attempt_finished" && e.attempt_id.as_deref() == Some(&first))
        .unwrap();
    let next_started = events
        .iter()
        .position(|e| e.kind == "attempt_started" && e.attempt_id.as_deref() != Some(&first))
        .unwrap();
    assert!(tool_finished < generation_finished && generation_finished < next_started);

    client.command(Command::Pause).await.unwrap();
    client
        .command(accumulated("held", 2, true, false))
        .await
        .unwrap();
    client.command(Command::Stop).await.unwrap();
    let pending = client.snapshot().queue[0].clone();
    sleep(Duration::from_millis(50)).await;
    assert!(client.snapshot().paused);
    assert!(client.snapshot().active.is_none());
    client
        .command(accumulated("still held", 3, true, false))
        .await
        .unwrap();
    sleep(Duration::from_millis(50)).await;
    assert!(client.snapshot().paused);
    assert!(client.snapshot().active.is_none());
    assert_eq!(client.snapshot().queue[0].id, pending.id);
    client
        .command(accumulated("run", 4, true, true))
        .await
        .unwrap();
    assert!(!client.snapshot().paused);
    wait(&client, |s| s.active.is_some() && s.queue.is_empty()).await;
    let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(history.last().unwrap().text, "held\n\nstill held\n\nrun");
    client.shutdown().await.unwrap();
}

#[test]
fn reopened_append_enforces_current_and_original_frozen_limits_without_losing_composer_or_draft() {
    for (original_limit, current_limit) in [(10, 20), (20, 10)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.limits.message_bytes = original_limit;
        config.provider.first_event_ms = 321;
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply(
                "first",
                &accumulated("first", 0, false, false),
                &config,
                None,
            )
            .unwrap();
        drop(store);
        let mut changed = config.clone();
        changed.limits.message_bytes = current_limit;
        changed.provider.first_event_ms = 654;
        let mut store = Store::open(&path, &changed).unwrap();
        store
            .apply(
                "save",
                &Command::SaveComposer {
                    text: "retained".into(),
                    cursor: 3,
                    revision: 1,
                },
                &changed,
                None,
            )
            .unwrap();
        let Reply::Draft(draft) = store
            .apply(
                "draft",
                &Command::SaveDraft {
                    text: "//abcd".into(),
                },
                &changed,
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        let composer = store.composer().unwrap();
        let queue = serde_json::to_value(store.queue(10).unwrap()).unwrap();
        let cursor = store.cursor().unwrap();
        for command in [
            accumulated("abcd", composer.revision, true, true),
            Command::DispatchDraftAccumulated {
                id: draft.id.clone(),
                revision: draft.revision,
                expected_paused: true,
            },
        ] {
            assert!(
                store
                    .apply("overflow", &command, &changed, None)
                    .unwrap_err()
                    .to_string()
                    .contains("combined message byte limit")
            );
            assert_eq!(store.composer().unwrap(), composer);
            assert_eq!(
                store.drafts().unwrap().as_slice(),
                std::slice::from_ref(&draft)
            );
            assert_eq!(
                serde_json::to_value(store.queue(10).unwrap()).unwrap(),
                queue
            );
            assert!(store.paused().unwrap());
            assert_eq!(store.cursor().unwrap(), cursor);
        }
        store
            .apply(
                "exact",
                &accumulated("abc", composer.revision, true, true),
                &changed,
                None,
            )
            .unwrap();
        let started = store.start_next(&changed).unwrap().unwrap();
        assert_eq!(started.config.provider.first_event_ms, 321);
        assert_eq!(started.config.limits.message_bytes, original_limit);
        assert_eq!(store.history(None, 10).unwrap()[0].text, "first\n\nabc");
    }
}

#[test]
fn draft_accumulation_consumption_resume_and_receipt_roll_back_together_and_keep_fifo() {
    for append in [false, true] {
        for (table, operation) in [
            ("messages", if append { "UPDATE" } else { "INSERT" }),
            ("drafts", "DELETE"),
            ("session", "UPDATE"),
            ("events", "INSERT"),
            ("command_results", "INSERT"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.sqlite3");
            let mut config = Config::default();
            config.limits.queue_messages = 2;
            let mut store = Store::open(&path, &config).unwrap();
            store.apply("stop", &Command::Stop, &config, None).unwrap();
            if append {
                for text in ["older", "latest"] {
                    store
                        .apply(text, &Command::Send { text: text.into() }, &config, None)
                        .unwrap();
                }
            }
            store
                .apply(
                    "composer",
                    &Command::SaveComposer {
                        text: "unrelated".into(),
                        cursor: 2,
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
                        text: "//draft\n//second line".into(),
                    },
                    &config,
                    None,
                )
                .unwrap()
            else {
                panic!()
            };
            let command = Command::DispatchDraftAccumulated {
                id: draft.id.clone(),
                revision: draft.revision,
                expected_paused: true,
            };
            let wire = serde_json::to_string(&command).unwrap();
            assert!(wire.starts_with("{\"DispatchDraftAccumulated\":"));
            let command = serde_json::from_str(&wire).unwrap();
            let queue = store.queue(10).unwrap();
            let composer = store.composer().unwrap();
            let cursor = store.cursor().unwrap();
            let db = Connection::open(&path).unwrap();
            db.execute_batch(&format!("CREATE TRIGGER fail_draft_batch BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'draft batch fault'); END;")).unwrap();
            assert!(store.apply("send-draft", &command, &config, None).is_err());
            assert_eq!(
                serde_json::to_value(store.queue(10).unwrap()).unwrap(),
                serde_json::to_value(&queue).unwrap()
            );
            assert_eq!(store.composer().unwrap(), composer);
            assert_eq!(store.drafts().unwrap(), [draft]);
            assert!(store.paused().unwrap());
            assert_eq!(store.cursor().unwrap(), cursor);
            assert!(store.previous("send-draft", &command).unwrap().is_none());
            db.execute_batch("DROP TRIGGER fail_draft_batch").unwrap();
            assert!(matches!(
                store.apply("send-draft", &command, &config, None).unwrap(),
                Reply::Ack
            ));
            assert!(!store.paused().unwrap());
            assert!(store.drafts().unwrap().is_empty());
            assert_eq!(store.composer().unwrap(), composer);
            let updated = store.queue(10).unwrap();
            if append {
                assert_eq!(
                    serde_json::to_value(&updated[0]).unwrap(),
                    serde_json::to_value(&queue[0]).unwrap()
                );
                assert_eq!(updated[1].id, queue[1].id);
                assert_eq!(updated[1].revision, queue[1].revision + 1);
                assert_eq!(updated[1].text, "latest\n\n/draft\n//second line");
            } else {
                assert_eq!(updated.len(), 1);
                assert_eq!(updated[0].text, "/draft\n//second line");
            }
            store
                .apply("pause-again", &Command::Pause, &config, None)
                .unwrap();
            let cursor = store.cursor().unwrap();
            store.apply("send-draft", &command, &config, None).unwrap();
            assert!(store.paused().unwrap());
            assert_eq!(store.cursor().unwrap(), cursor);
            assert_eq!(
                serde_json::to_value(store.queue(10).unwrap()).unwrap(),
                serde_json::to_value(updated).unwrap()
            );
        }
    }
}

#[test]
fn draft_send_rejects_lease_pause_revision_and_creation_capacity_conflicts_without_consuming_draft()
{
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.queue_messages = 2;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    for text in ["older", "latest"] {
        store
            .apply(text, &Command::Send { text: text.into() }, &config, None)
            .unwrap();
    }
    let Reply::Draft(draft) = store
        .apply(
            "draft",
            &Command::SaveDraft {
                text: "saved".into(),
            },
            &config,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    let queue = store.queue(10).unwrap();
    // The older lease must block even when the latest item is queued.
    store
        .apply(
            "lease",
            &Command::BeginEdit {
                id: queue[0].id.clone(),
                revision: queue[0].revision,
            },
            &config,
            None,
        )
        .unwrap();
    let leased = store.queue(10).unwrap();
    let cursor = store.cursor().unwrap();
    let command = Command::DispatchDraftAccumulated {
        id: draft.id.clone(),
        revision: draft.revision,
        expected_paused: true,
    };
    for command in [command.clone(), accumulated("new", 0, true, true)] {
        assert!(
            store
                .apply("blocked", &command, &config, None)
                .unwrap_err()
                .to_string()
                .contains("pending edit")
        );
        assert_eq!(
            serde_json::to_value(store.queue(10).unwrap()).unwrap(),
            serde_json::to_value(&leased).unwrap()
        );
        assert_eq!(
            store.drafts().unwrap().as_slice(),
            std::slice::from_ref(&draft)
        );
        assert_eq!(store.composer().unwrap().revision, 0);
        assert!(store.paused().unwrap());
        assert_eq!(store.cursor().unwrap(), cursor);
    }
    store
        .apply(
            "release",
            &Command::CancelEdit {
                id: leased[0].id.clone(),
                revision: leased[0].revision,
            },
            &config,
            None,
        )
        .unwrap();
    for (revision, expected_paused) in [(draft.revision + 1, true), (draft.revision, false)] {
        assert!(
            store
                .apply(
                    "conflict",
                    &Command::DispatchDraftAccumulated {
                        id: draft.id.clone(),
                        revision,
                        expected_paused
                    },
                    &config,
                    None
                )
                .is_err()
        );
        assert_eq!(
            store.drafts().unwrap().as_slice(),
            std::slice::from_ref(&draft)
        );
        assert!(store.paused().unwrap());
    }
    // Cancel the latest item and lower capacity: admission must not reuse older.
    store
        .apply(
            "cancel",
            &Command::Cancel {
                id: queue[1].id.clone(),
                revision: queue[1].revision,
            },
            &config,
            None,
        )
        .unwrap();
    config.limits.queue_messages = 1;
    assert!(
        store
            .apply("full", &command, &config, None)
            .unwrap_err()
            .to_string()
            .contains("queue limit")
    );
    assert_eq!(store.queue(10).unwrap()[0].text, "older");
    assert_eq!(store.drafts().unwrap(), [draft]);
    assert!(store.paused().unwrap());
}

#[test]
fn finish_pending_edit_replaces_payload_and_resumes_atomically_without_changing_fifo_or_config() {
    for (table, operation) in [
        ("messages", "UPDATE"),
        ("session", "UPDATE"),
        ("events", "INSERT"),
        ("command_results", "INSERT"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        store.apply("stop", &Command::Stop, &config, None).unwrap();
        for text in ["older", "latest"] {
            store
                .apply(text, &Command::Send { text: text.into() }, &config, None)
                .unwrap();
        }
        let item = store.queue(10).unwrap().remove(1);
        let Reply::Editing(lease) = store
            .apply(
                "lease",
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
        let queue = store.queue(10).unwrap();
        let cursor = store.cursor().unwrap();
        let command = Command::FinishPendingEdit {
            id: lease.id.clone(),
            revision: lease.revision,
            text: "//literal payload".into(),
            expected_paused: true,
        };
        let wire = serde_json::to_string(&command).unwrap();
        assert!(wire.starts_with("{\"FinishPendingEdit\":"));
        let command = serde_json::from_str(&wire).unwrap();
        let db = Connection::open(&path).unwrap();
        let frozen: String = db
            .query_row(
                "SELECT config FROM dispatch_configs WHERE message_id=?1",
                [&lease.id],
                |r| r.get(0),
            )
            .unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_edit BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'edit fault'); END;")).unwrap();
        assert!(store.apply("finish", &command, &config, None).is_err());
        assert_eq!(
            serde_json::to_value(store.queue(10).unwrap()).unwrap(),
            serde_json::to_value(&queue).unwrap()
        );
        assert!(store.paused().unwrap());
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(store.previous("finish", &command).unwrap().is_none());
        db.execute_batch("DROP TRIGGER fail_edit").unwrap();
        assert!(matches!(
            store.apply("finish", &command, &config, None).unwrap(),
            Reply::Ack
        ));
        assert!(!store.paused().unwrap());
        let updated = store.queue(10).unwrap();
        assert_eq!(
            serde_json::to_value(&updated[0]).unwrap(),
            serde_json::to_value(&queue[0]).unwrap()
        );
        assert_eq!(updated[1].id, item.id);
        assert_eq!(updated[1].state, QueueState::Queued);
        assert_eq!(updated[1].revision, lease.revision + 1);
        assert_eq!(updated[1].text, "//literal payload");
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT config FROM dispatch_configs WHERE message_id=?1",
                [&lease.id],
                |r| r.get(0)
            )
            .unwrap(),
            frozen
        );
        assert_eq!(store.composer().unwrap().revision, 0);
    }
}

#[test]
fn finish_pending_edit_rejects_empty_conflicting_and_oversized_payloads_and_cancel_deletes_instead()
{
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.message_bytes = 10;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "first",
            &accumulated("first", 0, false, false),
            &config,
            None,
        )
        .unwrap();
    store.apply("stop", &Command::Stop, &config, None).unwrap();
    let item = store.queue(10).unwrap().remove(0);
    let cursor = store.cursor().unwrap();
    let finish = Command::FinishPendingEdit {
        id: item.id.clone(),
        revision: item.revision,
        text: "new".into(),
        expected_paused: true,
    };
    assert!(store.apply("not-leased", &finish, &config, None).is_err());
    assert_eq!(store.cursor().unwrap(), cursor);
    let Reply::Editing(lease) = store
        .apply(
            "lease",
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
    let queue = serde_json::to_value(store.queue(10).unwrap()).unwrap();
    let cursor = store.cursor().unwrap();
    for (text, revision, expected_paused, limit) in [
        ("", lease.revision, true, 10),
        (" \n", lease.revision, true, 10),
        ("x", item.revision, true, 10),
        ("x", lease.revision, false, 10),
        ("12345678901", lease.revision, true, 20),
        ("123456789", lease.revision, true, 8),
    ] {
        config.limits.message_bytes = limit;
        let command = Command::FinishPendingEdit {
            id: lease.id.clone(),
            revision,
            text: text.into(),
            expected_paused,
        };
        assert!(store.apply("reject", &command, &config, None).is_err());
        assert!(store.previous("reject", &command).unwrap().is_none());
        assert!(store.paused().unwrap());
        assert_eq!(store.cursor().unwrap(), cursor);
        assert_eq!(
            serde_json::to_value(store.queue(10).unwrap()).unwrap(),
            queue
        );
    }
    store
        .apply(
            "delete",
            &Command::Cancel {
                id: lease.id,
                revision: lease.revision,
            },
            &config,
            None,
        )
        .unwrap();
    assert!(store.queue(10).unwrap().is_empty());
    assert!(store.paused().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_and_draft_receipts_replay_after_reopen_with_lowered_limits_without_resume_or_changes()
 {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store.apply("stop", &Command::Stop, &config, None).unwrap();
    store
        .apply(
            "first",
            &accumulated("first", 0, true, false),
            &config,
            None,
        )
        .unwrap();
    let item = store.queue(10).unwrap().remove(0);
    let Reply::Editing(lease) = store
        .apply(
            "lease",
            &Command::BeginEdit {
                id: item.id,
                revision: item.revision,
            },
            &config,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    let finish = Command::FinishPendingEdit {
        id: lease.id,
        revision: lease.revision,
        text: "replacement".into(),
        expected_paused: true,
    };
    store.apply("finish", &finish, &config, None).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    let Reply::Draft(draft) = store
        .apply(
            "draft",
            &Command::SaveDraft {
                text: "//more".into(),
            },
            &config,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    let send = Command::DispatchDraftAccumulated {
        id: draft.id,
        revision: draft.revision,
        expected_paused: true,
    };
    store.apply("send", &send, &config, None).unwrap();
    store
        .apply(
            "unrelated",
            &Command::SaveComposer {
                text: "new".into(),
                cursor: 2,
                revision: 1,
            },
            &config,
            None,
        )
        .unwrap();
    drop(store);
    let mut changed = config.clone();
    changed.limits.message_bytes = 4;
    let client = Client::start(&path, changed).await.unwrap();
    let before = client.snapshot();
    for (id, command) in [("finish", finish), ("send", send)] {
        for _ in 0..2 {
            assert!(matches!(
                client
                    .command_with_id(id.into(), command.clone())
                    .await
                    .unwrap(),
                Reply::Ack
            ));
        }
        assert!(
            client
                .command_with_id(format!("new-{id}"), command)
                .await
                .is_err()
        );
    }
    assert_eq!(client.snapshot().cursor, before.cursor);
    assert_eq!(
        serde_json::to_value(client.snapshot().queue).unwrap(),
        serde_json::to_value(before.queue).unwrap()
    );
    assert_eq!(client.snapshot().queue[0].text, "replacement\n\n/more");
    assert!(client.snapshot().paused);
    assert!(client.snapshot().active.is_none());
    assert!(
        matches!(client.command(Command::Composer).await.unwrap(), Reply::Composer(state) if state.text == "new" && state.revision == 2)
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_send_and_finish_pending_edit_wait_for_full_generation_after_commit() {
    for (from_edit, pause_before) in [(false, true), (true, true), (false, false), (true, false)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.first_event_ms = 0;
        config.provider.delay_ms = 150;
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
        timeout(Duration::from_secs(5), async {
            loop {
                let Reply::Events(events) = client
                    .command(Command::Events {
                        after: 0,
                        limit: 200,
                    })
                    .await
                    .unwrap()
                else {
                    panic!()
                };
                if events.iter().any(|e| e.kind == "fixture_tool_started") {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if pause_before {
            client.command(Command::Pause).await.unwrap();
        }
        client
            .command(Command::Send {
                text: "held".into(),
            })
            .await
            .unwrap();
        let command = if from_edit {
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
            Command::FinishPendingEdit {
                id: lease.id,
                revision: lease.revision,
                text: "replacement".into(),
                expected_paused: pause_before,
            }
        } else {
            let Reply::Draft(draft) = client
                .command(Command::SaveDraft {
                    text: "//draft".into(),
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            Command::DispatchDraftAccumulated {
                id: draft.id,
                revision: draft.revision,
                expected_paused: pause_before,
            }
        };
        let cursor = client.snapshot().cursor;
        let mut stale = command.clone();
        match &mut stale {
            Command::FinishPendingEdit {
                expected_paused, ..
            }
            | Command::DispatchDraftAccumulated {
                expected_paused, ..
            } => *expected_paused = !pause_before,
            _ => unreachable!(),
        }
        assert!(client.command(stale).await.is_err());
        assert_eq!(client.snapshot().paused, pause_before);
        assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
        assert_eq!(client.snapshot().cursor, cursor);
        assert!(matches!(
            client
                .command_with_id("deliver".into(), command.clone())
                .await
                .unwrap(),
            Reply::Ack
        ));
        assert!(!client.snapshot().paused);
        assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
        wait(&client, |s| {
            s.active.as_ref().is_some_and(|a| a.attempt_id != first)
        })
        .await;
        let next = client.snapshot().active.unwrap().attempt_id;
        let Reply::History(history) = client
            .command(Command::History {
                before: None,
                limit: 200,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(history[1].status, "completed");
        assert!(history[1].text.ends_with("Streaming chunk 23. "));
        assert_eq!(
            history[2].text,
            if from_edit {
                "replacement"
            } else {
                "held\n\n/draft"
            }
        );
        client.command(Command::Pause).await.unwrap();
        let cursor = client.snapshot().cursor;
        client
            .command_with_id("deliver".into(), command)
            .await
            .unwrap();
        assert!(client.snapshot().paused);
        assert_eq!(client.snapshot().active.unwrap().attempt_id, next);
        assert_eq!(client.snapshot().cursor, cursor);
        client.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_after_append_does_not_steer_but_explicit_legacy_steer_still_does() {
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
    client.command(Command::Pause).await.unwrap();
    client
        .command(accumulated("one", 0, true, false))
        .await
        .unwrap();
    client
        .command(accumulated("two", 1, true, false))
        .await
        .unwrap();
    sleep(Duration::from_millis(50)).await;
    assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
    assert!(client.snapshot().paused);
    let mut command = accumulated("three", 2, true, true);
    if let Command::SubmitInputAccumulated { mode, .. } = &mut command {
        *mode = DeliveryMode::Steer;
    }
    client
        .command_with_id("resume-append".into(), command.clone())
        .await
        .unwrap();
    let next = wait(&client, |s| {
        s.active.as_ref().is_some_and(|a| a.attempt_id != first)
    })
    .await
    .active
    .unwrap()
    .attempt_id;
    client.command(Command::Pause).await.unwrap();
    let cursor = client.snapshot().cursor;
    client
        .command_with_id("resume-append".into(), command)
        .await
        .unwrap();
    sleep(Duration::from_millis(50)).await;
    assert!(client.snapshot().paused);
    assert_eq!(client.snapshot().active.unwrap().attempt_id, next);
    assert_eq!(client.snapshot().cursor, cursor);
    let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(history[1].status, "steered");
    assert_eq!(history[2].text, "one\n\ntwo\n\nthree");
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accumulated_force_cancels_and_resumes_only_after_successful_checked_commit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Waiting;
    config.limits.queue_messages = 1;
    config.limits.message_bytes = 20;
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
    client.command(Command::Pause).await.unwrap();
    client
        .command(accumulated("held", 0, true, false))
        .await
        .unwrap();
    client
        .command(Command::SaveComposer {
            text: "retained".into(),
            cursor: 3,
            revision: 1,
        })
        .await
        .unwrap();
    let before = client.snapshot();
    for (source, revision, paused) in [
        ("next", 1, true),
        ("next", 2, false),
        ("123456789012345", 2, true),
    ] {
        let mut command = accumulated(source, revision, paused, true);
        if let Command::SubmitInputAccumulated { mode, .. } = &mut command {
            *mode = DeliveryMode::Force;
        }
        assert!(client.command(command).await.is_err());
        assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
        assert!(client.snapshot().paused);
        assert_eq!(client.snapshot().cursor, before.cursor);
        assert_eq!(client.snapshot().queue[0].text, "held");
        assert!(
            matches!(client.command(Command::Composer).await.unwrap(), Reply::Composer(state) if state.text == "retained" && state.revision == 2)
        );
    }
    let mut command = accumulated("next", 2, true, true);
    if let Command::SubmitInputAccumulated { mode, .. } = &mut command {
        *mode = DeliveryMode::Force;
    }
    assert!(matches!(client.command(command).await.unwrap(), Reply::Ack));
    assert!(!client.snapshot().paused);
    wait(&client, |s| {
        s.active.as_ref().is_some_and(|a| a.attempt_id != first)
    })
    .await;
    let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(history[1].status, "cancelled");
    assert_eq!(history[2].text, "held\n\nnext");
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_artifact_quotas_reject_append_draft_and_edit_without_consumption_resume_or_cancellation()
 {
    for frozen_exhausted in [true, false] {
        for active in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.sqlite3");
            let mut original = Config::default();
            original.provider.scenario = Scenario::Waiting;
            original.limits.output_bytes = 1024 * 1024;
            original.limits.artifact_bytes = 1024 * 1024;
            let mut larger = original.clone();
            larger.limits.artifact_bytes *= 2;
            let pending_config = if frozen_exhausted { &original } else { &larger };
            let reopened_config = if frozen_exhausted { &larger } else { &original };
            let mut store = Store::open(&path, &original).unwrap();
            store
                .apply(
                    "output",
                    &Command::Send {
                        text: "output".into(),
                    },
                    &original,
                    None,
                )
                .unwrap();
            let output_attempt = store.start_next(&original).unwrap().unwrap().attempt_id;
            // This FIFO predecessor can still run under its larger frozen quota.
            store
                .apply(
                    "active",
                    &Command::Send {
                        text: "active".into(),
                    },
                    &larger,
                    None,
                )
                .unwrap();
            store
                .apply(
                    "pending",
                    &Command::Send {
                        text: "pending".into(),
                    },
                    pending_config,
                    None,
                )
                .unwrap();
            store
                .apply(
                    "composer",
                    &Command::SaveComposer {
                        text: "retained composer".into(),
                        cursor: 3,
                        revision: 0,
                    },
                    &original,
                    None,
                )
                .unwrap();
            let Reply::Draft(draft) = store
                .apply(
                    "draft",
                    &Command::SaveDraft {
                        text: "//retained draft".into(),
                    },
                    &original,
                    None,
                )
                .unwrap()
            else {
                panic!()
            };
            store
                .append_output(
                    &output_attempt,
                    &"x".repeat(original.limits.output_bytes),
                    &original.limits,
                )
                .unwrap();
            store
                .finish(Completion {
                    attempt_id: &output_attempt,
                    text: "fixture",
                    status: "completed",
                })
                .unwrap();
            store
                .apply("stop", &Command::Stop, &original, None)
                .unwrap();
            drop(store);

            let client = Client::start(&path, reopened_config.clone()).await.unwrap();
            if active {
                client.command(Command::Resume).await.unwrap();
                wait(&client, |s| s.active.is_some()).await;
                client.command(Command::Pause).await.unwrap();
            }
            let before = client.snapshot();
            assert!(before.paused);
            assert_eq!(before.active.is_some(), active);
            let db = Connection::open(&path).unwrap();
            let artifact_bytes: i64 = db
                .query_row("SELECT SUM(bytes) FROM artifacts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(artifact_bytes as u64, original.limits.artifact_bytes);
            if frozen_exhausted {
                assert!((artifact_bytes as u64) < reopened_config.limits.artifact_bytes);
            } else {
                assert!((artifact_bytes as u64) < pending_config.limits.artifact_bytes);
            }
            let Reply::Composer(composer) = client.command(Command::Composer).await.unwrap() else {
                panic!()
            };
            let pending = before.queue.last().unwrap().clone();
            let frozen: String = db
                .query_row(
                    "SELECT config FROM dispatch_configs WHERE message_id=?1",
                    [&pending.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                frozen,
                toml::to_string(&pending_config.attempt_config()).unwrap()
            );
            let Reply::Actions(actions) = client
                .command(Command::Actions {
                    before: None,
                    limit: 200,
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            let mut forced = accumulated("//extra", composer.revision, true, true);
            if let Command::SubmitInputAccumulated { mode, .. } = &mut forced {
                *mode = DeliveryMode::Force;
            }
            for (id, command) in [
                (
                    "reject-steer",
                    accumulated("//extra", composer.revision, true, true),
                ),
                ("reject-force", forced),
                (
                    "reject-draft",
                    Command::DispatchDraftAccumulated {
                        id: draft.id.clone(),
                        revision: draft.revision,
                        expected_paused: true,
                    },
                ),
            ] {
                assert!(
                    client
                        .command_with_id(id.into(), command)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("artifact session quota exhausted"),
                    "frozen_exhausted={frozen_exhausted}, active={active}, id={id}"
                );
                sleep(Duration::from_millis(30)).await;
                let after = client.snapshot();
                assert!(after.paused);
                assert_eq!(after.phase, before.phase);
                assert_eq!(after.error, before.error);
                assert_eq!(after.cursor, before.cursor);
                assert_eq!(
                    after.active.as_ref().map(|a| &a.attempt_id),
                    before.active.as_ref().map(|a| &a.attempt_id)
                );
                assert_eq!(
                    serde_json::to_value(after.queue).unwrap(),
                    serde_json::to_value(&before.queue).unwrap()
                );
                assert!(
                    matches!(client.command(Command::Composer).await.unwrap(), Reply::Composer(state) if state == composer)
                );
                assert!(
                    matches!(client.command(Command::Drafts).await.unwrap(), Reply::Drafts(rows) if rows == [draft.clone()])
                );
                assert!(
                    matches!(client.command(Command::Actions { before: None, limit: 200 }).await.unwrap(), Reply::Actions(rows) if rows == actions)
                );
                assert_eq!(
                    db.query_row::<i64, _, _>(
                        "SELECT COUNT(*) FROM command_results WHERE id=?1",
                        [id],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    0
                );
            }

            let Reply::Editing(lease) = client
                .command(Command::BeginEdit {
                    id: pending.id.clone(),
                    revision: pending.revision,
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            let leased = client.snapshot();
            let finish = Command::FinishPendingEdit {
                id: lease.id.clone(),
                revision: lease.revision,
                text: "replacement".into(),
                expected_paused: true,
            };
            assert!(
                client
                    .command_with_id("reject-edit".into(), finish)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("artifact session quota exhausted")
            );
            sleep(Duration::from_millis(30)).await;
            let after = client.snapshot();
            assert!(after.paused);
            assert_eq!(after.phase, leased.phase);
            assert_eq!(after.error, leased.error);
            assert_eq!(after.cursor, leased.cursor);
            assert_eq!(
                after.active.as_ref().map(|a| &a.attempt_id),
                leased.active.as_ref().map(|a| &a.attempt_id)
            );
            assert_eq!(
                serde_json::to_value(after.queue).unwrap(),
                serde_json::to_value(&leased.queue).unwrap()
            );
            assert!(
                matches!(client.command(Command::Composer).await.unwrap(), Reply::Composer(state) if state == composer)
            );
            assert!(
                matches!(client.command(Command::Drafts).await.unwrap(), Reply::Drafts(rows) if rows == [draft])
            );
            assert!(
                matches!(client.command(Command::Actions { before: None, limit: 200 }).await.unwrap(), Reply::Actions(rows) if rows == actions)
            );
            assert_eq!(
                db.query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM command_results WHERE id='reject-edit'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT config FROM dispatch_configs WHERE message_id=?1",
                    [&pending.id],
                    |r| r.get(0)
                )
                .unwrap(),
                frozen
            );
            if let Some(attempt) = before.active {
                assert_eq!(
                    db.query_row::<String, _, _>(
                        "SELECT status FROM attempts WHERE id=?1",
                        [&attempt.attempt_id],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    "running"
                );
            }
            client.shutdown().await.unwrap();
        }
    }
}

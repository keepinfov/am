use am::{
    config::{Config, Scenario},
    runtime::{Client, Command, DeliveryMode, Reply, Snapshot},
    storage::{Completion, Store},
};
use std::time::Duration;
use tokio::time::{sleep, timeout};

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

fn submit(text: &str, mode: DeliveryMode, resume: bool) -> Command {
    Command::Submit {
        text: text.into(),
        mode,
        resume,
    }
}

async fn statuses(client: &Client) -> Vec<String> {
    let Reply::History(rows) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    rows.into_iter()
        .filter(|r| r.kind == "assistant")
        .map(|r| r.status)
        .collect()
}

#[test]
fn modes_have_locked_wire_names_and_dispatch_configuration_is_frozen() {
    for (mode, name) in [
        (DeliveryMode::Steer, "steer"),
        (DeliveryMode::After, "after"),
        (DeliveryMode::Force, "force"),
    ] {
        assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{name}\""));
        assert_eq!(
            serde_json::from_str::<DeliveryMode>(&format!("\"{name}\"")).unwrap(),
            mode
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.first_event_ms = 543;
    config.provider.scenario = Scenario::Burst;
    config.limits.response_bytes = 123;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "send",
            &Command::Send {
                text: "frozen".into(),
            },
            &config,
            None,
        )
        .unwrap();
    drop(store);
    let changed = Config::default();
    let mut store = Store::open(&path, &changed).unwrap();
    store
        .apply("resume", &Command::Resume, &changed, None)
        .unwrap();
    let started = store.start_next(&changed).unwrap().unwrap();
    assert_eq!(started.config.provider.first_event_ms, 543);
    assert_eq!(started.config.provider.scenario, Scenario::Burst);
    assert_eq!(started.config.limits.response_bytes, 123);
    let db = rusqlite::Connection::open(&path).unwrap();
    let saved: String = db
        .query_row("SELECT config FROM attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(saved, toml::to_string(&config.attempt_config()).unwrap());
    assert!(!saved.contains("interface"));
}

#[test]
fn force_draft_dispatch_and_cancellation_share_one_durable_receipt_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "first",
            &Command::Send {
                text: "active".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let attempt = store.start_next(&config).unwrap().unwrap().attempt_id;
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    let Reply::Draft(draft) = store
        .apply(
            "draft",
            &Command::SaveDraft {
                text: "replacement".into(),
            },
            &config,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    let command = Command::DispatchDraft {
        id: draft.id.clone(),
        revision: draft.revision,
        mode: DeliveryMode::Force,
        resume: true,
    };
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_force BEFORE INSERT ON command_results WHEN NEW.id='force' BEGIN SELECT RAISE(ABORT,'force receipt fault'); END;").unwrap();
    let completion = || Completion {
        attempt_id: &attempt,
        text: "partial output",
        status: "cancelled",
    };
    let cursor = store.cursor().unwrap();
    assert!(
        store
            .apply("force", &command, &config, Some(completion()))
            .is_err()
    );
    assert!(store.paused().unwrap());
    assert!(store.queue(10).unwrap().is_empty());
    assert_eq!(store.drafts().unwrap(), [draft]);
    assert_eq!(store.cursor().unwrap(), cursor);
    assert!(store.previous("force", &command).unwrap().is_none());
    assert_eq!(
        db.query_row::<String, _, _>("SELECT status FROM attempts", [], |r| r.get(0))
            .unwrap(),
        "running"
    );
    assert_eq!(store.actions(None, 10).unwrap()[0].status, "running");
    db.execute_batch("DROP TRIGGER fail_force").unwrap();
    store
        .apply("force", &command, &config, Some(completion()))
        .unwrap();
    assert!(!store.paused().unwrap());
    assert!(store.drafts().unwrap().is_empty());
    assert_eq!(store.queue(10).unwrap()[0].text, "replacement");
    assert_eq!(store.history(None, 10).unwrap()[1].status, "cancelled");
    let action = &store.actions(None, 10).unwrap()[0];
    assert_eq!(action.status, "cancelled");
    assert!(action.output.as_ref().unwrap().incomplete);
    let cursor = store.cursor().unwrap();
    store
        .apply("force", &command, &config, Some(completion()))
        .unwrap();
    assert_eq!(store.cursor().unwrap(), cursor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_queue_runs_the_frozen_provider_and_response_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Burst;
    config.provider.first_event_ms = 0;
    config.limits.response_bytes = 67;
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client
        .command(submit("frozen dispatch", DeliveryMode::After, false))
        .await
        .unwrap();
    client.shutdown().await.unwrap();

    let mut changed = Config::default();
    changed.provider.scenario = Scenario::Waiting;
    changed.provider.first_event_ms = 60_000;
    let reopened = Client::start(&path, changed).await.unwrap();
    assert!(reopened.snapshot().paused);
    reopened.command(Command::Resume).await.unwrap();
    wait(&reopened, |s| s.active.is_none() && s.queue.is_empty()).await;
    let Reply::History(rows) = reopened
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows[1].status, "truncated");
    assert_eq!(rows[1].text.len(), 67);
    assert!(rows[1].text.starts_with("fixture block"));
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_preserves_active_work_fifo_and_paused_submit_needs_explicit_resume() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.first_event_ms = 0;
    config.provider.delay_ms = 10;
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client
        .command(submit("first", DeliveryMode::After, false))
        .await
        .unwrap();
    sleep(Duration::from_millis(50)).await;
    assert!(client.snapshot().active.is_none());
    assert!(client.snapshot().paused);
    client.command(Command::Resume).await.unwrap();
    let first = wait(&client, |s| s.active.is_some())
        .await
        .active
        .unwrap()
        .attempt_id;
    client
        .command(submit("second", DeliveryMode::After, false))
        .await
        .unwrap();
    client
        .command(submit("third", DeliveryMode::After, false))
        .await
        .unwrap();
    assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
    wait(&client, |s| s.active.is_none() && s.queue.is_empty()).await;
    assert_eq!(
        statuses(&client).await,
        ["completed", "completed", "completed"]
    );
    let Reply::History(rows) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        rows.iter()
            .filter(|r| r.kind == "user")
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_interrupts_generation_waiting_before_first_event() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Waiting;
    let client = Client::start(&path, config).await.unwrap();
    client
        .command(Command::Send {
            text: "first".into(),
        })
        .await
        .unwrap();
    let first = wait(&client, |s| s.active.is_some())
        .await
        .active
        .unwrap()
        .attempt_id;
    let steering = submit("next", DeliveryMode::Steer, false);
    client
        .command_with_id("steering".into(), steering.clone())
        .await
        .unwrap();
    let next = wait(&client, |s| {
        s.active.as_ref().is_some_and(|a| a.attempt_id != first)
    })
    .await
    .active
    .unwrap()
    .attempt_id;
    assert_eq!(statuses(&client).await, ["steered"]);
    client
        .command_with_id("steering".into(), steering)
        .await
        .unwrap();
    sleep(Duration::from_millis(50)).await;
    assert_eq!(client.snapshot().active.unwrap().attempt_id, next);
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_waits_for_simulated_tool_but_force_does_not() {
    for mode in [DeliveryMode::Steer, DeliveryMode::Force] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let mut config = Config::default();
        config.provider.first_event_ms = 0;
        config.provider.delay_ms = 350;
        let client = Client::start(&path, config).await.unwrap();
        client
            .command(Command::Send {
                text: "first".into(),
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
        client.command(submit("next", mode, false)).await.unwrap();
        if mode == DeliveryMode::Steer {
            sleep(Duration::from_millis(80)).await;
            assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
        }
        timeout(
            Duration::from_millis(if mode == DeliveryMode::Force {
                150
            } else {
                600
            }),
            wait(&client, |s| {
                s.active.as_ref().is_some_and(|a| a.attempt_id != first)
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            statuses(&client).await,
            [if mode == DeliveryMode::Steer {
                "steered"
            } else {
                "cancelled"
            }]
        );
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
        assert_eq!(events.iter().any(|e| e.kind=="fixture_tool_finished" && e.attempt_id.as_ref()==Some(&first)), mode==DeliveryMode::Steer);
        client.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_force_dispatch_does_not_cancel_or_resume() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Waiting;
    config.limits.queue_messages = 1;
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
        .command(submit("full queue", DeliveryMode::After, false))
        .await
        .unwrap();
    let Reply::Draft(saved) = client
        .command(Command::SaveDraft {
            text: "draft".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    for command in [
        submit("rejected", DeliveryMode::Force, true),
        Command::DispatchDraft {
            id: saved.id,
            revision: 0,
            mode: DeliveryMode::Force,
            resume: true,
        },
    ] {
        assert!(client.command(command).await.is_err());
        assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
        assert!(client.snapshot().paused);
        assert!(statuses(&client).await.is_empty());
    }
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lowered_message_limit_allows_exact_receipt_replay_but_not_new_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.message_bytes = 64;
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    let mut receipts = Vec::new();
    let send = Command::Send {
        text: "x".repeat(40),
    };
    let reply = client
        .command_with_id("send".into(), send.clone())
        .await
        .unwrap();
    receipts.push(("send", send, reply));
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
    let finish = Command::FinishEdit {
        id: lease.id,
        revision: lease.revision,
        text: "y".repeat(45),
    };
    let reply = client
        .command_with_id("finish".into(), finish.clone())
        .await
        .unwrap();
    receipts.push(("finish", finish, reply));
    let save_draft = Command::SaveDraft {
        text: "\u{e9}".repeat(25),
    };
    let reply = client
        .command_with_id("draft".into(), save_draft.clone())
        .await
        .unwrap();
    let Reply::Draft(saved) = &reply else {
        panic!()
    };
    client
        .command(Command::DeleteDraft {
            id: saved.id.clone(),
            revision: saved.revision,
        })
        .await
        .unwrap();
    receipts.push(("draft", save_draft, reply));
    let save_composer = Command::SaveComposer {
        text: "z".repeat(60),
        cursor: 60,
        revision: 0,
    };
    let reply = client
        .command_with_id("composer".into(), save_composer.clone())
        .await
        .unwrap();
    receipts.push(("composer", save_composer, reply));
    let submitted = submit(&"z".repeat(60), DeliveryMode::After, false);
    let reply = client
        .command_with_id("submit".into(), submitted.clone())
        .await
        .unwrap();
    receipts.push(("submit", submitted, reply));
    client
        .command(Command::SaveComposer {
            text: "keep".into(),
            cursor: 4,
            revision: 2,
        })
        .await
        .unwrap();
    client.shutdown().await.unwrap();

    let mut changed = Config::default();
    changed.limits.message_bytes = 8;
    let reopened = Client::start(&path, changed).await.unwrap();
    let before = reopened.snapshot();
    for (id, command, reply) in receipts {
        let replay = reopened.command_with_id(id.into(), command).await.unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(reply).unwrap()
        );
    }
    for command in [
        Command::Send {
            text: "x".repeat(9),
        },
        submit(&"x".repeat(9), DeliveryMode::Force, true),
        Command::SaveDraft {
            text: "x".repeat(9),
        },
        Command::SaveComposer {
            text: "x".repeat(9),
            cursor: 9,
            revision: 3,
        },
    ] {
        assert!(reopened.command(command).await.is_err());
    }
    assert_eq!(reopened.snapshot().cursor, before.cursor);
    assert_eq!(
        serde_json::to_value(reopened.snapshot().queue).unwrap(),
        serde_json::to_value(before.queue).unwrap()
    );
    assert!(reopened.snapshot().paused);
    assert!(
        matches!(reopened.command(Command::Drafts).await.unwrap(),Reply::Drafts(rows) if rows.is_empty())
    );
    assert!(
        matches!(reopened.command(Command::Composer).await.unwrap(),Reply::Composer(state) if state.text=="keep" && state.revision==3)
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_force_dispatch_cannot_cancel_a_later_attempt_or_resume_a_pause() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.provider.scenario = Scenario::Waiting;
    let client = Client::start(&path, config).await.unwrap();
    client
        .command(Command::Send {
            text: "first".into(),
        })
        .await
        .unwrap();
    let first = wait(&client, |s| s.active.is_some())
        .await
        .active
        .unwrap()
        .attempt_id;
    let forced = submit("replacement", DeliveryMode::Force, true);
    client
        .command_with_id("force".into(), forced.clone())
        .await
        .unwrap();
    let replacement = wait(&client, |s| {
        s.active.as_ref().is_some_and(|a| a.attempt_id != first)
    })
    .await
    .active
    .unwrap()
    .attempt_id;
    client.command(Command::Pause).await.unwrap();
    let before = client.snapshot();
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
    for _ in 0..2 {
        client
            .command_with_id("force".into(), forced.clone())
            .await
            .unwrap();
    }
    assert_eq!(client.snapshot().active.unwrap().attempt_id, replacement);
    assert!(client.snapshot().paused);
    assert_eq!(client.snapshot().cursor, before.cursor);
    assert!(
        matches!(client.command(Command::Actions { before:None,limit:200 }).await.unwrap(),Reply::Actions(rows) if rows==actions)
    );
    client.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_steer_and_force_dispatches_leave_active_generation_running() {
    for mode in [DeliveryMode::Steer, DeliveryMode::Force] {
        for from_draft in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.sqlite3");
            let mut config = Config::default();
            config.provider.first_event_ms = 0;
            config.provider.delay_ms = 20;
            let client = Client::start(&path, config).await.unwrap();
            client
                .command(Command::Send {
                    text: "active".into(),
                })
                .await
                .unwrap();
            let first = wait(&client, |s| {
                s.active.as_ref().is_some_and(|a| !a.text.is_empty())
            })
            .await
            .active
            .unwrap()
            .attempt_id;
            client.command(Command::Pause).await.unwrap();
            let held = if from_draft {
                let Reply::Draft(draft) = client
                    .command(Command::SaveDraft {
                        text: "held".into(),
                    })
                    .await
                    .unwrap()
                else {
                    panic!()
                };
                Command::DispatchDraft {
                    id: draft.id,
                    revision: draft.revision,
                    mode,
                    resume: false,
                }
            } else {
                submit("held", mode, false)
            };
            let before_bytes = client.snapshot().active.unwrap().text.len();
            client
                .command_with_id("hold".into(), held.clone())
                .await
                .unwrap();
            assert!(client.snapshot().paused);
            assert_eq!(client.snapshot().active.unwrap().attempt_id, first);
            assert_eq!(client.snapshot().queue[0].text, "held");
            let progressing = wait(&client, |s| {
                s.active
                    .as_ref()
                    .is_none_or(|a| a.attempt_id != first || a.text.len() > before_bytes)
            })
            .await;
            assert_eq!(progressing.active.as_ref().unwrap().attempt_id, first);
            assert!(progressing.active.unwrap().text.len() > before_bytes);
            wait(&client, |s| s.active.is_none()).await;
            assert_eq!(statuses(&client).await, ["completed"]);
            let queued_id = client.snapshot().queue[0].id.clone();
            sleep(Duration::from_millis(50)).await;
            assert!(client.snapshot().paused);
            assert!(client.snapshot().active.is_none());
            assert_eq!(client.snapshot().queue[0].id, queued_id);

            client.command(Command::Resume).await.unwrap();
            let next = wait(&client, |s| s.active.is_some())
                .await
                .active
                .unwrap()
                .attempt_id;
            client.command_with_id("hold".into(), held).await.unwrap();
            sleep(Duration::from_millis(50)).await;
            assert_eq!(client.snapshot().active.unwrap().attempt_id, next);
            assert!(client.snapshot().queue.is_empty());
            client.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_resume_enables_steer_and_force_from_a_paused_queue() {
    for mode in [DeliveryMode::Steer, DeliveryMode::Force] {
        for from_draft in [false, true] {
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
            let command = if from_draft {
                let Reply::Draft(draft) = client
                    .command(Command::SaveDraft {
                        text: "deliver".into(),
                    })
                    .await
                    .unwrap()
                else {
                    panic!()
                };
                Command::DispatchDraft {
                    id: draft.id,
                    revision: draft.revision,
                    mode,
                    resume: true,
                }
            } else {
                submit("deliver", mode, true)
            };
            client.command(command).await.unwrap();
            assert!(!client.snapshot().paused);
            wait(&client, |s| {
                s.active.as_ref().is_some_and(|a| a.attempt_id != first)
            })
            .await;
            assert_eq!(
                statuses(&client).await,
                [if mode == DeliveryMode::Steer {
                    "steered"
                } else {
                    "cancelled"
                }]
            );
            client.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checked_pause_conflicts_never_interrupt_or_consume_active_input() {
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
    client.command(Command::Pause).await.unwrap();
    client
        .command(Command::SaveComposer {
            text: "saved earlier".into(),
            cursor: 0,
            revision: 0,
        })
        .await
        .unwrap();
    let Reply::Draft(draft) = client
        .command(Command::SaveDraft {
            text: "//draft".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    // A pending resume won admission before the stale Hold choice reached storage.
    client.command(Command::Resume).await.unwrap();
    let cursor = client.snapshot().cursor;
    for mode in [DeliveryMode::Steer, DeliveryMode::Force] {
        for resume in [false, true] {
            for command in [
                Command::SubmitInput {
                    source: "//recent editor".into(),
                    text: "/recent editor".into(),
                    composer_revision: 1,
                    mode,
                    resume,
                    expected_paused: true,
                },
                Command::DispatchDraftChecked {
                    id: draft.id.clone(),
                    revision: draft.revision,
                    mode,
                    resume,
                    expected_paused: true,
                },
            ] {
                assert!(
                    client
                        .command(command)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("paused state conflict")
                );
                assert_eq!(client.snapshot().active.unwrap().attempt_id, attempt);
                assert!(!client.snapshot().paused);
                assert!(client.snapshot().queue.is_empty());
                assert_eq!(client.snapshot().cursor, cursor);
                assert!(
                    matches!(client.command(Command::Composer).await.unwrap(), Reply::Composer(state) if state.text=="saved earlier" && state.revision==1)
                );
                assert!(
                    matches!(client.command(Command::Drafts).await.unwrap(), Reply::Drafts(rows) if rows==std::slice::from_ref(&draft))
                );
            }
        }
    }
    client.shutdown().await.unwrap();
}

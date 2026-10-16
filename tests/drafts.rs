use am::{
    config::Config,
    runtime::{Client, Command, ComposerState, DeliveryMode, DraftRecord, Reply},
    storage::Store,
};
use rusqlite::Connection;

fn draft(reply: Reply) -> DraftRecord {
    let Reply::Draft(draft) = reply else {
        panic!("expected draft")
    };
    draft
}

fn composer(reply: Reply) -> ComposerState {
    let Reply::Composer(state) = reply else {
        panic!("expected composer")
    };
    state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drafts_and_composer_are_independent_durable_and_optimistic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let client = Client::start(&path, config.clone()).await.unwrap();
    let saved = draft(
        client
            .command(Command::SaveDraft {
                text: "explicit draft".into(),
            })
            .await
            .unwrap(),
    );
    let command = Command::SaveComposer {
        text: "unfinished input".into(),
        cursor: 10,
        revision: 0,
    };
    let state = composer(
        client
            .command_with_id("autosave".into(), command.clone())
            .await
            .unwrap(),
    );
    assert_eq!(state.revision, 1);
    assert!(client.command(command.clone()).await.is_err());
    assert!(
        client
            .command(Command::SaveComposer {
                text: "new".into(),
                cursor: 4,
                revision: 1
            })
            .await
            .is_err()
    );
    assert!(
        client
            .command(Command::SaveComposer {
                text: "\u{e9}".into(),
                cursor: 1,
                revision: 1
            })
            .await
            .is_err()
    );
    assert!(
        client
            .command(Command::DeleteDraft {
                id: saved.id.clone(),
                revision: saved.revision + 1
            })
            .await
            .is_err()
    );
    assert!(client.snapshot().queue.is_empty());
    assert!(client.snapshot().active.is_none());
    client.shutdown().await.unwrap();
    let reopened = Client::start(&path, config).await.unwrap();
    assert!(reopened.snapshot().paused);
    assert_eq!(
        composer(reopened.command(Command::Composer).await.unwrap()),
        state
    );
    assert_eq!(
        composer(
            reopened
                .command_with_id("autosave".into(), command)
                .await
                .unwrap()
        ),
        state
    );
    assert!(
        matches!(reopened.command(Command::Drafts).await.unwrap(), Reply::Drafts(rows) if rows == [saved.clone()])
    );
    reopened
        .command(Command::DeleteDraft {
            id: saved.id,
            revision: saved.revision,
        })
        .await
        .unwrap();
    assert!(
        matches!(reopened.command(Command::Drafts).await.unwrap(), Reply::Drafts(rows) if rows.is_empty())
    );
    reopened.shutdown().await.unwrap();
}

#[test]
fn draft_dispatch_rolls_back_every_side_effect_and_replays_after_reopen() {
    for failure in ["messages", "command_results"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply("pause", &Command::Pause, &config, None)
            .unwrap();
        let saved = draft(
            store
                .apply(
                    "draft",
                    &Command::SaveDraft {
                        text: "dispatch me".into(),
                    },
                    &config,
                    None,
                )
                .unwrap(),
        );
        let command = Command::DispatchDraft {
            id: saved.id.clone(),
            revision: saved.revision,
            mode: DeliveryMode::After,
            resume: true,
        };
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_dispatch BEFORE INSERT ON {failure} BEGIN SELECT RAISE(ABORT,'dispatch fault'); END;")).unwrap();
        let cursor = store.cursor().unwrap();
        assert!(store.apply("dispatch", &command, &config, None).is_err());
        assert_eq!(store.drafts().unwrap(), std::slice::from_ref(&saved));
        assert!(store.queue(10).unwrap().is_empty());
        assert!(store.paused().unwrap());
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(store.previous("dispatch", &command).unwrap().is_none());
        db.execute_batch("DROP TRIGGER fail_dispatch").unwrap();
        store.apply("dispatch", &command, &config, None).unwrap();
        assert!(!store.paused().unwrap());
        assert!(store.drafts().unwrap().is_empty());
        assert_eq!(store.queue(10).unwrap()[0].text, saved.text);
        let cursor = store.cursor().unwrap();
        store.apply("dispatch", &command, &config, None).unwrap();
        assert_eq!(store.cursor().unwrap(), cursor);
        drop(store);
        let mut reopened = Store::open(&path, &config).unwrap();
        let cursor = reopened.cursor().unwrap();
        reopened.apply("dispatch", &command, &config, None).unwrap();
        assert!(
            reopened.paused().unwrap(),
            "receipt replay must not resume recovery"
        );
        assert_eq!(reopened.cursor().unwrap(), cursor);
        assert_eq!(reopened.queue(10).unwrap().len(), 1);
        assert!(
            reopened
                .apply("another-dispatch", &command, &config, None)
                .is_err()
        );
    }
}

#[test]
fn drafts_have_a_distinct_count_limit_and_failed_dispatch_keeps_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.queue_messages = 1;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    store
        .apply(
            "send",
            &Command::Send {
                text: "full queue".into(),
            },
            &config,
            None,
        )
        .unwrap();
    let saved = draft(
        store
            .apply(
                "draft",
                &Command::SaveDraft {
                    text: "independent slot".into(),
                },
                &config,
                None,
            )
            .unwrap(),
    );
    assert!(
        store
            .apply(
                "overflow",
                &Command::SaveDraft {
                    text: "second".into()
                },
                &config,
                None
            )
            .is_err()
    );
    for revision in [saved.revision, saved.revision + 1] {
        assert!(
            store
                .apply(
                    "dispatch",
                    &Command::DispatchDraft {
                        id: saved.id.clone(),
                        revision,
                        mode: DeliveryMode::Force,
                        resume: true
                    },
                    &config,
                    None
                )
                .is_err()
        );
        assert_eq!(store.drafts().unwrap(), std::slice::from_ref(&saved));
        assert!(store.paused().unwrap());
    }
}

#[test]
fn submit_clears_only_matching_composer_atomically_with_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "save",
            &Command::SaveComposer {
                text: "saved".into(),
                cursor: 5,
                revision: 0,
            },
            &config,
            None,
        )
        .unwrap();
    store
        .apply(
            "unrelated",
            &Command::Submit {
                text: "other".into(),
                mode: DeliveryMode::After,
                resume: false,
            },
            &config,
            None,
        )
        .unwrap();
    assert_eq!(store.composer().unwrap().text, "saved");
    assert_eq!(store.composer().unwrap().revision, 1);
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON command_results WHEN NEW.id='submit' BEGIN SELECT RAISE(ABORT,'receipt fault'); END;").unwrap();
    let submit = Command::Submit {
        text: "saved".into(),
        mode: DeliveryMode::After,
        resume: true,
    };
    assert!(store.apply("submit", &submit, &config, None).is_err());
    assert_eq!(store.composer().unwrap().text, "saved");
    assert_eq!(store.queue(10).unwrap().len(), 1);
    db.execute_batch("DROP TRIGGER fail_receipt").unwrap();
    store.apply("submit", &submit, &config, None).unwrap();
    assert_eq!(
        store.composer().unwrap(),
        ComposerState {
            text: String::new(),
            cursor: 0,
            revision: 2
        }
    );
    store
        .apply(
            "new-save",
            &Command::SaveComposer {
                text: "new saved input".into(),
                cursor: 0,
                revision: 2,
            },
            &config,
            None,
        )
        .unwrap();
    store.apply("submit", &submit, &config, None).unwrap();
    assert_eq!(store.composer().unwrap().text, "new saved input");
    assert_eq!(store.queue(10).unwrap().len(), 2);
}

#[test]
fn checked_submit_consumes_unsaved_escaped_editor_source_and_replays_after_crash() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "old-autosave",
            &Command::SaveComposer {
                text: "old saved text".into(),
                cursor: 4,
                revision: 0,
            },
            &config,
            None,
        )
        .unwrap();
    let command = Command::SubmitInput {
        source: "//recent editor text".into(),
        text: "/recent editor text".into(),
        composer_revision: 1,
        mode: DeliveryMode::After,
        resume: false,
        expected_paused: false,
    };
    assert!(matches!(
        store.apply("submit", &command, &config, None).unwrap(),
        Reply::Ack
    ));
    assert_eq!(store.queue(10).unwrap()[0].text, "/recent editor text");
    assert_eq!(
        store.composer().unwrap(),
        ComposerState {
            text: String::new(),
            cursor: 0,
            revision: 2
        }
    );
    let db = Connection::open(&path).unwrap();
    let frozen: String = db
        .query_row("SELECT config FROM dispatch_configs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(frozen, toml::to_string(&config.attempt_config()).unwrap());
    // No graceful shutdown or intervening autosave: dispatch is the sole receipt.
    drop(store);
    let mut reopened = Store::open(&path, &config).unwrap();
    assert!(reopened.paused().unwrap());
    assert_eq!(reopened.composer().unwrap().text, "");
    reopened
        .apply(
            "new-autosave",
            &Command::SaveComposer {
                text: "unrelated new input".into(),
                cursor: 0,
                revision: 2,
            },
            &config,
            None,
        )
        .unwrap();
    let state = reopened.composer().unwrap();
    let cursor = reopened.cursor().unwrap();
    reopened.apply("submit", &command, &config, None).unwrap();
    assert_eq!(reopened.composer().unwrap(), state);
    assert_eq!(reopened.cursor().unwrap(), cursor);
    assert!(reopened.paused().unwrap());
    assert_eq!(reopened.queue(10).unwrap().len(), 1);
    assert!(
        reopened
            .apply("new-submit", &command, &config, None)
            .is_err()
    );
    assert_eq!(reopened.composer().unwrap(), state);
}

#[test]
fn checked_submit_rolls_back_composer_queue_config_resume_and_receipt_together() {
    for table in [
        "messages",
        "dispatch_configs",
        "composer",
        "command_results",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply("pause", &Command::Pause, &config, None)
            .unwrap();
        store
            .apply(
                "autosave",
                &Command::SaveComposer {
                    text: "saved earlier".into(),
                    cursor: 3,
                    revision: 0,
                },
                &config,
                None,
            )
            .unwrap();
        let command = Command::SubmitInput {
            source: "//new editor input".into(),
            text: "/new editor input".into(),
            composer_revision: 1,
            mode: DeliveryMode::Force,
            resume: true,
            expected_paused: true,
        };
        let db = Connection::open(&path).unwrap();
        let operation = if table == "composer" {
            "UPDATE"
        } else {
            "INSERT"
        };
        db.execute_batch(&format!("CREATE TRIGGER fail_checked BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'checked submit fault'); END;")).unwrap();
        let before = store.composer().unwrap();
        let cursor = store.cursor().unwrap();
        assert!(store.apply("checked", &command, &config, None).is_err());
        assert_eq!(store.composer().unwrap(), before);
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(store.queue(10).unwrap().is_empty());
        assert!(store.paused().unwrap());
        assert!(store.previous("checked", &command).unwrap().is_none());
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT COUNT(*) FROM dispatch_configs", [], |r| r.get(0))
                .unwrap(),
            0
        );
        db.execute_batch("DROP TRIGGER fail_checked").unwrap();
        store.apply("checked", &command, &config, None).unwrap();
        assert!(!store.paused().unwrap());
        assert_eq!(store.composer().unwrap().revision, 2);
        assert!(store.composer().unwrap().text.is_empty());
        assert_eq!(store.queue(10).unwrap()[0].text, "/new editor input");
    }
}

#[test]
fn checked_input_revision_payload_and_size_conflicts_preserve_unrelated_composer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.message_bytes = 32;
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply(
            "autosave",
            &Command::SaveComposer {
                text: "unrelated saved input".into(),
                cursor: 0,
                revision: 0,
            },
            &config,
            None,
        )
        .unwrap();
    let before = store.composer().unwrap();
    let cursor = store.cursor().unwrap();
    for (source, text, revision) in [
        ("new".into(), "new".into(), 0),
        ("//new".into(), "//new".into(), 1),
        ("new".into(), "/new".into(), 1),
        ("x".repeat(33), "x".repeat(33), 1),
        (
            "//".to_owned() + &"x".repeat(31),
            "/".to_owned() + &"x".repeat(31),
            1,
        ),
        (String::new(), String::new(), 1),
    ] {
        let command = Command::SubmitInput {
            source,
            text,
            composer_revision: revision,
            mode: DeliveryMode::After,
            resume: false,
            expected_paused: false,
        };
        assert!(store.apply("invalid", &command, &config, None).is_err());
        assert!(store.previous("invalid", &command).unwrap().is_none());
        assert_eq!(store.composer().unwrap(), before);
        assert_eq!(store.cursor().unwrap(), cursor);
        assert!(store.queue(10).unwrap().is_empty());
    }
    // Literal source mapping only, including single slashes and multiline input.
    for (index, source) in [
        "/pause",
        "///literal",
        "//first\n//second",
        " //not-at-start",
    ]
    .into_iter()
    .enumerate()
    {
        let payload = if source.starts_with("//") {
            &source[1..]
        } else {
            source
        };
        let command = Command::SubmitInput {
            source: source.into(),
            text: payload.into(),
            composer_revision: 1 + index as i64,
            mode: DeliveryMode::After,
            resume: false,
            expected_paused: false,
        };
        store
            .apply(&format!("literal-{index}"), &command, &config, None)
            .unwrap();
        assert_eq!(store.queue(10).unwrap()[index].text, payload);
    }
}

#[test]
fn checked_dispatch_strips_only_editor_escape_and_legacy_drafts_remain_literal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    store
        .apply(
            "autosave",
            &Command::SaveComposer {
                text: "unrelated composer".into(),
                cursor: 0,
                revision: 0,
            },
            &config,
            None,
        )
        .unwrap();
    let before = store.composer().unwrap();
    for (index, source) in ["//pause", "///literal", "/pause", "//first\n//second"]
        .into_iter()
        .enumerate()
    {
        let saved = draft(
            store
                .apply(
                    &format!("draft-{index}"),
                    &Command::SaveDraft {
                        text: source.into(),
                    },
                    &config,
                    None,
                )
                .unwrap(),
        );
        let command = Command::DispatchDraftChecked {
            id: saved.id,
            revision: saved.revision,
            mode: DeliveryMode::After,
            resume: false,
            expected_paused: true,
        };
        store
            .apply(&format!("dispatch-{index}"), &command, &config, None)
            .unwrap();
    }
    let legacy = draft(
        store
            .apply(
                "legacy-draft",
                &Command::SaveDraft {
                    text: "//pause".into(),
                },
                &config,
                None,
            )
            .unwrap(),
    );
    store
        .apply(
            "legacy-dispatch",
            &Command::DispatchDraft {
                id: legacy.id,
                revision: legacy.revision,
                mode: DeliveryMode::After,
                resume: false,
            },
            &config,
            None,
        )
        .unwrap();
    assert_eq!(
        store
            .queue(10)
            .unwrap()
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>(),
        [
            "/pause",
            "//literal",
            "/pause",
            "/first\n//second",
            "//pause"
        ]
    );
    assert_eq!(store.composer().unwrap(), before);
    assert!(store.drafts().unwrap().is_empty());
    assert!(store.paused().unwrap());
}

#[test]
fn checked_hold_conflicts_with_a_pending_resume_without_consuming_input_or_draft() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    store
        .apply("pause", &Command::Pause, &config, None)
        .unwrap();
    store
        .apply(
            "autosave",
            &Command::SaveComposer {
                text: "old saved input".into(),
                cursor: 0,
                revision: 0,
            },
            &config,
            None,
        )
        .unwrap();
    let saved = draft(
        store
            .apply(
                "draft",
                &Command::SaveDraft {
                    text: "//draft".into(),
                },
                &config,
                None,
            )
            .unwrap(),
    );
    store
        .apply("pending-resume", &Command::Resume, &config, None)
        .unwrap();
    let before = store.composer().unwrap();
    let cursor = store.cursor().unwrap();
    for resume in [false, true] {
        for command in [
            Command::SubmitInput {
                source: "//recent edit".into(),
                text: "/recent edit".into(),
                composer_revision: 1,
                mode: DeliveryMode::Force,
                resume,
                expected_paused: true,
            },
            Command::DispatchDraftChecked {
                id: saved.id.clone(),
                revision: saved.revision,
                mode: DeliveryMode::Force,
                resume,
                expected_paused: true,
            },
        ] {
            assert!(
                store
                    .apply("hold", &command, &config, None)
                    .unwrap_err()
                    .to_string()
                    .contains("paused state conflict")
            );
            assert_eq!(store.composer().unwrap(), before);
            assert_eq!(store.drafts().unwrap(), std::slice::from_ref(&saved));
            assert!(store.queue(10).unwrap().is_empty());
            assert!(!store.paused().unwrap());
            assert_eq!(store.cursor().unwrap(), cursor);
            assert!(store.previous("hold", &command).unwrap().is_none());
        }
    }
    let command = Command::DispatchDraftChecked {
        id: saved.id.clone(),
        revision: saved.revision,
        mode: DeliveryMode::After,
        resume: false,
        expected_paused: false,
    };
    store.apply("deliver", &command, &config, None).unwrap();
    store
        .apply("pause-again", &Command::Pause, &config, None)
        .unwrap();
    let cursor = store.cursor().unwrap();
    store.apply("deliver", &command, &config, None).unwrap();
    assert!(store.paused().unwrap());
    assert_eq!(store.cursor().unwrap(), cursor);
    assert_eq!(store.queue(10).unwrap().len(), 1);
    assert!(store.drafts().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checked_submit_ack_clears_composer_and_replays_with_a_lowered_source_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.message_bytes = 64;
    let client = Client::start(&path, config).await.unwrap();
    client.command(Command::Pause).await.unwrap();
    client
        .command(Command::SaveComposer {
            text: "old saved text".into(),
            cursor: 0,
            revision: 0,
        })
        .await
        .unwrap();
    let source = format!("//{}", "\u{e9}".repeat(20));
    let payload = source[1..].to_owned();
    let command = Command::SubmitInput {
        source,
        text: payload.clone(),
        composer_revision: 1,
        mode: DeliveryMode::After,
        resume: false,
        expected_paused: true,
    };
    assert!(matches!(
        client
            .command_with_id("checked".into(), command.clone())
            .await
            .unwrap(),
        Reply::Ack
    ));
    assert_eq!(client.snapshot().queue[0].text, payload);
    assert_eq!(
        composer(client.command(Command::Composer).await.unwrap()),
        ComposerState {
            text: String::new(),
            cursor: 0,
            revision: 2
        }
    );
    client.shutdown().await.unwrap();

    let mut lowered = Config::default();
    lowered.limits.message_bytes = 8;
    let reopened = Client::start(&path, lowered).await.unwrap();
    reopened
        .command(Command::SaveComposer {
            text: "keep".into(),
            cursor: 4,
            revision: 2,
        })
        .await
        .unwrap();
    let state = composer(reopened.command(Command::Composer).await.unwrap());
    let cursor = reopened.snapshot().cursor;
    reopened
        .command_with_id("checked".into(), command.clone())
        .await
        .unwrap();
    assert_eq!(
        composer(reopened.command(Command::Composer).await.unwrap()),
        state
    );
    assert_eq!(reopened.snapshot().cursor, cursor);
    assert_eq!(reopened.snapshot().queue.len(), 1);
    assert!(reopened.snapshot().paused);
    assert!(reopened.command(command).await.is_err());
    assert_eq!(
        composer(reopened.command(Command::Composer).await.unwrap()),
        state
    );
    reopened.shutdown().await.unwrap();
}

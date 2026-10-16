use super::{
    completion::Stage,
    render::queue_id,
    state::native_end,
    transcript::{markdown_rows, message_rows, sanitized, tail, wrapped},
    *,
};
use crate::{
    config::{Config, StatusPreset},
    runtime::{MAX_NOTICE_BYTES, NoticeLevel, Phase, QueueItem, QueueState, Reply, Snapshot},
    settings::Change,
};

fn uncolored_config() -> Config {
    let mut config = Config::default();
    config.interface.status_color = false;
    config
}

#[test]
fn double_escape_stops_only_plain_root_and_expires_after_700ms() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("keep exact draft");
    ui.draft.set_cursor(4);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(ui.stop_armed.is_some() && rx.try_recv().is_err());
    ui.stop_armed = Some(Instant::now() - Duration::from_millis(701));
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(ui.stop_armed.is_some() && rx.try_recv().is_err());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    let stop = rx.try_recv().unwrap();
    assert!(matches!(
        stop.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(matches!(stop.op, Op::Stop { clear_draft: false }));
    assert!(ui.stop_pending && ui.stop_armed.is_none());
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("keep exact draft", 4)
    );
    assert!(ui.submit(&snapshot, &tx).is_err());
    complete(
        &mut ui,
        stop.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    assert!(ui.stopped && !ui.stop_pending);

    for view in [
        View::Help,
        View::Settings,
        View::Queue,
        View::Details,
        View::Drafts,
        View::Bindings,
        View::History,
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.stop_armed = Some(Instant::now());
        ui.view = view;
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(ui.stop_armed.is_none() && rx.try_recv().is_err());
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(
            rx.try_recv().is_err(),
            "closing a panel cannot count as the first Escape"
        );
    }
    for palette in [false, true] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.palette = palette;
        if palette {
            ui.palette_query.set_text("/d");
        } else {
            ui.draft.set_text("/d");
        }
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        ui.stop_armed = Some(Instant::now());
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(ui.stop_armed.is_none() && rx.try_recv().is_err());
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn stop_errors_unlock_guards_and_empty_enter_never_resumes() {
    for failure in [0, 1, 2] {
        let mut snapshot = fixture_snapshot();
        let mut ui = Ui::new(&Config::default(), &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        ui.draft.set_text("/stop");
        ui.submit(&snapshot, &tx).unwrap();
        let stop = rx.try_recv().unwrap();
        assert!(ui.stop_pending && ui.draft_busy);
        complete(
            &mut ui,
            stop.op,
            match failure {
                0 => Err(anyhow::anyhow!("storage failed")),
                1 => Ok(Reply::Inputs(vec![])),
                _ => Ok(Reply::Ack),
            },
        );
        assert!(!ui.stop_pending && !ui.draft_busy && !ui.stopped);
        assert_eq!(ui.draft.text(), "/stop");
        ui.submit(&snapshot, &tx).unwrap();
        complete(
            &mut ui,
            rx.try_recv().unwrap().op,
            Ok(Reply::Stopped {
                paused: true,
                cancelled: false,
            }),
        );
        snapshot.paused = true;
        ui.draft.set_text(" \n\t");
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(rx.try_recv().is_err() && ui.stopped);
        ui.draft.set_text("restart");
        ui.submit(&snapshot, &tx).unwrap();
        let send = rx.try_recv().unwrap();
        assert!(matches!(
            send.command,
            Command::SubmitInputAccumulated {
                resume: true,
                expected_paused: true,
                ..
            }
        ));
        complete(&mut ui, send.op, Ok(Reply::Ack));
        assert!(!ui.stopped && !ui.draft_busy && ui.pending_send.is_none());
    }
}

#[test]
fn repeated_stop_consumes_the_command_field_without_touching_primary_input() {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    for pinned in [false, true] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        ui.stopped = true;
        ui.draft.set_text("retained primary");
        ui.draft.set_cursor(4);
        ui.palette = true;
        ui.palette_pinned = pinned;
        ui.commands_focus = true;
        ui.palette_query.set_text("/stop");
        ui.submit(&snapshot, &tx).unwrap();
        let stop = rx.try_recv().unwrap();
        assert!(matches!(
            stop.command,
            Command::StopPending {
                expected_attempt_id: None,
                expected_paused: true,
            }
        ));
        assert!(matches!(stop.op, Op::Stop { clear_draft: false }));
        assert_eq!(
            (ui.draft.text(), ui.draft.cursor()),
            ("retained primary", 4)
        );
        assert!(ui.palette_query.text().is_empty());
        assert_eq!(ui.palette, pinned);
        assert!(ui.stopped && ui.stop_pending && !ui.draft_busy);
        assert!(ui.execute(Action::Stop, "", false, &snapshot, &tx).is_err());
        complete(&mut ui, stop.op, Ok(Reply::Ack));
        assert!(ui.stopped && !ui.stop_pending && !ui.draft_busy);
        assert!(ui.notice.as_ref().unwrap().2);
        ui.palette = true;
        ui.commands_focus = true;
        ui.palette_query.set_text("/stop");
        ui.submit(&snapshot, &tx).unwrap();
        let stop = rx.try_recv().unwrap();
        assert!(matches!(
            stop.command,
            Command::StopPending {
                expected_paused: true,
                ..
            }
        ));
        complete(
            &mut ui,
            stop.op,
            Ok(Reply::Stopped {
                paused: true,
                cancelled: false,
            }),
        );
        assert!(ui.stopped && !ui.stop_pending && !ui.draft_busy);
        assert!(ui.palette_query.text().is_empty());
        assert_eq!(ui.palette, pinned);
        assert_eq!(
            (ui.draft.text(), ui.draft.cursor()),
            ("retained primary", 4)
        );
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn checked_stop_captures_attempt_and_pause_for_command_and_double_escape() {
    for paused in [false, true] {
        for attempt in [None, Some("selected-attempt")] {
            for double_escape in [false, true] {
                let mut snapshot = fixture_snapshot();
                snapshot.paused = paused;
                snapshot.active = attempt.map(|id| crate::runtime::ActiveView {
                    attempt_id: id.into(),
                    text: "partial".into(),
                    durable_bytes: 0,
                    truncated: false,
                    message_id: String::new(),
                });
                let mut ui = Ui::new(&Config::default(), &snapshot);
                let (tx, mut rx) = mpsc::channel(OUTSTANDING);
                ui.draft.set_text(if double_escape {
                    "retained draft"
                } else {
                    "/stop"
                });
                if double_escape {
                    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
                    assert!(rx.try_recv().is_err());
                    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
                } else {
                    ui.submit(&snapshot, &tx).unwrap();
                }
                let stop = rx.try_recv().unwrap();
                assert!(matches!(stop.command, Command::StopPending {
                    expected_attempt_id,
                    expected_paused,
                } if expected_attempt_id.as_deref() == attempt && expected_paused == paused));
                assert!(
                    matches!(stop.op, Op::Stop { clear_draft } if clear_draft == !double_escape)
                );
                assert!(ui.stop_pending);
                assert_eq!(ui.draft_busy, !double_escape);
                assert!(rx.try_recv().is_err());
            }
        }
    }
}

#[test]
fn stale_checked_stop_error_preserves_input_and_never_retargets_new_snapshot() {
    for pause_changed in [false, true] {
        for double_escape in [false, true] {
            let mut snapshot = fixture_snapshot();
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "selected-attempt".into(),
                text: "partial".into(),
                durable_bytes: 0,
                truncated: false,
                message_id: String::new(),
            });
            let mut ui = Ui::new(&Config::default(), &snapshot);
            let (tx, mut rx) = mpsc::channel(OUTSTANDING);
            ui.draft.set_text(if double_escape {
                "keep exact draft"
            } else {
                "/stop"
            });
            ui.draft.set_cursor(3);
            let before = (ui.draft.text().to_owned(), ui.draft.cursor());
            if double_escape {
                press(&mut ui, KeyCode::Esc, &snapshot, &tx);
                press(&mut ui, KeyCode::Esc, &snapshot, &tx);
            } else {
                ui.submit(&snapshot, &tx).unwrap();
            }
            let stop = rx.try_recv().unwrap();
            let inputs = ui.inputs.clone();
            if pause_changed {
                snapshot.paused = true;
            } else {
                snapshot.active.as_mut().unwrap().attempt_id = "new-attempt".into();
            }
            assert!(matches!(stop.command, Command::StopPending {
                expected_attempt_id: Some(ref id),
                expected_paused: false,
            } if id == "selected-attempt"));
            let error = if pause_changed {
                "pause state changed"
            } else {
                "active attempt changed"
            };
            complete(&mut ui, stop.op, Err(anyhow::anyhow!(error)));
            assert_eq!(
                (ui.draft.text(), ui.draft.cursor()),
                (before.0.as_str(), before.1)
            );
            assert!(!ui.stop_pending && !ui.draft_busy && !ui.stopped);
            assert_eq!(ui.inputs, inputs);
            assert!(ui.notice.as_ref().unwrap().2);
            assert!(ui.notice.as_ref().unwrap().0.contains(error));
            ui.advance_chain(&snapshot, &tx);
            assert!(
                rx.try_recv().is_err(),
                "a stale stop must not retry against the new snapshot"
            );
            assert_eq!(snapshot.paused, pause_changed);
            assert_eq!(
                snapshot.active.as_ref().unwrap().attempt_id,
                if pause_changed {
                    "selected-attempt"
                } else {
                    "new-attempt"
                }
            );
        }
    }
}

#[test]
fn checked_stop_reply_with_queued_work_continues_without_setting_stopped() {
    for queued in [false, true] {
        for active in [false, true] {
            for paused in [false, true] {
                for double_escape in [false, true] {
                    let mut snapshot = pending_queue_snapshot();
                    snapshot.paused = paused;
                    if !queued {
                        snapshot.queue.clear();
                    }
                    snapshot.active = active.then(|| crate::runtime::ActiveView {
                        attempt_id: "selected-attempt".into(),
                        text: "partial".into(),
                        durable_bytes: 0,
                        truncated: false,
                        message_id: String::new(),
                    });
                    let mut ui = Ui::new(&Config::default(), &snapshot);
                    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
                    ui.stopped = paused;
                    ui.draft.set_text(if double_escape {
                        "retained draft"
                    } else {
                        "/stop"
                    });
                    ui.draft.set_cursor(3);
                    if double_escape {
                        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
                        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
                    } else {
                        ui.submit(&snapshot, &tx).unwrap();
                    }
                    let stop = rx.try_recv().unwrap();
                    assert!(
                        matches!(stop.op, Op::Stop { clear_draft } if clear_draft == !double_escape)
                    );
                    complete(
                        &mut ui,
                        stop.op,
                        Ok(Reply::Stopped {
                            paused: !queued,
                            cancelled: active,
                        }),
                    );
                    assert_eq!(ui.stopped, !queued);
                    assert!(!ui.stop_pending && !ui.draft_busy);
                    assert_eq!(
                        (ui.draft.text(), ui.draft.cursor()),
                        if double_escape {
                            ("retained draft", 3)
                        } else {
                            ("", 0)
                        }
                    );
                    assert_eq!(
                        ui.notice.as_ref().unwrap().0,
                        if !queued {
                            "stopped"
                        } else if active {
                            "response interrupted; pending input will continue"
                        } else {
                            "pending input will continue"
                        }
                    );
                    assert!(!ui.notice.as_ref().unwrap().2);
                    assert!(
                        rx.try_recv().is_err(),
                        "continuation needs no client resume command"
                    );
                    snapshot.paused = !queued;
                    snapshot.active = None;
                    ui.draft.set_text("next input");
                    ui.submit(&snapshot, &tx).unwrap();
                    assert!(
                        matches!(rx.try_recv().unwrap().command, Command::SubmitInputAccumulated {
                        mode: crate::runtime::DeliveryMode::After, resume, expected_paused, ..
                    } if resume == !queued && expected_paused == !queued)
                    );
                }
            }
        }
    }
}

#[test]
fn intervening_input_or_shortcut_disarms_double_escape() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    ui.append("paste");
    assert!(ui.stop_armed.is_none());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    press(&mut ui, KeyCode::Left, &snapshot, &tx);
    assert!(ui.stop_armed.is_none());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    ui.shortcut(Action::ClearDraft, &snapshot, &tx).unwrap();
    assert!(ui.stop_armed.is_none() && rx.try_recv().is_err());
}

fn pending_queue_snapshot() -> Snapshot {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    snapshot.queue = vec![
        QueueItem {
            id: "old-pending".into(),
            revision: 3,
            text: "older legacy row".into(),
            state: QueueState::Queued,
        },
        QueueItem {
            id: "latest-pending".into(),
            revision: 7,
            text: "combined pending text".into(),
            state: QueueState::Queued,
        },
    ];
    snapshot
}

#[test]
fn ordinary_send_resume_is_driven_by_snapshot_not_local_stopped_flag() {
    for paused in [false, true] {
        for stopped in [false, true] {
            let mut snapshot = fixture_snapshot();
            snapshot.paused = paused;
            let mut ui = Ui::new(&Config::default(), &snapshot);
            assert!(matches!(ui.mode, state::InputMode::After));
            let (tx, mut rx) = mpsc::channel(OUTSTANDING);
            ui.stopped = stopped;
            ui.draft.set_text("next nonempty message");
            ui.submit(&snapshot, &tx).unwrap();
            assert!(
                matches!(rx.try_recv().unwrap().command, Command::SubmitInputAccumulated { mode: crate::runtime::DeliveryMode::After, resume, expected_paused, .. } if resume == paused && expected_paused == paused)
            );
            assert!(ui.pending_send.is_none());
        }
    }
}

#[test]
fn empty_up_prefers_latest_pending_and_worker_failure_cleans_acquisition_flags() {
    let snapshot = pending_queue_snapshot();
    let mut config = Config::default();
    config.interface.prompt_history = false;
    let mut ui = Ui::new(&config, &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "", 0, 2);
    ui.inputs.push("previous input".into());
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(work.command, Command::BeginEdit { ref id, revision: 7 } if id == "latest-pending")
    );
    complete(&mut ui, work.op, Ok(Reply::Inputs(vec![])));
    assert!(!ui.draft_busy && ui.edit.is_none() && ui.edit_draft.is_none());
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("", 0));
    let (closed, receiver) = mpsc::channel(OUTSTANDING);
    drop(receiver);
    press(&mut ui, KeyCode::Up, &snapshot, &closed);
    assert!(!ui.draft_busy && ui.edit.is_none() && ui.edit_draft.is_none());
    ui.draft.set_text("not yet saved");
    ui.draft.set_cursor(4);
    press(&mut ui, KeyCode::Up, &snapshot, &closed);
    assert!(!ui.draft_busy && ui.edit.is_none() && ui.edit_draft.is_none());
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("not yet saved", 4));
}

#[test]
fn late_begin_edit_receipt_never_overwrites_newer_composer() {
    let snapshot = pending_queue_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "suspended", 2, 4);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    let acquire = rx.try_recv().unwrap();
    ui.draft.set_text("newer composer");
    ui.draft.set_cursor(3);
    complete(
        &mut ui,
        acquire.op,
        Ok(Reply::Editing(QueueItem {
            id: "latest-pending".into(),
            revision: 8,
            text: "authoritative queued text".into(),
            state: QueueState::Editing,
        })),
    );
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("newer composer", 3));
    assert!(ui.edit.is_none() && ui.draft_busy);
    ui.flush_edit_release(&tx);
    let release = rx.try_recv().unwrap();
    assert!(matches!(
        release.command,
        Command::CancelEdit { revision: 8, .. }
    ));
    complete(&mut ui, release.op, Ok(Reply::Ack));
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("newer composer", 3));
    assert!(!ui.draft_busy && ui.edit_draft.is_none());
}

#[test]
fn queue_first_up_saves_exact_draft_before_authoritative_acquisition_even_without_history() {
    let snapshot = pending_queue_snapshot();
    for history in [false, true] {
        let mut config = Config::default();
        config.interface.prompt_history = history;
        let mut ui = Ui::new(&config, &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        restored(&mut ui, "", 0, 4);
        ui.draft.set_text("na\u{00ef}ve\nsecond line");
        ui.draft.set_cursor(4);
        ui.inputs.push("naive recalled input".into());
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        let save = rx.try_recv().unwrap();
        assert!(
            matches!(save.command, Command::SaveComposer { ref text, cursor: 4, revision: 4 } if text == "na\u{00ef}ve\nsecond line")
        );
        assert!(ui.draft_busy && ui.edit.is_none() && rx.try_recv().is_err());
        assert_eq!(ui.draft.cursor(), 4);
        ui.append("must not overwrite");
        complete(
            &mut ui,
            save.op,
            Ok(Reply::Composer(crate::runtime::ComposerState {
                text: "na\u{00ef}ve\nsecond line".into(),
                cursor: 4,
                revision: 5,
            })),
        );
        ui.flush_pending_edit(&tx);
        let acquire = rx.try_recv().unwrap();
        assert!(
            matches!(acquire.command, Command::BeginEdit { ref id, revision: 7 } if id == "latest-pending")
        );
        assert_eq!(ui.edit_draft, Some(("na\u{00ef}ve\nsecond line".into(), 4)));
        complete(
            &mut ui,
            acquire.op,
            Ok(Reply::Editing(QueueItem {
                id: "latest-pending".into(),
                revision: 8,
                text: "authoritative combined text".into(),
                state: QueueState::Editing,
            })),
        );
        assert_eq!(ui.edit, Some(("latest-pending".into(), 8)));
        assert_eq!(ui.draft.text(), "authoritative combined text");
        ui.autosave(&tx, true);
        assert!(rx.try_recv().is_err());
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        let cancel = rx.try_recv().unwrap();
        assert!(
            matches!(cancel.command, Command::CancelEdit { ref id, revision: 8 } if id == "latest-pending")
        );
        assert_eq!(ui.draft.text(), "authoritative combined text");
        assert!(ui.stop_armed.is_none());
        complete(&mut ui, cancel.op, Ok(Reply::Ack));
        assert_eq!(
            (ui.draft.text(), ui.draft.cursor()),
            ("na\u{00ef}ve\nsecond line", 4)
        );
        assert!(ui.edit.is_none() && ui.edit_draft.is_none());
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(rx.try_recv().is_err(), "cancel edit cannot arm stop");
    }
}

#[test]
fn pinned_palette_primary_up_edits_pending_and_down_restores_both_fields_after_ack() {
    let snapshot = pending_queue_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "", 0, 4);
    ui.draft.set_text("na\u{00ef}ve draft");
    ui.draft.set_cursor(4);
    ui.palette = true;
    ui.palette_pinned = true;
    ui.commands_focus = false;
    ui.palette_query.set_text("/stop");
    ui.palette_query.set_cursor(3);

    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    let save = rx.try_recv().unwrap();
    assert!(
        matches!(save.command, Command::SaveComposer { ref text, cursor: 4, revision: 4 } if text == "na\u{00ef}ve draft")
    );
    assert!(ui.draft_busy && ui.edit.is_none() && rx.try_recv().is_err());
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("na\u{00ef}ve draft", 4)
    );
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("/stop", 3)
    );
    assert!(ui.palette && ui.palette_pinned && !ui.commands_focus);

    complete(
        &mut ui,
        save.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "na\u{00ef}ve draft".into(),
            cursor: 4,
            revision: 5,
        })),
    );
    ui.flush_pending_edit(&tx);
    let acquire = rx.try_recv().unwrap();
    assert!(
        matches!(acquire.command, Command::BeginEdit { ref id, revision: 7 } if id == "latest-pending")
    );
    assert!(ui.draft_busy && ui.edit.is_none() && rx.try_recv().is_err());
    complete(
        &mut ui,
        acquire.op,
        Ok(Reply::Editing(QueueItem {
            id: "latest-pending".into(),
            text: "authoritative combined text".into(),
            revision: 8,
            state: QueueState::Editing,
        })),
    );
    assert_eq!(ui.edit, Some(("latest-pending".into(), 8)));
    assert_eq!(ui.draft.text(), "authoritative combined text");
    assert_eq!(ui.draft.cursor(), ui.draft.text().len());
    assert_eq!(ui.edit_draft, Some(("na\u{00ef}ve draft".into(), 4)));
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("/stop", 3)
    );
    assert!(ui.palette && ui.palette_pinned && !ui.commands_focus);
    assert!(!ui.draft_busy && !ui.stop_pending && !ui.exiting);
    assert!(ui.inputs.is_empty() && rx.try_recv().is_err());

    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    let cancel = rx.try_recv().unwrap();
    assert!(
        matches!(cancel.command, Command::CancelEdit { ref id, revision: 8 } if id == "latest-pending")
    );
    assert!(ui.draft_busy && ui.edit.is_some() && rx.try_recv().is_err());
    assert_eq!(ui.draft.text(), "authoritative combined text");
    complete(&mut ui, cancel.op, Ok(Reply::Ack));
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("na\u{00ef}ve draft", 4)
    );
    assert!(ui.edit.is_none() && ui.edit_draft.is_none() && !ui.draft_busy);
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("/stop", 3)
    );
    assert!(ui.palette && ui.palette_pinned && !ui.commands_focus);
    assert!(ui.inputs.is_empty() && rx.try_recv().is_err());
    assert!(!ui.stop_pending && !ui.stopped && !ui.exiting);
}

#[test]
fn queue_acquisition_save_or_begin_failure_preserves_draft_and_caret() {
    let snapshot = pending_queue_snapshot();
    for save_failure in [false, true] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        restored(&mut ui, "original", 2, 4);
        if save_failure {
            ui.draft.set_text("changed");
            ui.draft.set_cursor(2);
        }
        let before = (ui.draft.text().to_owned(), ui.draft.cursor());
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        let work = rx.try_recv().unwrap();
        assert_eq!(
            matches!(work.command, Command::SaveComposer { .. }),
            save_failure
        );
        complete(&mut ui, work.op, Err(anyhow::anyhow!("stale revision")));
        ui.flush_pending_edit(&tx);
        assert_eq!(
            (ui.draft.text(), ui.draft.cursor()),
            (before.0.as_str(), before.1)
        );
        assert!(!ui.draft_busy && ui.edit.is_none() && ui.edit_draft.is_none());
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn queue_acquisition_waits_for_old_autosave_then_saves_latest_suspended_draft() {
    let snapshot = pending_queue_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "", 0, 1);
    ui.draft.set_text("old save");
    ui.autosave(&tx, true);
    let old = rx.try_recv().unwrap();
    ui.draft.set_text("latest draft");
    ui.draft.set_cursor(2);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert!(rx.try_recv().is_err());
    complete(
        &mut ui,
        old.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "old save".into(),
            cursor: 8,
            revision: 2,
        })),
    );
    ui.flush_pending_edit(&tx);
    let latest = rx.try_recv().unwrap();
    assert!(
        matches!(latest.command, Command::SaveComposer { ref text, cursor: 2, revision: 2 } if text == "latest draft")
    );
    complete(
        &mut ui,
        latest.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "latest draft".into(),
            cursor: 2,
            revision: 3,
        })),
    );
    ui.flush_pending_edit(&tx);
    assert!(matches!(
        rx.try_recv().unwrap().command,
        Command::BeginEdit { revision: 7, .. }
    ));
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("latest draft", 2));
}

#[test]
fn pending_edit_finish_empty_delete_and_down_restore_only_after_ack() {
    let snapshot = pending_queue_snapshot();
    for operation in ["finish", "delete", "down", "escape"] {
        for fail in [false, true] {
            let mut ui = Ui::new(&Config::default(), &snapshot);
            let (tx, mut rx) = mpsc::channel(OUTSTANDING);
            restored(&mut ui, "suspended", 3, 2);
            press(&mut ui, KeyCode::Up, &snapshot, &tx);
            let acquire = rx.try_recv().unwrap();
            complete(
                &mut ui,
                acquire.op,
                Ok(Reply::Editing(QueueItem {
                    id: "latest-pending".into(),
                    revision: 8,
                    text: "combined pending text".into(),
                    state: QueueState::Editing,
                })),
            );
            if operation == "delete" {
                ui.draft.clear();
            }
            if operation == "finish" {
                ui.draft.set_text("combined replacement");
            }
            press(
                &mut ui,
                match operation {
                    "down" => KeyCode::Down,
                    "escape" => KeyCode::Esc,
                    _ => KeyCode::Enter,
                },
                &snapshot,
                &tx,
            );
            let work = rx.try_recv().unwrap();
            match operation {
                "finish" => assert!(
                    matches!(work.command, Command::FinishPendingEdit { ref id, revision: 8, ref text, expected_paused: true } if id == "latest-pending" && text == "combined replacement")
                ),
                "delete" => assert!(matches!(work.command, Command::Cancel { revision: 8, .. })),
                _ => assert!(matches!(
                    work.command,
                    Command::CancelEdit { revision: 8, .. }
                )),
            }
            assert!(ui.edit.is_some() && ui.draft_busy);
            let edit_text = ui.draft.text().to_owned();
            complete(
                &mut ui,
                work.op,
                if fail {
                    Err(anyhow::anyhow!("pause state changed"))
                } else {
                    Ok(Reply::Ack)
                },
            );
            assert!(!ui.draft_busy);
            if fail {
                assert_eq!(ui.edit, Some(("latest-pending".into(), 8)));
                assert_eq!(ui.draft.text(), edit_text);
                assert_eq!(ui.edit_draft, Some(("suspended".into(), 3)));
            } else {
                assert_eq!((ui.draft.text(), ui.draft.cursor()), ("suspended", 3));
                assert!(ui.edit.is_none() && ui.edit_draft.is_none());
                assert_eq!(
                    ui.inputs.contains(&"combined replacement".to_owned()),
                    operation == "finish"
                );
            }
            assert!(ui.stop_armed.is_none() && rx.try_recv().is_err());
        }
    }
}

#[test]
fn shutdown_during_pending_edit_keeps_suspended_composer_not_unsubmitted_edit_text() {
    let snapshot = pending_queue_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "suspended", 2, 4);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    let acquire = rx.try_recv().unwrap();
    complete(
        &mut ui,
        acquire.op,
        Ok(Reply::Editing(QueueItem {
            id: "latest-pending".into(),
            revision: 8,
            text: "original queue text".into(),
            state: QueueState::Editing,
        })),
    );
    ui.draft.set_text("unsubmitted replacement");
    ui.pending_notices.clear();
    ui.execute(Action::Quit, "", false, &snapshot, &tx).unwrap();
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
    assert_eq!(ui.edit_draft, Some(("suspended".into(), 2)));
    assert!(
        rx.try_recv().is_err(),
        "leased edits must never autosave over the suspended composer"
    );
}

#[test]
fn binding_shift_arrows_move_words_without_accepting_ghost_or_preview() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        KeyModifiers::SHIFT | KeyModifiers::ALT,
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.view = View::Bindings;
        ui.binding_key = Some("f3".into());
        ui.binding_commands.set_text("/his");
        ui.key(
            KeyEvent::new(KeyCode::Left, modifiers),
            &snapshot,
            &tx,
            100,
            20,
        )
        .unwrap();
        assert_eq!(
            (ui.binding_commands.text(), ui.binding_commands.cursor()),
            ("/his", 1)
        );
        ui.key(
            KeyEvent::new(KeyCode::Right, modifiers),
            &snapshot,
            &tx,
            100,
            20,
        )
        .unwrap();
        assert_eq!(
            (ui.binding_commands.text(), ui.binding_commands.cursor()),
            ("/his", 4)
        );
        ui.binding_commands.set_text("/d");
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        assert!(ui.completion_active && ui.pager_selected);
        let text = ui.binding_commands.text().to_owned();
        ui.key(
            KeyEvent::new(KeyCode::Left, modifiers),
            &snapshot,
            &tx,
            100,
            20,
        )
        .unwrap();
        assert_eq!(ui.binding_commands.text(), text);
        assert!(!ui.completion_active && !ui.pager_selected);
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn visual_binding_unique_completion_requires_confirmation_then_save_without_executing_steps() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Bindings;
    ui.binding_key = Some("f2".into());
    ui.binding_commands.set_text("/settings status-color fal");
    ui.draft.set_text("protected primary");
    ui.draft.set_cursor(4);
    ui.edit = Some(("lease".into(), 7));
    ui.palette = true;
    ui.palette_pinned = true;
    ui.commands_focus = false;
    ui.palette_query.set_text("pinned command");
    ui.palette_query.set_cursor(3);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.binding_commands.text(), "/settings status-color false ");
    assert!(ui.completion_active && ui.completion_visible());
    assert_eq!(ui.editor().text(), ui.binding_commands.text());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(!ui.completion_active && ui.keys.macros.is_empty());
    assert!(ui.binding_key.is_some() && ui.bindings_task.is_none());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.keys.macros[0].commands, ["/settings status-color false"]);
    assert!(ui.interface.status_color);
    assert_eq!(ui.draft.text(), "protected primary");
    assert_eq!(ui.draft.cursor(), 4);
    assert_eq!(ui.palette_query.text(), "pinned command");
    assert_eq!(ui.palette_query.cursor(), 3);
    assert!(ui.palette_pinned && !ui.commands_focus);
    assert_eq!(ui.edit, Some(("lease".into(), 7)));
    assert!(rx.try_recv().is_err());
}

#[test]
fn visual_binding_pager_cycles_reversibly_and_escape_rolls_back_before_edit_cancel() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Bindings;
    ui.binding_key = Some("f2".into());
    ui.binding_commands.set_text("/help ; /s");
    let values = ui
        .binding_completion(&snapshot)
        .unwrap()
        .items
        .into_iter()
        .map(|item| item.value)
        .collect::<Vec<_>>();
    assert!(values.len() > 2);
    ui.draft.set_text("unchanged primary");
    ui.draft.set_cursor(5);
    ui.palette = true;
    ui.palette_pinned = true;
    ui.palette_query.set_text("unchanged secondary");
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.binding_commands.text(), "/help ; /s");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(
        ui.binding_commands.text(),
        format!("/help ; {} ", values[0])
    );
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(
        ui.binding_commands.text(),
        format!("/help ; {} ", values[1])
    );
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(
        ui.binding_commands.text(),
        format!("/help ; {} ", values[0])
    );
    press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
    assert_eq!(
        ui.binding_commands.text(),
        format!("/help ; {} ", values.last().unwrap())
    );
    assert!(!ui.exiting && ui.keys.macros.is_empty());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.binding_commands.text(), "/help ; /s");
    assert_eq!(ui.binding_commands.cursor(), "/help ; /s".len());
    assert!(ui.binding_key.is_some() && !ui.completion_active);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(ui.binding_key.is_none());
    assert_eq!(ui.view, View::Bindings);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!(ui.draft.text(), "unchanged primary");
    assert_eq!(ui.draft.cursor(), 5);
    assert_eq!(ui.palette_query.text(), "unchanged secondary");
    assert!(ui.palette_pinned && rx.try_recv().is_err());
}

#[test]
fn visual_binding_selected_enter_is_commit_only_and_respects_selection_visibility() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Bindings;
    ui.binding_key = Some("f2".into());
    ui.binding_commands.set_text("/settings status-color ");
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert!(ui.pager_selected && ui.completion_active);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            1,
            1
        )
        .is_err()
    );
    assert!(ui.completion_active && ui.keys.macros.is_empty());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.keys.macros.is_empty() && !ui.completion_active);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.keys.macros[0].commands, ["/settings status-color false"]);
    assert!(ui.interface.status_color);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn visual_binding_default_completion_does_not_save_until_second_enter_or_duplicate_flag() {
    let snapshot = fixture_snapshot();
    let directory = tempfile::tempdir().unwrap();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.preferences_path = directory.path().join("preferences.toml");
    ui.view = View::Bindings;
    ui.binding_key = Some("f3".into());
    ui.binding_commands.set_text("/help --d");
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.binding_commands.text(), "/help --default ");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.bindings_task.is_none() && !ui.preferences_path.exists());
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    ui.finish_pending_bindings().await.unwrap();
    let saved = Config::load_with_preferences(None, &ui.preferences_path).unwrap();
    assert_eq!(saved.keys.macros[0].commands, ["/help"]);
    assert!(rx.try_recv().is_err());
}

#[test]
fn production_binding_and_unbinding_completion_uses_current_client_macros() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.keys.macros.push(crate::commands::CommandBinding {
        key: "f8".into(),
        commands: vec!["/stop".into()],
    });
    ui.draft.set_text("/unbind ");
    assert_eq!(ui.completion(&snapshot).unwrap().items[0].value, "f8");
    ui.palette = true;
    ui.palette_query
        .set_text("bind f9 /help; /settings status mi");
    let menu = ui.completion(&snapshot).unwrap();
    assert_eq!(menu.items[0].value, "minimal");
    assert_eq!(&ui.palette_query.text()[menu.range], "mi");
    ui.view = View::Bindings;
    ui.binding_key = Some("f9".into());
    ui.binding_commands.set_text("/unbind ");
    assert_eq!(
        ui.binding_completion(&snapshot).unwrap().items[0].value,
        "f8"
    );
    assert_eq!(ui.draft.text(), "/unbind ");
    assert_eq!(
        ui.palette_query.text(),
        "bind f9 /help; /settings status mi"
    );
}

#[test]
fn nested_binding_draft_completion_fetches_records_without_stealing_binding_selection() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Bindings;
    ui.binding_key = Some("f2".into());
    ui.selected = 7;
    ui.binding_commands.set_text("/help ; /send ");
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    assert!(ui.binding_completion(&snapshot).unwrap().items.is_empty());
    ui.poll(&tx);
    let read = rx.try_recv().unwrap();
    assert!(matches!(read.command, Command::Drafts));
    complete(
        &mut ui,
        read.op,
        Ok(Reply::Drafts(vec![crate::runtime::DraftRecord {
            id: "aaaa-saved".into(),
            text: "saved body".into(),
            revision: 4,
        }])),
    );
    assert_eq!(
        ui.binding_completion(&snapshot).unwrap().items[0].value,
        "aaaa-saved"
    );
    assert_eq!(ui.selected, 7);
    assert_eq!(ui.binding_commands.text(), "/help ; /send ");
    while let Ok(read) = rx.try_recv() {
        let reply = match read.command {
            Command::NativeTranscript { .. } => Reply::NativeTranscript(vec![]),
            Command::Transcript { .. } => Reply::History(vec![]),
            Command::Events { .. } => Reply::Events(vec![]),
            command => panic!("unexpected completion read: {command:?}"),
        };
        complete(&mut ui, read.op, Ok(reply));
    }
}

#[test]
fn defaults_keep_colored_bars_status_and_colored_errors() {
    let mut snapshot = fixture_snapshot();
    snapshot.phase = Phase::StorageError;
    snapshot.error = Some("writes paused".into());
    let ui = Ui::new(&Config::default(), &snapshot);
    assert!(ui.interface.status_color);
    assert_eq!(ui.interface.status_preset, StatusPreset::Bars);
    assert!(
        rendered(&ui, &snapshot, 80, INLINE_HEIGHT)
            .content
            .iter()
            .any(|cell| cell.fg == Color::Indexed(167))
    );
}

#[test]
fn checked_submissions_wait_for_own_save_and_pause_ack_and_preserve_raw_source() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "//literal", 9, 5);
    ui.append("!");
    ui.autosave(&tx, true);
    let save = rx.try_recv().unwrap();
    assert!(
        ui.submit(&snapshot, &tx)
            .unwrap_err()
            .to_string()
            .contains("composer save acknowledgement")
    );
    assert!(
        ui.execute(Action::ForceSend, "", false, &snapshot, &tx)
            .is_err()
    );
    assert_eq!(ui.draft.text(), "//literal!");
    assert!(rx.try_recv().is_err());
    complete(
        &mut ui,
        save.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "//literal!".into(),
            cursor: 10,
            revision: 6,
        })),
    );
    ui.pause_pending = true;
    assert!(
        ui.submit(&snapshot, &tx)
            .unwrap_err()
            .to_string()
            .contains("pause/resume acknowledgement")
    );
    assert_eq!(ui.draft.text(), "//literal!");
    assert!(rx.try_recv().is_err());
    ui.pause_pending = false;
    ui.submit(&snapshot, &tx).unwrap();
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(work.command, Command::SubmitInputAccumulated { source, text, composer_revision: 6, mode: crate::runtime::DeliveryMode::After, resume: false, expected_paused: false } if source == "//literal!" && text == "/literal!")
    );
    complete(
        &mut ui,
        work.op,
        Err(anyhow::anyhow!("pause state changed")),
    );
    assert_eq!(ui.draft.text(), "//literal!");
    ui.execute(Action::ForceSend, "", false, &snapshot, &tx)
        .unwrap();
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::SubmitInputAccumulated { source, text, composer_revision: 6, mode: crate::runtime::DeliveryMode::Force, resume: false, expected_paused: false } if source == "//literal!" && text == "/literal!")
    );
}

#[test]
fn paused_accumulated_send_keeps_revision_and_suspends_autosave() {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "", 0, 8);
    ui.append("//held");
    ui.submit(&snapshot, &tx).unwrap();
    let work = rx.try_recv().unwrap();
    ui.autosave(&tx, true);
    assert!(rx.try_recv().is_err());
    assert!(ui.pending_send.is_none() && ui.draft_busy);
    assert!(
        matches!(work.command, Command::SubmitInputAccumulated { source, text, composer_revision: 8, resume: true, expected_paused: true, .. } if source == "//held" && text == "/held")
    );
}

#[test]
fn lowered_limits_reject_queue_edits_before_acquisition_and_release_oversized_receipts() {
    let mut snapshot = fixture_snapshot();
    let mut config = Config::default();
    config.limits.message_bytes = 8;
    snapshot.queue.push(QueueItem {
        id: "aaaa-queued".into(),
        text: "large queued message".into(),
        revision: 4,
        state: QueueState::Queued,
    });
    let mut ui = Ui::new(&config, &snapshot);
    ui.draft.set_text("keep");
    ui.draft.set_cursor(2);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    assert!(
        ui.execute(Action::Edit, "aaaa", true, &snapshot, &tx)
            .unwrap_err()
            .to_string()
            .contains("no edit lease acquired")
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(ui.draft.text(), "keep");
    assert_eq!(ui.draft.cursor(), 2);
    complete(
        &mut ui,
        Op::BeginEdit,
        Ok(Reply::Editing(QueueItem {
            id: "aaaa-queued".into(),
            text: "larger authoritative text".into(),
            revision: 5,
            state: QueueState::Editing,
        })),
    );
    assert!(ui.edit.is_none() && ui.draft_busy);
    assert_eq!(ui.draft.text(), "keep");
    assert_eq!(ui.draft.cursor(), 2);
    ui.flush_edit_release(&tx);
    let release = rx.try_recv().unwrap();
    assert!(
        matches!(release.command, Command::CancelEdit { id, revision: 5 } if id == "aaaa-queued")
    );
    complete(&mut ui, release.op, Ok(Reply::Ack));
    assert!(!ui.draft_busy && ui.edit.is_none());
    assert_eq!(ui.draft.text(), "keep");
    assert_eq!(ui.draft.cursor(), 2);
}

#[test]
fn oversized_draft_copy_is_rejected_before_saving_or_replacing_current_input() {
    let snapshot = fixture_snapshot();
    let mut config = Config::default();
    config.limits.message_bytes = 8;
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    for current in ["", "keep"] {
        let mut ui = Ui::new(&config, &snapshot);
        ui.view = View::Drafts;
        ui.draft.set_text(current);
        ui.drafts.push(crate::runtime::DraftRecord {
            id: "aaaa-saved".into(),
            text: "oversized saved text".into(),
            revision: 2,
        });
        assert!(
            ui.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &snapshot,
                &tx,
                100,
                20
            )
            .unwrap_err()
            .to_string()
            .contains("composer unchanged")
        );
        assert_eq!(ui.draft.text(), current);
        assert_eq!(ui.draft.cursor(), current.len());
        assert!(rx.try_recv().is_err());
        assert!(!ui.draft_busy);
    }
}

#[test]
fn history_to_drafts_or_bindings_always_leaves_fullscreen_and_escape_stays_inline() {
    let snapshot = fixture_snapshot();
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    for action in [Action::Drafts, Action::Bindings] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.view = View::History;
        ui.full_history = true;
        ui.execute(action, "", false, &snapshot, &tx).unwrap();
        assert!(!ui.full_history);
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert_eq!(ui.view, View::Root);
        assert!(!ui.full_history);
    }
}

#[test]
fn palette_enter_at_command_caret_preserves_and_executes_exact_arguments_only() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.palette = true;
    ui.palette_query.set_text("/settings status minimal");
    ui.palette_query.set_cursor(2);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.interface.status_preset, StatusPreset::Minimal);
    ui.palette = true;
    ui.palette_query.set_text("/commands keep");
    ui.palette_query.set_cursor(3);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.palette_pinned);
    ui.palette_query.set_text("/stop");
    ui.palette_query.set_cursor(3);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(matches!(
        work.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(matches!(work.op, Op::Stop { clear_draft: false }));
    complete(
        &mut ui,
        work.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    ui.palette_query.set_text("/quit unexpected");
    ui.palette_query.set_cursor(2);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            100,
            20
        )
        .is_err()
    );
    assert!(!ui.exiting);
    ui.palette_query.set_text("/qui unexpected");
    ui.palette_query.set_cursor(4);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.palette_query.text(), "/quit unexpected");
    assert!(!ui.exiting && rx.try_recv().is_err());
}

#[test]
fn oversized_joined_binding_chain_never_truncates_or_changes_the_saved_binding() {
    let snapshot = fixture_snapshot();
    let step = format!(
        "/bind f3 {}",
        vec!["/settings status-color true"; 16].join("; ")
    );
    let binding = crate::commands::CommandBinding {
        key: "f2".into(),
        commands: vec![step; 16],
    };
    assert!(binding.commands.join("; ").len() > 4096);
    let mut config = Config::default();
    config.keys.macros.push(binding.clone());
    config.keys.compile_macros().unwrap();
    let mut ui = Ui::new(&config, &snapshot);
    ui.view = View::Bindings;
    ui.binding_commands.set_text("previous editor text");
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            100,
            20
        )
        .unwrap_err()
        .to_string()
        .contains("4096-byte editor limit")
    );
    assert!(ui.binding_key.is_none());
    assert_eq!(ui.binding_commands.text(), "previous editor text");
    assert_eq!(ui.keys.macros, [binding]);
}

#[test]
fn send_rejects_legacy_delivery_and_pause_flags_without_completions_or_dispatch() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    for text in [
        "/send aaaa --after ",
        "/send aaaa --force ",
        "/send aaaa --hold ",
        "/send aaaa --run ",
        "/send aaaa --after --run ",
    ] {
        ui.draft.set_text(text);
        assert!(ui.completion(&snapshot).is_none());
        assert!(crate::commands::parse(text).is_err());
        assert!(ui.submit(&snapshot, &tx).is_err());
        assert_eq!(ui.draft.text(), text);
        assert!(!ui.draft_busy && ui.inputs.is_empty() && rx.try_recv().is_err());
    }
}

fn drain_action_reads(
    ui: &mut Ui,
    rx: &mut mpsc::Receiver<Work>,
    actions: &[crate::runtime::ActionRecord],
) -> Vec<Command> {
    let mut requests = Vec::new();
    while let Ok(work) = rx.try_recv() {
        requests.push(work.command.clone());
        let reply = match work.command {
            Command::Actions { before, limit } => Reply::Actions(
                actions
                    .iter()
                    .rev()
                    .filter(|action| before.is_none_or(|before| action.seq < before))
                    .take(limit.min(50))
                    .cloned()
                    .collect(),
            ),
            Command::ActionsAfter { after, limit } => Reply::Actions(
                actions
                    .iter()
                    .filter(|action| action.seq > after)
                    .take(limit.min(50))
                    .cloned()
                    .collect(),
            ),
            Command::NativeTranscript { .. } => Reply::NativeTranscript(vec![]),
            Command::Transcript { .. } | Command::History { .. } => Reply::History(vec![]),
            Command::Events { .. } => Reply::Events(vec![]),
            command => panic!("unexpected history request: {command:?}"),
        };
        complete(ui, work.op, Ok(reply));
    }
    requests
}

#[test]
fn history_action_list_slow_navigation_keeps_contiguous_bounded_pages_beyond_300_records() {
    use crate::runtime::ActionRecord;
    let actions = (1..=500)
        .map(|index| ActionRecord {
            seq: index * 7,
            id: format!("action-{index}"),
            group_id: String::new(),
            category: "queue".into(),
            label: format!("action {index}"),
            status: "done".into(),
            output: None,
        })
        .collect::<Vec<_>>();
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::History;
    ui.full_history = true;
    ui.history_browser.focus = history::browser::Focus::List;
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.poll(&tx);
    drain_action_reads(&mut ui, &mut rx, &actions);
    while ui.journal.selected_action().unwrap().seq != 3500 {
        press(&mut ui, KeyCode::Down, &snapshot, &tx);
        drain_action_reads(&mut ui, &mut rx, &actions);
    }
    for index in (1..500).rev() {
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        drain_action_reads(&mut ui, &mut rx, &actions);
        assert_eq!(ui.journal.selected_action().unwrap().seq, index * 7);
        ui.poll(&tx);
        let requests = drain_action_reads(&mut ui, &mut rx, &actions);
        assert!(!requests.iter().any(|command| matches!(
            command,
            Command::Actions { before: None, .. } | Command::ActionsAfter { .. }
        )));
        assert_eq!(ui.journal.selected_action().unwrap().seq, index * 7);
        assert!(ui.journal.actions.len() <= 200);
        assert!(
            ui.journal
                .actions
                .windows(2)
                .all(|pair| pair[1].seq - pair[0].seq == 7)
        );
    }
    for index in 2..=500 {
        press(&mut ui, KeyCode::Down, &snapshot, &tx);
        drain_action_reads(&mut ui, &mut rx, &actions);
        ui.poll(&tx);
        drain_action_reads(&mut ui, &mut rx, &actions);
        assert_eq!(ui.journal.selected_action().unwrap().seq, index * 7);
        assert!(ui.journal.actions.len() <= 200);
        assert!(
            ui.journal
                .actions
                .windows(2)
                .all(|pair| pair[1].seq - pair[0].seq == 7)
        );
    }
    // Older pages must become available again after forward paging evicts them.
    for index in (1..500).rev() {
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        drain_action_reads(&mut ui, &mut rx, &actions);
        assert_eq!(ui.journal.selected_action().unwrap().seq, index * 7);
    }
}

#[test]
fn in_flight_latest_actions_do_not_replace_an_older_browsing_window() {
    use crate::runtime::ActionRecord;
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::History;
    ui.full_history = true;
    ui.history_browser.focus = history::browser::Focus::List;
    let make = |seq| ActionRecord {
        seq,
        id: format!("action-{seq}"),
        group_id: String::new(),
        category: "queue".into(),
        label: "action".into(),
        status: "done".into(),
        output: None,
    };
    ui.journal.append_actions((100..150).map(make).collect());
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.poll(&tx);
    let work = rx.try_recv().unwrap();
    assert!(matches!(
        work.command,
        Command::ActionsAfter { after: 149, .. }
    ));
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    complete(
        &mut ui,
        work.op,
        Ok(Reply::Actions((300..400).map(make).collect())),
    );
    assert_eq!(ui.journal.selected_action().unwrap().seq, 100);
    assert_eq!(ui.journal.actions.last().unwrap().seq, 149);
}

#[test]
fn artifact_backpaging_beyond_200_chunks_uses_backend_offsets_and_keeps_utf8_pages_bounded() {
    use crate::runtime::{ActionRecord, ArtifactChunk, ArtifactRecord};
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::History;
    ui.full_history = true;
    ui.history_browser.focus = history::browser::Focus::Preview;
    ui.journal.append_actions(vec![ActionRecord {
        seq: 1,
        id: "action".into(),
        group_id: String::new(),
        category: "tool".into(),
        label: "output".into(),
        status: "done".into(),
        output: Some(ArtifactRecord {
            id: "output".into(),
            path: "/output".into(),
            bytes: 4_000_000,
            incomplete: false,
        }),
    }]);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let chunk = |text: String, next_offset| ArtifactChunk {
        id: "output".into(),
        text,
        next_offset,
        eof: false,
        path: "/output".into(),
        incomplete: false,
    };
    let mut offsets = Vec::new();
    let mut offset = 0;
    for index in 0..260 {
        offsets.push(offset);
        let size = 8192 - index % 3;
        let text = format!("{}\u{e9}", "x".repeat(size - 2));
        let next = offset + text.len() as u64;
        complete(
            &mut ui,
            Op::Artifact {
                id: "output".into(),
                offset,
                generation: 0,
            },
            Ok(Reply::Artifact(chunk(text, next))),
        );
        offset = next;
        assert!(ui.journal.preview_text.len() <= 8192);
    }
    press(&mut ui, KeyCode::PageUp, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(work.command, Command::ReadArtifactBefore { before, limit: 8192, .. } if before == offsets[259] && before > 0)
    );
    complete(
        &mut ui,
        work.op,
        Ok(Reply::ArtifactPage {
            offset: offsets[258],
            chunk: chunk("previous \u{e9}".into(), offsets[259]),
        }),
    );
    assert_eq!(ui.journal.preview_text, "previous \u{e9}");
    press(&mut ui, KeyCode::PageUp, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(work.command, Command::ReadArtifactBefore { before, .. } if before == offsets[258])
    );
    complete(
        &mut ui,
        Op::Artifact {
            id: "output".into(),
            offset: 0,
            generation: 1,
        },
        Ok(Reply::ArtifactPage {
            offset: 0,
            chunk: chunk("stale".into(), 5),
        }),
    );
    assert_eq!(ui.journal.preview_text, "previous \u{e9}");
}

fn restored(ui: &mut Ui, text: &str, cursor: usize, revision: i64) {
    complete(
        ui,
        Op::RestoreComposer,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: text.into(),
            cursor,
            revision,
        })),
    );
}

#[test]
fn composer_restore_blocks_input_and_stale_save_ack_never_overwrites_latest_edits() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.restore_composer(&tx);
    let restore = rx.try_recv().unwrap();
    assert!(ui.draft_busy);
    press(&mut ui, KeyCode::Char('x'), &snapshot, &tx);
    assert!(ui.draft.text().is_empty());
    complete(
        &mut ui,
        restore.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "restored".into(),
            cursor: 3,
            revision: 7,
        })),
    );
    assert_eq!(ui.draft.cursor(), 3);
    press(&mut ui, KeyCode::Char('!'), &snapshot, &tx);
    ui.autosave(&tx, false);
    assert!(rx.try_recv().is_err());
    ui.autosave(&tx, true);
    let first = rx.try_recv().unwrap();
    assert!(matches!(
        first.command,
        Command::SaveComposer {
            revision: 7,
            cursor: 4,
            ..
        }
    ));
    press(&mut ui, KeyCode::Char('?'), &snapshot, &tx);
    complete(
        &mut ui,
        first.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "res!tored".into(),
            cursor: 4,
            revision: 8,
        })),
    );
    assert_eq!(ui.draft.text(), "res!?tored");
    ui.autosave(&tx, true);
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::SaveComposer { text, revision: 8, cursor: 5 } if text == "res!?tored")
    );
}

#[test]
fn checked_dispatch_ack_advances_composer_revision_without_rebasing_or_losing_new_input() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "submitted", 9, 4);
    ui.submit(&snapshot, &tx).unwrap();
    let send = rx.try_recv().unwrap();
    ui.autosave(&tx, true);
    assert!(rx.try_recv().is_err());
    complete(&mut ui, send.op, Ok(Reply::Ack));
    ui.autosave(&tx, true);
    assert!(rx.try_recv().is_err());
    ui.draft.set_text("next message");
    ui.autosave(&tx, true);
    assert_eq!(ui.draft.text(), "next message");
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::SaveComposer { text, revision: 5, .. } if text == "next message")
    );
}

#[test]
fn checked_draft_dispatch_chain_keeps_composer_revision_and_does_not_need_refresh_reads() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "protected primary", 4, 12);
    ui.drafts = ["aaaa-one", "bbbb-two"]
        .into_iter()
        .map(|id| crate::runtime::DraftRecord {
            id: id.into(),
            text: "//saved literal".into(),
            revision: 3,
        })
        .collect();
    ui.start_chain(vec!["/send aaaa".into(), "/send bbbb".into()])
        .unwrap();
    ui.advance_chain(&snapshot, &tx);
    let first = rx.try_recv().unwrap();
    assert!(matches!(
        first.command,
        Command::DispatchDraftAccumulated {
            expected_paused: false,
            ..
        }
    ));
    complete(&mut ui, first.op, Ok(Reply::Ack));
    ui.advance_chain(&snapshot, &tx);
    let second = rx.try_recv().unwrap();
    assert!(
        matches!(second.command, Command::DispatchDraftAccumulated { id, .. } if id == "bbbb-two")
    );
    complete(&mut ui, second.op, Ok(Reply::Ack));
    ui.autosave(&tx, true);
    assert!(rx.try_recv().is_err());
    assert_eq!(ui.draft.text(), "protected primary");
    assert_eq!(ui.draft.cursor(), 4);
    ui.submit(&snapshot, &tx).unwrap();
    assert!(matches!(
        rx.try_recv().unwrap().command,
        Command::SubmitInputAccumulated {
            composer_revision: 12,
            ..
        }
    ));
}

#[test]
fn quit_drains_latest_composer_before_shutdown_and_leased_edits_are_not_autosaved() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    restored(&mut ui, "", 0, 2);
    ui.draft.set_text("durable on exit");
    ui.execute(Action::Quit, "", false, &snapshot, &tx).unwrap();
    let save = rx.try_recv().unwrap();
    assert!(matches!(save.command, Command::SaveComposer { .. }));
    assert!(!ui.try_shutdown(&tx));
    complete(
        &mut ui,
        save.op,
        Ok(Reply::Composer(crate::runtime::ComposerState {
            text: "durable on exit".into(),
            cursor: 15,
            revision: 3,
        })),
    );
    assert!(ui.try_shutdown(&tx));
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));

    let mut ui = Ui::new(&Config::default(), &snapshot);
    restored(&mut ui, "", 0, 0);
    ui.edit = Some(("lease".into(), 5));
    ui.draft.set_text("leased replacement");
    ui.autosave(&tx, true);
    assert!(rx.try_recv().is_err());
}

#[test]
fn pinned_commands_switch_focus_and_preserve_primary_text_and_cursor() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    ui.draft.set_text("primary text");
    ui.draft.set_cursor(4);
    ui.execute(Action::Commands, "keep", false, &snapshot, &tx)
        .unwrap();
    ui.palette_query.set_text("/settings status-color false");
    ui.submit(&snapshot, &tx).unwrap();
    assert!(ui.palette && ui.palette_pinned);
    assert!(!ui.interface.status_color);
    assert_eq!(ui.draft.text(), "primary text");
    assert_eq!(ui.draft.cursor(), 4);
    ui.execute(Action::FocusInput, "", false, &snapshot, &tx)
        .unwrap();
    press(&mut ui, KeyCode::Char('!'), &snapshot, &tx);
    assert_eq!(ui.draft.text(), "prim!ary text");
    ui.execute(Action::FocusInput, "", false, &snapshot, &tx)
        .unwrap();
    assert!(ui.commands_focus);
    ui.execute(Action::Exit, "", false, &snapshot, &tx).unwrap();
    assert!(!ui.palette && !ui.palette_pinned);
    assert_eq!(ui.draft.cursor(), 5);
}

#[test]
fn paused_send_auto_resumes_and_failure_preserves_exact_composer() {
    for fail in [false, true] {
        let mut snapshot = fixture_snapshot();
        snapshot.paused = true;
        let mut ui = Ui::new(&Config::default(), &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        ui.draft.set_text("unsent");
        ui.draft.set_cursor(2);
        ui.submit(&snapshot, &tx).unwrap();
        assert!(ui.pending_send.is_none() && ui.draft_busy);
        assert_eq!(ui.draft.text(), "unsent");
        assert_eq!(ui.draft.cursor(), 2);
        let work = rx.try_recv().unwrap();
        assert!(matches!(
            work.command,
            Command::SubmitInputAccumulated {
                resume: true,
                expected_paused: true,
                ..
            }
        ));
        if fail {
            complete(
                &mut ui,
                work.op,
                Err(anyhow::anyhow!("pause state changed")),
            );
            assert_eq!((ui.draft.text(), ui.draft.cursor()), ("unsent", 2));
        } else {
            complete(&mut ui, work.op, Ok(Reply::Ack));
            assert!(ui.draft.text().is_empty());
        }
        assert!(!ui.draft_busy);
    }
}

#[test]
fn pending_send_ack_never_erases_newer_input() {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.draft.set_text("original");
    ui.submit(&snapshot, &tx).unwrap();
    let work = rx.try_recv().unwrap();
    ui.draft.set_text("newer");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    complete(&mut ui, work.op, Ok(Reply::Ack));
    assert_eq!(ui.draft.text(), "newer");
    assert!(rx.try_recv().is_err());
}

#[test]
fn draft_mode_saves_without_dispatch_and_loading_copy_first_saves_unsent_input() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.mode = state::InputMode::Draft;
    ui.draft.set_text("save me");
    ui.submit(&snapshot, &tx).unwrap();
    let work = rx.try_recv().unwrap();
    assert!(matches!(work.command, Command::SaveDraft { .. }));
    let saved = crate::runtime::DraftRecord {
        id: "aaaa-one".into(),
        text: "save me".into(),
        revision: 1,
    };
    complete(&mut ui, work.op, Ok(Reply::Draft(saved)));
    assert!(ui.draft.text().is_empty());
    ui.view = View::Drafts;
    ui.draft.set_text("do not erase");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(matches!(work.command, Command::SaveDraft { text } if text == "do not erase"));
    assert_eq!(ui.draft.text(), "do not erase");
    complete(&mut ui, work.op, Err(anyhow::anyhow!("disk full")));
    assert_eq!(ui.draft.text(), "do not erase");
    assert_eq!(ui.view, View::Drafts);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    complete(
        &mut ui,
        work.op,
        Ok(Reply::Draft(crate::runtime::DraftRecord {
            id: "bbbb-two".into(),
            text: "do not erase".into(),
            revision: 1,
        })),
    );
    assert_eq!(ui.draft.text(), "save me");
    assert_eq!(ui.view, View::Root);
    assert!(ui.drafts.iter().any(|draft| draft.text == "do not erase"));
}

#[test]
fn draft_mode_round_trip_preserves_literal_slash_escape() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.mode = state::InputMode::Draft;
    ui.draft.set_text("//quit");
    ui.submit(&snapshot, &tx).unwrap();
    let work = rx.try_recv().unwrap();
    assert!(matches!(work.command, Command::SaveDraft { text } if text == "//quit"));
    complete(
        &mut ui,
        work.op,
        Ok(Reply::Draft(crate::runtime::DraftRecord {
            id: "aaaa-literal".into(),
            text: "//quit".into(),
            revision: 0,
        })),
    );
    ui.view = View::Drafts;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    ui.mode = state::InputMode::Steer;
    ui.submit(&snapshot, &tx).unwrap();
    assert!(!ui.exiting);
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::SubmitInputAccumulated { source, text, mode: crate::runtime::DeliveryMode::After, .. } if source == "//quit" && text == "/quit")
    );
}

#[test]
fn accumulated_draft_dispatch_binds_revision_and_pause_state_without_flags() {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.draft.set_text("primary remains");
    ui.drafts.push(crate::runtime::DraftRecord {
        id: "aaaa-draft".into(),
        text: "saved".into(),
        revision: 8,
    });
    assert!(
        ui.execute(Action::Send, "aaaa --force --hold", false, &snapshot, &tx)
            .is_err()
    );
    assert!(rx.try_recv().is_err());
    ui.execute(Action::Send, "aaaa", false, &snapshot, &tx)
        .unwrap();
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(work.command, Command::DispatchDraftAccumulated { id, revision: 8, expected_paused: true } if id == "aaaa-draft")
    );
    assert!(ui.pending_send.is_none());
    complete(&mut ui, work.op, Ok(Reply::Ack));
    assert_eq!(ui.draft.text(), "primary remains");
}

#[test]
fn macro_chain_waits_for_its_own_ack_and_stops_on_failure_without_erasing_composer() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.draft.set_text("preserved");
    ui.draft.set_cursor(3);
    ui.start_chain(vec!["/stop".into(), "/settings status-color true".into()])
        .unwrap();
    ui.advance_chain(&snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(matches!(
        work.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(
        matches!(&work.op, Op::Chain(op) if matches!(op.as_ref(), Op::Stop { clear_draft: false }))
    );
    complete(&mut ui, Op::Inputs, Ok(Reply::Inputs(vec![])));
    ui.advance_chain(&snapshot, &tx);
    assert!(!ui.interface.status_color);
    complete(
        &mut ui,
        work.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.interface.status_color);
    assert_eq!(ui.draft.text(), "preserved");
    assert_eq!(ui.draft.cursor(), 3);
    ui.start_chain(vec!["/stop".into(), "/settings status-color false".into()])
        .unwrap();
    ui.advance_chain(&snapshot, &tx);
    let work = rx.try_recv().unwrap();
    complete(&mut ui, work.op, Err(anyhow::anyhow!("unconfirmed")));
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.interface.status_color);
    assert!(ui.command_chain.is_empty());
}

#[test]
fn bindings_editor_captures_valid_keys_and_changes_macros_immediately() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    ui.execute(Action::Bindings, "", false, &snapshot, &tx)
        .unwrap();
    press(&mut ui, KeyCode::Char('n'), &snapshot, &tx);
    assert!(ui.binding_capture);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            100,
            20
        )
        .is_err()
    );
    press(&mut ui, KeyCode::F(2), &snapshot, &tx);
    assert_eq!(ui.binding_key.as_deref(), Some("f2"));
    ui.binding_commands
        .set_text("/settings status minimal; /stop");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(
        ui.keys.compile_macros().unwrap()[0].1,
        ["/settings status minimal", "/stop"]
    );
    ui.execute(Action::Unbind, "f2", false, &snapshot, &tx)
        .unwrap();
    assert!(ui.keys.compile_macros().unwrap().is_empty());
}

#[tokio::test]
async fn binding_list_d_saves_whole_keymap_and_deletions_without_clearing_fields() {
    let snapshot = fixture_snapshot();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("preferences.toml");
    std::fs::write(&path, "[interface]\nstatus_line = false\n").unwrap();
    let mut config = Config::default();
    config.keys.history = vec!["f4".into()];
    config.keys.macros = vec![
        crate::commands::CommandBinding {
            key: "f2".into(),
            commands: vec!["/settings status minimal".into()],
        },
        crate::commands::CommandBinding {
            key: "f3".into(),
            commands: vec!["/stop".into()],
        },
    ];
    crate::settings::save_keys(&path, &config.keys).unwrap();
    let mut ui = Ui::new(&config, &snapshot);
    ui.preferences_path = path.clone();
    ui.view = View::Bindings;
    ui.draft.set_text("protected primary");
    ui.draft.set_cursor(4);
    ui.palette = true;
    ui.palette_pinned = true;
    ui.commands_focus = false;
    ui.palette_query.set_text("/settings status-color false");
    ui.palette_query.set_cursor(10);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    press(&mut ui, KeyCode::Delete, &snapshot, &tx);
    assert_eq!(ui.keys.macros.len(), 1);
    press(&mut ui, KeyCode::Char('d'), &snapshot, &tx);
    let task_id = ui.bindings_task.as_ref().unwrap().id();
    press(&mut ui, KeyCode::Char('d'), &snapshot, &tx);
    assert_eq!(ui.bindings_task.as_ref().unwrap().id(), task_id);
    ui.finish_pending_bindings().await.unwrap();
    assert!(ui.palette && ui.palette_pinned && !ui.commands_focus);
    assert_eq!(ui.draft.text(), "protected primary");
    assert_eq!(ui.draft.cursor(), 4);
    assert_eq!(ui.palette_query.text(), "/settings status-color false");
    assert_eq!(ui.palette_query.cursor(), 10);
    assert_eq!(ui.notice.as_ref().unwrap().0, "Binding defaults saved");
    let saved = Config::load_with_preferences(None, &path).unwrap();
    assert_eq!(saved.keys.history, ["f4"]);
    assert_eq!(saved.keys.macros, ui.keys.macros);
    assert_eq!(saved.keys.macros[0].key, "f3");
    assert!(!saved.interface.status_line);
    assert!(saved.interface.status_color);
    assert_eq!(saved.interface.status_preset, StatusPreset::Bars);
    ui.flush_notices(&tx);
    complete(&mut ui, rx.try_recv().unwrap().op, Ok(Reply::Ack));
    ui.flush_notices(&tx);
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::RecordAction { text, category, level: NoticeLevel::Info } if text == "Binding defaults saved" && category == "bindings")
    );
}

#[tokio::test]
async fn binding_list_d_rejects_missing_path_and_busy_preferences_and_retains_fields_on_failure() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Bindings;
    ui.palette = true;
    ui.palette_pinned = true;
    ui.palette_query.set_text("pinned command");
    ui.palette_query.set_cursor(3);
    ui.draft.set_text("unsent primary");
    ui.draft.set_cursor(2);
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
    assert!(
        ui.key(key, &snapshot, &tx, 100, 20)
            .unwrap_err()
            .to_string()
            .contains("preferences path")
    );
    assert!(ui.bindings_task.is_none());
    let directory = tempfile::tempdir().unwrap();
    ui.preferences_path = directory.path().to_owned();
    ui.saving_defaults = true;
    assert!(
        ui.key(key, &snapshot, &tx, 100, 20)
            .unwrap_err()
            .to_string()
            .contains("pending preferences save")
    );
    assert!(ui.bindings_task.is_none());
    ui.saving_defaults = false;
    ui.defaults_task = Some(tokio::task::spawn_blocking(|| {
        Ok(Change::StatusColor(false))
    }));
    assert!(ui.key(key, &snapshot, &tx, 100, 20).is_err());
    ui.defaults_task.take().unwrap().await.unwrap().unwrap();
    ui.key(key, &snapshot, &tx, 100, 20).unwrap();
    assert!(ui.finish_pending_bindings().await.is_err());
    assert!(ui.palette && ui.palette_pinned);
    assert_eq!(ui.palette_query.text(), "pinned command");
    assert_eq!(ui.palette_query.cursor(), 3);
    assert_eq!(ui.draft.text(), "unsent primary");
    assert_eq!(ui.draft.cursor(), 2);
    assert!(ui.keys.macros.is_empty());
}

#[tokio::test]
async fn binding_default_chain_waits_for_worker_and_keeps_primary_caret() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let directory = tempfile::tempdir().unwrap();
    ui.preferences_path = directory.path().join("preferences.toml");
    ui.draft.set_text("unchanged primary");
    ui.draft.set_cursor(4);
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    ui.start_chain(vec![
        "/bind f2 /stop --default".into(),
        "/settings status minimal".into(),
    ])
    .unwrap();
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.bindings_task.is_some());
    ui.advance_chain(&snapshot, &tx);
    assert_eq!(ui.interface.status_preset, StatusPreset::Bars);
    ui.finish_pending_bindings().await.unwrap();
    ui.advance_chain(&snapshot, &tx);
    assert_eq!(ui.interface.status_preset, StatusPreset::Minimal);
    assert_eq!(ui.draft.text(), "unchanged primary");
    assert_eq!(ui.draft.cursor(), 4);
    assert_eq!(ui.keys.macros[0].commands, ["/stop"]);
    let preferences = crate::config::Config::load(Some(&ui.preferences_path)).unwrap();
    assert_eq!(preferences.keys.macros[0].key, "f2");
}

#[tokio::test]
async fn binding_default_ack_preserves_newer_secondary_input_and_failure_retains_editor() {
    let snapshot = fixture_snapshot();
    let directory = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::channel(OUTSTANDING);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.preferences_path = directory.path().join("preferences.toml");
    ui.draft.set_text("primary preserved");
    ui.draft.set_cursor(3);
    ui.palette = true;
    ui.palette_query.set_text("/bind f2 /stop --default");
    ui.submit(&snapshot, &tx).unwrap();
    ui.palette_query.set_text("/settings status minimal");
    ui.finish_pending_bindings().await.unwrap();
    assert!(ui.palette);
    assert_eq!(ui.palette_query.text(), "/settings status minimal");
    assert_eq!(ui.draft.text(), "primary preserved");
    assert_eq!(ui.draft.cursor(), 3);

    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.preferences_path = directory.path().to_owned();
    ui.view = View::Bindings;
    ui.binding_key = Some("f3".into());
    ui.binding_commands.set_text("/stop");
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        &snapshot,
        &tx,
        100,
        20,
    )
    .unwrap();
    assert!(ui.finish_pending_bindings().await.is_err());
    assert_eq!(ui.binding_key.as_deref(), Some("f3"));
    assert_eq!(ui.binding_commands.text(), "/stop");
    assert!(ui.keys.macros.is_empty());
}

#[test]
fn paused_macro_send_suspends_chain_until_dispatch_ack() {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.drafts.push(crate::runtime::DraftRecord {
        id: "aaaa-one".into(),
        text: "saved".into(),
        revision: 3,
    });
    ui.start_chain(vec![
        "/send aaaa".into(),
        "/settings status-color false".into(),
    ])
    .unwrap();
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.pending_send.is_none() && ui.draft_busy);
    let work = rx.try_recv().unwrap();
    assert!(matches!(
        work.command,
        Command::DispatchDraftAccumulated {
            expected_paused: true,
            ..
        }
    ));
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.interface.status_color);
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.interface.status_color);
    complete(&mut ui, work.op, Ok(Reply::Ack));
    ui.advance_chain(&snapshot, &tx);
    assert!(!ui.interface.status_color);
}

#[test]
fn journal_discards_late_artifact_reply_and_pages_back_at_recorded_utf8_boundaries() {
    use crate::runtime::{ActionRecord, ArtifactChunk, ArtifactRecord};
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.view = View::History;
    ui.full_history = true;
    ui.journal.append_actions(
        (1..=2)
            .map(|seq| ActionRecord {
                seq,
                id: format!("action-{seq}"),
                group_id: "group".into(),
                category: "tool".into(),
                label: "tool output".into(),
                status: "done".into(),
                output: Some(ArtifactRecord {
                    id: format!("artifact-{seq}"),
                    path: "/output".into(),
                    bytes: 30_000,
                    incomplete: false,
                }),
            })
            .collect(),
    );
    let chunk = |id: &str, text: &str, next_offset| ArtifactChunk {
        id: id.into(),
        text: text.into(),
        next_offset,
        eof: false,
        path: "/output".into(),
        incomplete: false,
    };
    ui.journal.selected = 1;
    complete(
        &mut ui,
        Op::Artifact {
            id: "artifact-1".into(),
            offset: 0,
            generation: 0,
        },
        Ok(Reply::Artifact(chunk("artifact-1", "stale", 5))),
    );
    assert!(ui.journal.preview.is_empty());
    complete(
        &mut ui,
        Op::Artifact {
            id: "artifact-2".into(),
            offset: 0,
            generation: 0,
        },
        Ok(Reply::Artifact(chunk("artifact-2", "first", 8190))),
    );
    complete(
        &mut ui,
        Op::Artifact {
            id: "artifact-2".into(),
            offset: 8190,
            generation: 0,
        },
        Ok(Reply::Artifact(chunk("artifact-2", "second", 16381))),
    );
    ui.history_browser.focus = history::browser::Focus::Preview;
    press(&mut ui, KeyCode::PageUp, &snapshot, &tx);
    assert!(matches!(
        rx.try_recv().unwrap().command,
        Command::ReadArtifactBefore {
            before: 8190,
            limit: 8192,
            ..
        }
    ));
    assert_eq!(ui.journal.preview_text, "second");
}

#[test]
fn draft_completion_includes_preview_and_no_delivery_flags() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.drafts.push(crate::runtime::DraftRecord {
        id: "aaaa-one".into(),
        text: "line one\nline two".into(),
        revision: 1,
    });
    ui.draft.set_text("/send ");
    let menu = ui.completion(&snapshot).unwrap();
    assert_eq!(menu.items[0].value, "aaaa-one");
    assert_eq!(menu.items[0].description, "line one line two");
    ui.draft.set_text("/draft save ");
    assert!(ui.completion(&snapshot).is_none());
    ui.draft.set_text("/draft delete ");
    assert_eq!(ui.completion(&snapshot).unwrap().items[0].value, "aaaa-one");
    ui.draft.set_text("/send aaaa-one ");
    assert!(ui.completion(&snapshot).is_none());
}

#[test]
fn semantic_actions_are_explicit_and_native_groups_append_without_rewriting_plain_notices() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let mut rows = Vec::new();
    for (text, category) in [
        ("status set to **custom**", Some("settings")),
        ("color enabled", Some("settings")),
        ("paused", Some("queue")),
        ("status set to **custom**", None),
    ] {
        if let Some(category) = category {
            ui.record_action(text, NoticeLevel::Info, category);
        } else {
            ui.record_notice(text, NoticeLevel::Info);
        }
        ui.flush_notices(&tx);
        let work = rx.try_recv().unwrap();
        match &work.command {
            Command::RecordAction {
                category: actual, ..
            } => assert_eq!(Some(actual.as_str()), category),
            Command::RecordNotice { .. } => assert!(category.is_none()),
            _ => panic!("unexpected notice work"),
        }
        complete(&mut ui, work.op, Ok(Reply::Ack));
        assert_eq!(ui.live_notices.back().unwrap().text, text);
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(
        rows,
        [
            "",
            "\u{25cf} Settings",
            "  \u{251c} status set to **custom**",
            "  \u{2514} color enabled",
            "",
            "\u{25cf} Queue",
            "  \u{2514} paused",
            "",
            "\u{25cf} Events"
        ]
    );
    assert_eq!(
        ui.native_tail_rows(80)[0].to_string(),
        "  \u{2514} status set to **custom**"
    );
    assert!(ui.action_categories.is_empty());
    ui.publish_with(&snapshot, 80, |_| panic!("published actions replayed"))
        .unwrap();
}

#[test]
fn native_action_publish_failure_retains_receipt_and_group_metadata_for_retry() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.record_action("saved", NoticeLevel::Info, "drafts");
    ui.flush_notices(&tx);
    complete(&mut ui, rx.try_recv().unwrap().op, Ok(Reply::Ack));
    assert!(
        ui.publish_with(&snapshot, 80, |_| anyhow::bail!("terminal write failed"))
            .is_err()
    );
    assert_eq!(ui.live_notices.len(), 1);
    assert_eq!(ui.action_categories.len(), 1);
    let mut rows = Vec::new();
    ui.publish_with(&snapshot, 80, |batch| {
        rows.extend(batch.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    assert!(rows.iter().any(|row| row == "\u{25cf} Drafts"));
    assert!(ui.action_categories.is_empty());
}

fn native_notice(seq: i64, text: &str, category: Option<&str>) -> crate::runtime::NativeEntry {
    crate::runtime::NativeEntry {
        entry: Saved {
            seq,
            attempt_id: String::new(),
            kind: "notice".into(),
            text: text.into(),
            status: "info".into(),
        },
        category: category.map(str::to_owned),
    }
}

#[test]
fn restored_native_categories_are_read_only_and_legacy_phrases_are_generic_events() {
    use crate::storage::Store;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    for (index, category) in [
        Some("settings"),
        Some("settings"),
        Some("queue"),
        Some("drafts"),
        Some("bindings"),
        Some("delivery"),
        Some("error"),
        None,
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let text = format!("status set to **custom** {index}");
        let command = match category {
            Some(category) => Command::RecordAction {
                text,
                category: category.into(),
                level: NoticeLevel::Info,
            },
            None => Command::RecordNotice {
                text,
                level: NoticeLevel::Info,
            },
        };
        store
            .apply(&format!("notice-{index}"), &command, &config, None)
            .unwrap();
    }
    let plain = store.transcript(None, None, 50).unwrap();
    drop(store);
    let store = Store::open(&path, &config).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let counts = || {
        db.query_row(
            "SELECT (SELECT COUNT(*) FROM history), (SELECT COUNT(*) FROM actions),
                    (SELECT COUNT(*) FROM events), (SELECT COUNT(*) FROM command_results)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .unwrap()
    };
    let original_counts = counts();
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&config, &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.poll(&tx);
    let work = rx.try_recv().unwrap();
    let Command::NativeTranscript {
        before,
        after,
        limit,
    } = work.command
    else {
        panic!("root must read durable native metadata")
    };
    assert_eq!((before, after), (None, None));
    let mut entries = store.native_transcript(before, after, limit).unwrap();
    // Unknown metadata is defensive input coverage; current writes reject it.
    entries[7].category = Some("unknown".into());
    complete(&mut ui, work.op, Ok(Reply::NativeTranscript(entries)));
    let mut rows = Vec::new();
    while !ui.pending.is_empty() {
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(
        rows.iter()
            .filter(|row| row.starts_with('\u{25cf}'))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "\u{25cf} Settings",
            "\u{25cf} Queue",
            "\u{25cf} Drafts",
            "\u{25cf} Bindings",
            "\u{25cf} Delivery",
            "\u{25cf} Errors",
            "\u{25cf} Events",
        ]
    );
    assert!(rows.contains(&"  \u{251c} status set to **custom** 0".into()));
    assert!(rows.contains(&"  \u{2514} status set to **custom** 1".into()));
    assert_eq!(
        ui.native_tail_rows(80)[0].to_string(),
        "  \u{2514} status set to **custom** 8"
    );
    assert!(ui.action_categories.is_empty());
    assert_eq!(store.transcript(None, None, 50).unwrap(), plain);
    assert_eq!(counts(), original_counts);
    // Sidebar history still consumes the plain durable entries, independently of the held child.
    ui.view = View::History;
    ui.full_history = true;
    complete(
        &mut ui,
        Op::History(history::Load::Latest, false),
        Ok(Reply::History(plain)),
    );
    assert!(
        ui.history
            .lines(80, 20)
            .iter()
            .any(|row| row.to_string().contains("status set to **custom**"))
    );
    ui.view = View::Root;
    ui.full_history = false;
    ui.publish_with(&snapshot, 80, |_| {
        panic!("returning from history replayed a child")
    })
    .unwrap();
    assert_eq!(
        ui.native_tail_rows(80)[0].to_string(),
        "  \u{2514} status set to **custom** 8"
    );
}

#[test]
fn restored_page_one_groups_hold_real_last_child_until_next_user_message() {
    let snapshot = fixture_snapshot();
    let mut config = Config::default();
    config.limits.history_page = 1;
    let mut ui = Ui::new(&config, &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let mut rows = Vec::new();
    for seq in 1..=2 {
        ui.poll(&tx);
        let work = rx.try_recv().unwrap();
        assert!(
            matches!(work.command, Command::NativeTranscript { after, limit: 1, .. } if after == ui.published)
        );
        complete(
            &mut ui,
            work.op,
            Ok(Reply::NativeTranscript(vec![native_notice(
                seq,
                &format!("result {seq}"),
                Some("settings"),
            )])),
        );
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(ui.published, Some(seq));
        assert!(ui.pending.is_empty() && ui.action_categories.is_empty());
        assert_eq!(
            ui.native_tail_rows(80)[0].to_string(),
            format!("  \u{2514} result {seq}")
        );
        ui.publish_with(&snapshot, 80, |_| panic!("held child was dumped on redraw"))
            .unwrap();
        while let Ok(work) = rx.try_recv() {
            assert!(matches!(work.command, Command::Events { .. }));
            complete(&mut ui, work.op, Ok(Reply::Events(vec![])));
        }
    }
    assert_eq!(rows, ["", "\u{25cf} Settings", "  \u{251c} result 1"]);
    ui.pending.insert(
        3,
        Saved {
            seq: 3,
            attempt_id: "next".into(),
            kind: "user".into(),
            text: "hello".into(),
            status: "accepted".into(),
        },
    );
    ui.publish_with(&snapshot, 80, |batch| {
        rows.extend(batch.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        rows,
        [
            "",
            "\u{25cf} Settings",
            "  \u{251c} result 1",
            "  \u{2514} result 2",
            "",
            "\u{2503} hello"
        ]
    );
    assert!(ui.native_tail_rows(80).is_empty());
    assert!(!rows.iter().any(|row| row.trim() == "\u{2514}\u{2500}"));
}

#[test]
fn failed_child_boundary_and_exit_publications_preserve_held_children_exactly() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    let mut rows = Vec::new();
    for (index, category) in ["settings", "settings", "queue"].into_iter().enumerate() {
        ui.record_action(format!("result {index}"), NoticeLevel::Info, category);
        ui.flush_notices(&tx);
        complete(&mut ui, rx.try_recv().unwrap().op, Ok(Reply::Ack));
        if index > 0 {
            let held = ui.native_tail_rows(80);
            assert!(
                ui.publish_with(&snapshot, 80, |_| anyhow::bail!("write failed"))
                    .is_err()
            );
            assert_eq!(ui.native_tail_rows(80), held);
            assert_eq!(ui.live_notices.len(), 1);
            assert_eq!(ui.action_categories.len(), 1);
        }
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert!(ui.live_notices.is_empty() && ui.action_categories.is_empty());
    }
    assert_eq!(
        rows,
        [
            "",
            "\u{25cf} Settings",
            "  \u{251c} result 0",
            "  \u{2514} result 1",
            "",
            "\u{25cf} Queue"
        ]
    );
    ui.exiting = true;
    ui.quit_pending = true;
    assert!(!ui.try_shutdown(&tx));
    assert!(rx.try_recv().is_err());
    assert!(
        ui.publish_with(&snapshot, 80, |_| anyhow::bail!("exit write failed"))
            .is_err()
    );
    assert_eq!(
        ui.native_tail_rows(80)[0].to_string(),
        "  \u{2514} result 2"
    );
    ui.publish_with(&snapshot, 80, |batch| {
        rows.extend(batch.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    assert_eq!(rows.last().unwrap(), "  \u{2514} result 2");
    assert!(ui.native_tail_rows(80).is_empty());
    ui.publish_with(&snapshot, 80, |_| panic!("exit child replayed"))
        .unwrap();
    assert!(ui.try_shutdown(&tx));
}

#[test]
fn quit_waits_for_unstaged_restored_notices_and_their_last_child() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    complete(
        &mut ui,
        Op::Tail,
        Ok(Reply::NativeTranscript(vec![native_notice(
            1,
            "restored result",
            Some("settings"),
        )])),
    );
    ui.execute(Action::Quit, "", false, &snapshot, &tx).unwrap();
    assert!(ui.quit_pending && ui.exiting);
    assert!(rx.try_recv().is_err());
    assert!(!ui.try_shutdown(&tx));
    let mut rows = Vec::new();
    for _ in 0..2 {
        assert!(!ui.try_shutdown(&tx));
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(
        rows,
        ["", "\u{25cf} Settings", "  \u{2514} restored result"]
    );
    assert!(ui.try_shutdown(&tx));
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
}

#[test]
fn every_notice_admission_path_uses_metadata_and_keeps_streaming_cursor_independent() {
    for path in ["ordered", "running", "receipt"] {
        let mut snapshot = fixture_snapshot();
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "active".into(),
            text: "first\nsecond\npartial".into(),
            durable_bytes: 0,
            truncated: false,
            message_id: String::new(),
        });
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.published = Some(1);
        let assistant = Saved {
            seq: 2,
            attempt_id: "active".into(),
            kind: "assistant".into(),
            text: "first\nsecond\npartial".into(),
            status: "running".into(),
        };
        if path != "ordered" {
            ui.pending.insert(2, assistant.clone());
            ui.native_offset = "first\n".len();
        }
        let entry = native_notice(3, "draft saved", Some("drafts"));
        if path == "receipt" {
            let (tx, mut rx) = mpsc::channel(OUTSTANDING);
            ui.record_action("draft saved", NoticeLevel::Info, "drafts");
            ui.flush_notices(&tx);
            complete(
                &mut ui,
                rx.try_recv().unwrap().op,
                Ok(Reply::Notice(entry.entry.clone())),
            );
        } else {
            complete(
                &mut ui,
                Op::Tail,
                Ok(Reply::NativeTranscript(vec![entry.clone()])),
            );
        }
        let offset = ui.native_offset;
        let mut rows = Vec::new();
        ui.publish_with(&snapshot, 12, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, ["", "\u{25cf} Drafts"], "{path}");
        assert_eq!(ui.native_offset, offset);
        assert_eq!(
            ui.native_tail_rows(12)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["  \u{2514} draft ", "    saved"]
        );
        assert!(ui.action_categories.is_empty());
        complete(&mut ui, Op::Tail, Ok(Reply::NativeTranscript(vec![entry])));
        if path == "ordered" {
            ui.pending.insert(
                4,
                Saved {
                    seq: 4,
                    kind: "user".into(),
                    text: "next".into(),
                    status: "accepted".into(),
                    attempt_id: "next".into(),
                },
            );
        }
        let held = ui.native_tail_rows(12);
        assert!(
            ui.publish_with(&snapshot, 12, |_| anyhow::bail!(
                "conversation write failed"
            ))
            .is_err()
        );
        assert_eq!(ui.native_tail_rows(12), held);
        assert_eq!(ui.native_offset, offset);
        ui.publish_with(&snapshot, 12, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert!(ui.native_tail_rows(12).is_empty());
        if path != "ordered" {
            assert_eq!(ui.native_offset, "first\nsecond\n".len());
            assert_eq!(ui.published, Some(1));
            assert!(rows.iter().any(|row| row == "  second"));
        }
        assert_eq!(
            rows.iter()
                .filter(|row| row.starts_with('\u{25cf}'))
                .count(),
            1
        );
        assert_eq!(
            rows.windows(2)
                .filter(|pair| pair[0] == "  \u{2514} draft " && pair[1] == "    saved")
                .count(),
            1
        );
    }
}

#[test]
fn late_notice_receipts_behind_cursor_are_not_lost_or_replayed() {
    for presented in [false, true] {
        let snapshot = fixture_snapshot();
        let mut ui = Ui::new(&Config::default(), &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        ui.record_action("late saved", NoticeLevel::Info, "settings");
        ui.flush_notices(&tx);
        let write = rx.try_recv().unwrap();
        let late = native_notice(3, "late saved", Some("settings"));
        let initial = if presented {
            late.clone()
        } else {
            native_notice(4, "other", Some("queue"))
        };
        complete(
            &mut ui,
            Op::Tail,
            Ok(Reply::NativeTranscript(vec![initial])),
        );
        let mut rows = Vec::new();
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        // Advance beyond the receipt, closing its group before the ACK arrives.
        ui.pending.insert(
            5,
            Saved {
                seq: 5,
                kind: "user".into(),
                text: "hello".into(),
                status: "accepted".into(),
                attempt_id: "next".into(),
            },
        );
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(ui.published, Some(5));
        complete(
            &mut ui,
            Op::Tail,
            Ok(Reply::NativeTranscript(vec![native_notice(
                6,
                "held newer",
                Some("queue"),
            )])),
        );
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        complete(&mut ui, write.op, Ok(Reply::Notice(late.entry)));
        assert_eq!(ui.live_notices.len(), usize::from(!presented));
        if !presented {
            ui.publish_with(&snapshot, 80, |batch| {
                rows.extend(batch.iter().map(ToString::to_string));
                Ok(())
            })
            .unwrap();
            assert_eq!(
                ui.native_tail_rows(80)[0].to_string(),
                "  \u{2514} late saved"
            );
            assert_eq!(ui.published, Some(6));
        } else {
            assert_eq!(
                ui.native_tail_rows(80)[0].to_string(),
                "  \u{2514} held newer"
            );
        }
        ui.exiting = true;
        ui.publish_with(&snapshot, 80, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            rows.iter().filter(|row| row.contains("late saved")).count(),
            1
        );
        assert_eq!(
            rows.iter().filter(|row| row.contains("held newer")).count(),
            1
        );
        assert!(ui.action_categories.is_empty());
    }
}

#[test]
fn macro_dispatch_keeps_one_shot_field_text_focus_and_primary_caret() {
    let mut snapshot = fixture_snapshot();
    snapshot.paused = true;
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.draft.set_text("protected primary");
    ui.draft.set_cursor(4);
    ui.palette = true;
    ui.palette_query.set_text("/settings status-color ");
    ui.palette_query.set_cursor(10);
    ui.drafts.push(crate::runtime::DraftRecord {
        id: "aaaa-one".into(),
        text: "saved".into(),
        revision: 2,
    });
    ui.start_chain(vec![
        "/send aaaa".into(),
        "/settings status-color true".into(),
    ])
    .unwrap();
    ui.advance_chain(&snapshot, &tx);
    complete(&mut ui, rx.try_recv().unwrap().op, Ok(Reply::Ack));
    ui.advance_chain(&snapshot, &tx);
    assert!(ui.palette && ui.commands_focus && !ui.palette_pinned);
    assert_eq!(ui.palette_query.text(), "/settings status-color ");
    assert_eq!(ui.palette_query.cursor(), 10);
    assert_eq!(ui.draft.text(), "protected primary");
    assert_eq!(ui.draft.cursor(), 4);
    assert!(ui.interface.status_color);
}

#[test]
fn unavailable_startup_restore_never_unlocks_or_accepts_edits() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, rx) = mpsc::channel(OUTSTANDING);
    drop(rx);
    ui.restore_composer(&tx);
    ui.append("must not be accepted");
    press(&mut ui, KeyCode::Char('x'), &snapshot, &tx);
    assert!(ui.draft_busy && ui.draft.text().is_empty());
    assert!(
        ui.execute(Action::ClearDraft, "", false, &snapshot, &tx)
            .is_err()
    );
}

#[test]
fn artifact_focus_changes_keep_current_page_and_old_generation_receipts_are_ignored() {
    use crate::runtime::{ActionRecord, ArtifactChunk, ArtifactRecord};
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.view = View::History;
    ui.full_history = true;
    ui.journal.append_actions(
        (1..=2)
            .map(|seq| ActionRecord {
                seq,
                id: format!("action-{seq}"),
                group_id: String::new(),
                category: "tool".into(),
                label: "output".into(),
                status: "done".into(),
                output: Some(ArtifactRecord {
                    id: format!("artifact-{seq}"),
                    path: "/output".into(),
                    bytes: 20_000,
                    incomplete: false,
                }),
            })
            .collect(),
    );
    ui.history_browser.focus = history::browser::Focus::List;
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    let first_read = rx.try_recv().unwrap();
    // Selection is near the older boundary, so a pagination read comes first.
    let read = if matches!(first_read.op, Op::Artifact { .. }) {
        first_read
    } else {
        complete(&mut ui, first_read.op, Ok(Reply::Actions(vec![])));
        rx.try_recv().unwrap()
    };
    let generation = match &read.op {
        Op::Artifact { generation, .. } => *generation,
        _ => panic!("artifact read required"),
    };
    complete(
        &mut ui,
        read.op,
        Ok(Reply::Artifact(ArtifactChunk {
            id: "artifact-2".into(),
            text: "page two".into(),
            next_offset: 16_384,
            eof: false,
            path: "/output".into(),
            incomplete: false,
        })),
    );
    press(&mut ui, KeyCode::Right, &snapshot, &tx);
    assert_eq!(ui.journal.preview_text, "page two");
    assert!(rx.try_recv().is_err());
    complete(
        &mut ui,
        Op::Artifact {
            id: "artifact-2".into(),
            offset: 0,
            generation: generation.wrapping_sub(1),
        },
        Ok(Reply::Artifact(ArtifactChunk {
            id: "artifact-2".into(),
            text: "stale page".into(),
            next_offset: 8192,
            eof: false,
            path: "/output".into(),
            incomplete: false,
        })),
    );
    assert_eq!(ui.journal.preview_text, "page two");
}
use crossterm::event::KeyEvent;
use ratatui::{
    Terminal,
    style::{Color, Modifier},
};

#[test]
fn paste_budget_never_keeps_only_a_prefix_of_an_accent_or_joined_emoji() {
    let snapshot = fixture_snapshot();
    for (limit, text, expected) in [
        (2, "ae\u{301}", "a"),
        (2, "ae\x1b\u{301}", "a"),
        (5, "a\u{1f469}\u{200d}\u{1f4bb}", "a"),
        (4, "ae\u{301}", "ae\u{301}"),
    ] {
        let mut config = Config::default();
        config.limits.message_bytes = limit;
        let mut ui = Ui::new(&config, &snapshot);
        ui.append(text);
        assert_eq!(ui.draft.text(), expected);
    }
}

#[tokio::test]
async fn closing_palette_during_failed_default_save_retains_the_submitted_command() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    ui.draft.set_text("ordinary draft");
    ui.draft.set_cursor(4);
    ui.palette = true;
    ui.palette_query
        .set_text("/settings status-color true --default");
    let cursor = ui.palette_query.cursor();
    ui.saving_defaults = true;
    ui.defaults_palette = true;
    ui.draft_busy = true;
    let (release, wait) = std::sync::mpsc::channel();
    ui.defaults_task = Some(tokio::task::spawn_blocking(move || {
        wait.recv().unwrap();
        anyhow::bail!("fixture: save failed");
    }));
    ui.ctrl_c(Instant::now(), &snapshot, &tx);
    assert!(!ui.palette);
    assert_eq!(
        ui.palette_query.text(),
        "/settings status-color true --default"
    );
    release.send(()).unwrap();
    assert!(ui.finish_pending_defaults().await.is_err());
    assert!(ui.palette && !ui.draft_busy && !ui.saving_defaults);
    assert_eq!(ui.palette_query.cursor(), cursor);
    assert_eq!(ui.draft.text(), "ordinary draft");
    assert_eq!(ui.draft.cursor(), 4);
    assert!(!ui.interface.status_color);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn exit_waits_for_pending_default_result_and_propagates_failure() {
    for fail in [false, true] {
        let mut ui = Ui::new(&uncolored_config(), &fixture_snapshot());
        ui.exiting = true;
        ui.saving_defaults = true;
        ui.draft_busy = true;
        let (release, wait) = std::sync::mpsc::channel();
        ui.defaults_task = Some(tokio::task::spawn_blocking(move || {
            wait.recv().unwrap();
            if fail {
                anyhow::bail!("fixture: unconfirmed preference write");
            }
            Ok(Change::StatusColor(true))
        }));
        let mut finish = Box::pin(ui.finish_pending_defaults());
        tokio::select! {
            result = &mut finish => panic!("exit finished before save acknowledgement: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        release.send(()).unwrap();
        assert_eq!(finish.await.is_err(), fail);
        assert_eq!(ui.interface.status_color, !fail);
        assert!(!ui.saving_defaults && !ui.draft_busy && ui.defaults_task.is_none());
    }
}

fn fixture_snapshot() -> Snapshot {
    let now = Instant::now();
    Snapshot {
        session_id: "session-hidden-in-root".into(),
        agent_id: "agent-hidden-in-root".into(),
        conversation_id: "conversation-hidden-in-root".into(),
        phase: Phase::Idle,
        phase_started: now,
        last_event: now,
        active: None,
        queue: Vec::new(),
        paused: false,
        recovered: false,
        cursor: 42,
        error: None,
    }
}

#[test]
fn notices_are_plain_concise_records_not_assistant_markdown_or_status_tags() {
    for (status, color) in [("info", 250), ("error", 252), ("status", 245)] {
        let entry = Saved {
            seq: 1,
            attempt_id: String::new(),
            kind: "notice".into(),
            text: "status set to **custom**".into(),
            status: status.into(),
        };
        let rows = message_rows(&entry, 80, 1024);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].to_string(), "  status set to **custom**");
        assert_eq!(rows[0].style.fg, Some(Color::Indexed(color)));
        assert_eq!(rows[1].to_string(), "");
    }
}

#[test]
fn native_notices_during_streaming_print_once_without_advancing_response_cursor() {
    let mut snapshot = fixture_snapshot();
    snapshot.active = Some(crate::runtime::ActiveView {
        attempt_id: "active".into(),
        text: "first line\nunfinished".into(),
        durable_bytes: 0,
        truncated: false,
        message_id: String::new(),
    });
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.published = Some(1);
    ui.pending.insert(
        2,
        Saved {
            seq: 2,
            attempt_id: "active".into(),
            kind: "assistant".into(),
            text: "first line\n".into(),
            status: "running".into(),
        },
    );
    ui.pending.insert(
        3,
        Saved {
            seq: 3,
            attempt_id: String::new(),
            kind: "notice".into(),
            text: "status-color enabled".into(),
            status: "info".into(),
        },
    );
    let mut output = Vec::new();
    ui.publish_with(&snapshot, 80, |rows| {
        output.extend(rows.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    assert_eq!(output, ["", "\u{25cf} Events"]);
    assert_eq!(
        ui.native_tail_rows(80)[0].to_string(),
        "  \u{2514} status-color enabled"
    );
    assert_eq!(ui.published, Some(1));
    assert_eq!(ui.native_offset, 0);
    ui.publish_with(&snapshot, 80, |rows| {
        output.extend(rows.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    assert_eq!(ui.native_offset, "first line\n".len());
    snapshot.active = None;
    let entry = ui.pending.get_mut(&2).unwrap();
    entry.text = "first line\nunfinished done".into();
    entry.status = "completed".into();
    ui.publish_with(&snapshot, 80, |rows| {
        output.extend(rows.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    ui.publish_with(&snapshot, 80, |_| panic!("notice replayed"))
        .unwrap();
    assert_eq!(ui.published, Some(3));
    assert_eq!(
        output
            .iter()
            .filter(|row| row.contains("status-color enabled"))
            .count(),
        1
    );
    assert_eq!(output.last().unwrap(), "  unfinished done");
}

#[test]
fn native_blocks_have_one_separator_and_no_trailing_blank_before_composer() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    for (seq, kind, text) in [
        (1, "user", "hello"),
        (2, "assistant", "reply"),
        (3, "notice", "enabled"),
    ] {
        ui.pending.insert(
            seq,
            Saved {
                seq,
                attempt_id: if kind == "notice" {
                    String::new()
                } else {
                    "attempt".into()
                },
                kind: kind.into(),
                text: text.into(),
                status: "completed".into(),
            },
        );
    }
    let mut output = Vec::new();
    for _ in 0..3 {
        ui.publish_with(&snapshot, 80, |rows| {
            output.extend(rows.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(
        output,
        ["", "\u{2503} hello", "", "  reply", "", "\u{25cf} Events"]
    );
    assert_eq!(ui.native_tail_rows(80)[0].to_string(), "  \u{2514} enabled");
}

#[test]
fn notice_receipts_publish_beyond_page_boundaries_during_an_unfinished_response() {
    for page in [1, 50] {
        let mut config = Config::default();
        config.limits.history_page = page;
        let mut snapshot = fixture_snapshot();
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "active".into(),
            text: String::new(),
            durable_bytes: 0,
            truncated: false,
            message_id: String::new(),
        });
        let mut ui = Ui::new(&config, &snapshot);
        ui.published = Some(1);
        ui.pending.insert(
            2,
            Saved {
                seq: 2,
                attempt_id: "active".into(),
                kind: "assistant".into(),
                text: String::new(),
                status: "running".into(),
            },
        );
        let mut output = Vec::new();
        for seq in 3..103 {
            ui.outstanding = 1;
            ui.notice_busy = true;
            ui.completed_with(
                Done {
                    op: Op::Notice,
                    result: Ok(Reply::Notice(Saved {
                        seq,
                        attempt_id: String::new(),
                        kind: "notice".into(),
                        text: format!("setting set to {seq}"),
                        status: "info".into(),
                    })),
                },
                |_| Ok(()),
            )
            .unwrap();
            ui.publish_with(&snapshot, 80, |rows| {
                output.extend(rows.iter().map(ToString::to_string));
                Ok(())
            })
            .unwrap();
            assert_eq!(ui.published, Some(1));
            assert_eq!(ui.native_offset, 0);
            assert_eq!(ui.printed_notices.len(), 1);
        }
        assert_eq!(
            output
                .iter()
                .filter(|row| row.contains("setting set to "))
                .count(),
            99
        );
        assert_eq!(
            ui.native_tail_rows(80)[0].to_string(),
            "  \u{2514} setting set to 102"
        );
        snapshot.active = None;
        let entry = ui.pending.get_mut(&2).unwrap();
        entry.text = "answer".into();
        entry.status = "completed".into();
        ui.publish_with(&snapshot, 80, |_| Ok(())).unwrap();
        ui.pending.insert(
            3,
            Saved {
                seq: 3,
                attempt_id: String::new(),
                kind: "notice".into(),
                text: "setting set to 3".into(),
                status: "info".into(),
            },
        );
        ui.publish_with(&snapshot, 80, |_| {
            panic!("already printed receipt replayed")
        })
        .unwrap();
        assert_eq!(ui.published, Some(102));
        assert!(ui.printed_notices.is_empty());
    }
}

fn complete(ui: &mut Ui, op: Op, result: anyhow::Result<Reply>) -> bool {
    // Manual Tail fixtures use plain history; the real worker uses NativeTranscript.
    let result = match (matches!(op, Op::Tail), result) {
        (true, Ok(Reply::History(entries))) => Ok(Reply::NativeTranscript(
            entries
                .into_iter()
                .map(|entry| crate::runtime::NativeEntry {
                    entry,
                    category: None,
                })
                .collect(),
        )),
        (_, result) => result,
    };
    // The test worker returns authoritative row receipts for notice writes.
    let result = if matches!(op, Op::Notice) && matches!(&result, Ok(Reply::Ack)) {
        let (text, level) = ui.notice_in_flight.clone().unwrap();
        Ok(Reply::Notice(Saved {
            seq: ui
                .live_notices
                .back()
                .map_or(0, |entry| entry.seq)
                .max(
                    ui.printed_notices
                        .iter()
                        .map(|(_, end)| *end)
                        .max()
                        .unwrap_or(0),
                )
                .max(ui.published.unwrap_or(0))
                + 1,
            attempt_id: String::new(),
            kind: "notice".into(),
            text,
            status: match level {
                NoticeLevel::Info => "info",
                NoticeLevel::Error => "error",
                NoticeLevel::Status => "status",
            }
            .into(),
        }))
    } else {
        result
    };
    let stopping = ui
        .completed_with(Done { op, result }, |full| {
            assert!(!full);
            Ok(())
        })
        .unwrap();
    if ui.exiting {
        while !ui.live_notices.is_empty() {
            ui.publish_with(&fixture_snapshot(), 80, |_| Ok(()))
                .unwrap();
        }
        ui.publish_with(&fixture_snapshot(), 80, |_| Ok(()))
            .unwrap();
    }
    stopping
}

#[test]
fn notices_are_compact_bounded_and_deduplicated_until_ack_or_error_dismissal() {
    let mut ui = Ui::new(&Config::default(), &fixture_snapshot());
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.notify("an ephemeral hint", false);
    assert!(ui.pending_notices.is_empty());
    let error = format!(" bad\n\t\r\0\x1b\u{2028}\u{2029}{}", "\u{e9}".repeat(1024));
    ui.notify(error.clone(), true);
    ui.notify(error.clone(), true);
    assert_eq!(ui.pending_notices.len(), 1);
    let text = &ui.pending_notices[0].0;
    assert!(text.starts_with("error: bad "));
    assert!(text.len() <= MAX_NOTICE_BYTES);
    crate::runtime::validate_notice(text).unwrap();
    ui.flush_notices(&tx);
    assert!(ui.notice_busy && ui.pending_notices.is_empty());
    let work = rx.try_recv().unwrap();
    assert!(matches!(work.op, Op::Notice));
    assert!(matches!(
        work.command,
        Command::RecordNotice {
            level: NoticeLevel::Error,
            ..
        }
    ));
    ui.reset_completion();
    ui.notify(error.clone(), true);
    ui.flush_notices(&tx);
    assert!(ui.pending_notices.is_empty() && rx.try_recv().is_err());
    assert!(!complete(&mut ui, work.op, Ok(Reply::Ack)));
    assert!(!ui.notice_busy && ui.outstanding == 0);
    assert!(ui.last_poll.elapsed() >= Duration::from_millis(300));
    ui.notify(error.clone(), true);
    assert!(ui.pending_notices.is_empty());
    ui.reset_completion();
    ui.notify(error, true);
    assert_eq!(ui.pending_notices.len(), 1);
    assert!(ui.inputs.is_empty());
}

#[test]
fn notice_dispatch_removes_only_accepted_work_and_never_recursively_notifies() {
    let mut ui = Ui::new(&Config::default(), &fixture_snapshot());
    let (tx, mut rx) = mpsc::channel(1);
    assert!(ui.enqueue(&tx, Op::Inputs, Command::Inputs { limit: 1 }));
    assert!(ui.record_action("session paused", NoticeLevel::Status, "queue"));
    ui.flush_notices(&tx);
    assert_eq!(ui.pending_notices.len(), 1);
    assert!(!ui.notice_busy && ui.notice.is_none());
    complete(
        &mut ui,
        rx.try_recv().unwrap().op,
        Ok(Reply::Inputs(vec![])),
    );
    ui.outstanding = OUTSTANDING;
    ui.flush_notices(&tx);
    assert_eq!(ui.pending_notices.len(), 1);
    assert!(rx.try_recv().is_err());
    ui.outstanding = 0;
    ui.flush_notices(&tx);
    assert!(ui.notice_busy);
    ui.record_notice("another outcome", NoticeLevel::Info);
    ui.flush_notices(&tx);
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(work.command, Command::RecordAction { text, level: NoticeLevel::Status, category } if text == "session paused" && category == "queue")
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(ui.pending_notices.len(), 1);
    drop(rx);
    complete(&mut ui, work.op, Ok(Reply::Ack));
    ui.flush_notices(&tx);
    assert_eq!(ui.pending_notices.len(), 1);
    assert!(!ui.notice_busy && ui.notice.is_none());
}

#[test]
fn notice_write_errors_are_visible_without_logging_the_logging_failure() {
    for result in [
        Err(anyhow::anyhow!("disk\nfull")),
        Ok(Reply::Inputs(vec![])),
    ] {
        let mut ui = Ui::new(&Config::default(), &fixture_snapshot());
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        ui.record_notice("paused", NoticeLevel::Info);
        ui.flush_notices(&tx);
        let work = rx.try_recv().unwrap();
        assert!(!complete(&mut ui, work.op, result));
        assert!(!ui.notice_busy && ui.outstanding == 0);
        assert!(ui.pending_notices.is_empty());
        let (text, _, error) = ui.notice.as_ref().unwrap();
        assert!(*error && text.starts_with("history not saved: ") && !text.contains('\n'));
        ui.flush_notices(&tx);
        assert!(rx.try_recv().is_err());
        ui.draft.set_text("still usable");
        ui.submit(&fixture_snapshot(), &tx).unwrap();
        assert!(matches!(
            rx.try_recv().unwrap().command,
            Command::SubmitInputAccumulated { .. }
        ));
    }
}

#[test]
fn settings_outcomes_are_concise_and_logging_never_submits_model_input() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    for (args, outcome) in [
        ("status-color true", "status-color enabled"),
        ("status-color false", "status-color disabled"),
        ("status braille", "status set to braille"),
        ("status-line false", "status-line disabled"),
        ("prompt-history true", "prompt-history enabled"),
    ] {
        ui.draft.set_text(&format!("/settings {args}"));
        ui.submit(&snapshot, &tx).unwrap();
        assert_eq!(
            ui.pending_notices.back().unwrap(),
            &(outcome.into(), NoticeLevel::Info)
        );
        assert_eq!(ui.inputs.last().unwrap(), &format!("/settings {args}"));
        assert!(ui.draft.text().is_empty());
        ui.flush_notices(&tx);
        let work = rx.try_recv().unwrap();
        assert!(
            matches!(work.command, Command::RecordAction { text, level: NoticeLevel::Info, category } if text == outcome && category == "settings")
        );
        let inputs = ui.inputs.clone();
        complete(&mut ui, work.op, Ok(Reply::Ack));
        assert_eq!(ui.inputs, inputs);
        assert!(rx.try_recv().is_err());
    }
    ui.draft.set_text("/settings status-color invalid");
    let error = ui.submit(&snapshot, &tx).unwrap_err();
    ui.notify(format!("{error:#}"), true);
    assert_eq!(ui.pending_notices.len(), 1);
    assert_eq!(ui.pending_notices[0].1, NoticeLevel::Error);
    assert!(!ui.interface.status_color);
    assert_eq!(ui.draft.text(), "/settings status-color invalid");
}

#[test]
fn control_outcomes_follow_actual_commands_and_only_acknowledged_results() {
    for (action, paused, outcome) in [
        (Action::Pause, false, "paused"),
        (Action::Resume, true, "resumed"),
        (Action::TogglePause, true, "resumed"),
        (Action::TogglePause, false, "paused"),
        (Action::Stop, false, "stopped"),
        (Action::Interrupt, false, "interrupted"),
    ] {
        for fail in [false, true] {
            let mut snapshot = fixture_snapshot();
            snapshot.paused = paused;
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "active".into(),
                text: "partial".into(),
                truncated: false,
                message_id: String::new(),
                durable_bytes: 0,
            });
            let mut ui = Ui::new(&Config::default(), &snapshot);
            let (tx, mut rx) = mpsc::channel(OUTSTANDING);
            ui.draft.set_text("retain until ACK");
            ui.execute(action, "", true, &snapshot, &tx).unwrap();
            let work = rx.try_recv().unwrap();
            if action == Action::Stop {
                assert!(matches!(work.op, Op::Stop { clear_draft: true }));
                assert!(matches!(work.command, Command::StopPending {
                    expected_attempt_id: Some(ref id),
                    expected_paused: false,
                } if id == "active"));
            } else {
                assert!(
                    matches!(work.op, Op::Control { outcome: captured, .. } if captured == outcome)
                );
            }
            assert!(ui.pending_notices.is_empty());
            complete(
                &mut ui,
                work.op,
                if fail {
                    Err(anyhow::anyhow!("not acknowledged"))
                } else if action == Action::Stop {
                    Ok(Reply::Stopped {
                        paused: true,
                        cancelled: true,
                    })
                } else {
                    Ok(Reply::Ack)
                },
            );
            assert!(!ui.pause_pending && !ui.stop_pending && !ui.draft_busy);
            assert_eq!(ui.pending_notices.len(), 1);
            if fail {
                assert_eq!(ui.pending_notices[0].1, NoticeLevel::Error);
                assert_eq!(ui.draft.text(), "retain until ACK");
            } else {
                assert_eq!(ui.pending_notices[0], (outcome.into(), NoticeLevel::Info));
                assert!(ui.draft.text().is_empty());
            }
        }
    }
}

#[test]
fn editing_acknowledgements_log_outcomes_without_changing_lease_or_send_semantics() {
    let mut ui = Ui::new(&Config::default(), &fixture_snapshot());
    let item = QueueItem {
        id: "leased".into(),
        revision: 3,
        text: "queued text".into(),
        state: QueueState::Editing,
    };
    complete(&mut ui, Op::BeginEdit, Ok(Reply::Editing(item)));
    assert_eq!(ui.edit, Some(("leased".into(), 3)));
    assert_eq!(ui.draft.text(), "queued text");
    assert_eq!(
        ui.pending_notices.back().unwrap().0,
        "editing queued message"
    );
    complete(
        &mut ui,
        Op::FinishEdit,
        Err(anyhow::anyhow!("stale revision")),
    );
    assert_eq!(ui.edit, Some(("leased".into(), 3)));
    assert_eq!(ui.draft.text(), "queued text");
    complete(&mut ui, Op::FinishEdit, Ok(Reply::Ack));
    assert!(ui.edit.is_none() && ui.draft.text().is_empty());
    assert_eq!(
        ui.pending_notices.back().unwrap().0,
        "queued message updated"
    );
    ui.edit = Some(("leased".into(), 4));
    complete(&mut ui, Op::CancelEdit, Ok(Reply::Ack));
    assert!(ui.edit.is_none());
    assert_eq!(ui.pending_notices.back().unwrap().0, "edit cancelled");
    complete(
        &mut ui,
        Op::Cancel {
            id: "leased".into(),
            clear_draft: false,
        },
        Ok(Reply::Ack),
    );
    assert_eq!(ui.pending_notices.back().unwrap().0, "message cancelled");
    let notices = ui.pending_notices.clone();
    ui.draft.set_text("ordinary prompt");
    complete(
        &mut ui,
        Op::Dispatch {
            clear: true,
            text: "ordinary prompt".into(),
            action: false,
            composer_revision: Some(0),
            draft_id: None,
        },
        Ok(Reply::Ack),
    );
    assert_eq!(ui.inputs, ["queued text", "ordinary prompt"]);
    assert_eq!(ui.pending_notices, notices);
}

#[test]
fn a_full_notice_queue_blocks_mutations_but_not_typing_or_navigation() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    for index in 0..100 {
        ui.notify(format!("failure {index}"), true);
    }
    assert_eq!(ui.pending_notices.len(), 16);
    ui.draft.set_text("/settings status-color true");
    assert!(
        ui.submit(&snapshot, &tx)
            .unwrap_err()
            .to_string()
            .contains("wait for history")
    );
    assert!(!ui.interface.status_color);
    assert!(ui.execute(Action::Stop, "", false, &snapshot, &tx).is_err());
    ui.draft.set_text("still typing");
    press(&mut ui, KeyCode::Char('!'), &snapshot, &tx);
    assert_eq!(ui.draft.text(), "still typing!");
    assert!(ui.submit(&snapshot, &tx).is_err());
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    assert_eq!(ui.view, View::Help);
    assert_eq!(ui.draft.text(), "still typing!");
    assert!(rx.try_recv().is_err());
    ui.flush_notices(&tx);
    complete(&mut ui, rx.try_recv().unwrap().op, Ok(Reply::Ack));
    let saved = ui.live_notices.front().unwrap().clone();
    ui.publish_with(&snapshot, 80, |_| Ok(())).unwrap();
    ui.pending.insert(saved.seq, saved);
    ui.publish_with(&snapshot, 80, |_| panic!("notice replayed"))
        .unwrap();
    ui.execute(Action::Settings, "status-color true", false, &snapshot, &tx)
        .unwrap();
    assert!(ui.interface.status_color);
    assert_eq!(ui.pending_notices.len(), 16);
    assert_eq!(ui.pending_notices.back().unwrap().0, "status-color enabled");
}

#[test]
fn error_storm_reserves_space_for_every_in_flight_confirmed_outcome() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    for _ in 0..OUTSTANDING {
        assert!(ui.enqueue(
            &tx,
            Op::Control {
                clear_draft: false,
                pause: false,
                outcome: "stopped"
            },
            Command::Stop
        ));
    }
    for index in 0..100 {
        ui.notify(format!("poll failure {index}"), true);
    }
    assert_eq!(ui.pending_notices.len(), 16 - OUTSTANDING);
    for _ in 0..OUTSTANDING {
        complete(&mut ui, rx.try_recv().unwrap().op, Ok(Reply::Ack));
    }
    assert_eq!(ui.pending_notices.len(), 16);
    assert_eq!(
        ui.pending_notices
            .iter()
            .filter(|(text, level)| text == "stopped" && *level == NoticeLevel::Info)
            .count(),
        OUTSTANDING
    );
}

#[tokio::test]
async fn deferred_quit_flushes_default_results_before_shutdown_and_retains_exit_failure() {
    for fail in [false, true] {
        let snapshot = fixture_snapshot();
        let mut ui = Ui::new(&uncolored_config(), &snapshot);
        let (tx, mut rx) = mpsc::channel(OUTSTANDING);
        ui.record_notice("earlier outcome", NoticeLevel::Info);
        ui.saving_defaults = true;
        ui.draft_busy = true;
        let (release, wait) = std::sync::mpsc::channel();
        ui.defaults_task = Some(tokio::task::spawn_blocking(move || {
            wait.recv().unwrap();
            if fail {
                anyhow::bail!("preference write failed");
            }
            Ok(Change::StatusColor(true))
        }));
        ui.execute(Action::Quit, "", false, &snapshot, &tx).unwrap();
        assert!(ui.exiting && ui.quit_pending);
        assert!(rx.try_recv().is_err());
        ui.flush_notices(&tx);
        let earlier = rx.try_recv().unwrap();
        assert!(matches!(earlier.op, Op::Notice));
        complete(&mut ui, earlier.op, Ok(Reply::Ack));
        assert!(!ui.try_shutdown(&tx));
        assert!(rx.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(ui.finish_pending_defaults().await.is_err(), fail);
        assert_eq!(ui.interface.status_color, !fail);
        assert_eq!(ui.exit_error.is_some(), fail);
        assert_eq!(ui.pending_notices.len(), 1);
        assert!(!ui.try_shutdown(&tx));
        let notice = rx.try_recv().unwrap();
        match notice.command {
            Command::RecordAction {
                text,
                level,
                category,
            } if fail => {
                assert_eq!(category, "settings");
                if fail {
                    assert_eq!(level, NoticeLevel::Error);
                    assert!(
                        text.starts_with("error: ") && text.contains("preference write failed")
                    );
                } else {
                    assert_eq!(level, NoticeLevel::Info);
                    assert_eq!(text, "status-color enabled (default)");
                }
            }
            Command::RecordAction {
                text,
                level,
                category,
            } if !fail => {
                assert_eq!(level, NoticeLevel::Info);
                assert_eq!(category, "settings");
                assert_eq!(text, "status-color enabled (default)");
            }
            _ => panic!("default outcome must be recorded before Shutdown"),
        }
        assert!(!ui.try_shutdown(&tx));
        assert!(rx.try_recv().is_err());
        complete(&mut ui, notice.op, Ok(Reply::Ack));
        assert!(ui.try_shutdown(&tx));
        assert!(!ui.quit_pending);
        assert!(!ui.try_shutdown(&tx));
        let shutdown = rx.try_recv().unwrap();
        assert!(matches!(shutdown.command, Command::Shutdown));
        assert!(complete(&mut ui, shutdown.op, Ok(Reply::Ack)));
        assert_eq!(ui.exit_error.take().is_some(), fail);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn default_failure_before_quit_is_logged_without_becoming_an_exit_failure() {
    let mut ui = Ui::new(&Config::default(), &fixture_snapshot());
    ui.saving_defaults = true;
    assert!(
        ui.complete_defaults(Err(anyhow::anyhow!("save failed")))
            .is_err()
    );
    assert!(ui.exit_error.is_none());
    assert_eq!(ui.pending_notices[0].1, NoticeLevel::Error);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.execute(Action::Quit, "", false, &fixture_snapshot(), &tx)
        .unwrap();
    assert!(!ui.try_shutdown(&tx));
    let notice = rx.try_recv().unwrap();
    assert!(matches!(notice.op, Op::Notice));
    complete(&mut ui, notice.op, Ok(Reply::Ack));
    assert!(ui.try_shutdown(&tx));
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
    assert!(ui.exit_error.is_none());
}

#[test]
fn read_replies_do_not_log_notices_and_shutdown_waits_for_outstanding_reads() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    for (op, reply) in [
        (Op::Tail, Reply::History(vec![])),
        (
            Op::History(history::Load::Latest, false),
            Reply::History(vec![]),
        ),
        (Op::HistoryRefresh, Reply::History(vec![])),
        (Op::Inputs, Reply::Inputs(vec!["previous prompt".into()])),
        (Op::Events, Reply::Events(vec![])),
    ] {
        assert!(!complete(&mut ui, op, Ok(reply)));
        assert!(ui.pending_notices.is_empty() && ui.notice.is_none());
    }
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.enqueue(&tx, Op::Inputs, Command::Inputs { limit: 100 });
    let read = rx.try_recv().unwrap();
    ui.execute(Action::Quit, "", false, &snapshot, &tx).unwrap();
    assert!(ui.exiting && ui.quit_pending);
    assert!(!ui.try_shutdown(&tx));
    ui.poll(&tx);
    assert!(rx.try_recv().is_err());
    complete(&mut ui, read.op, Ok(Reply::Inputs(vec![])));
    assert!(ui.try_shutdown(&tx));
    assert!(!ui.try_shutdown(&tx));
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));

    let (closed, receiver) = mpsc::channel(OUTSTANDING);
    drop(receiver);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("/quit");
    ui.submit(&snapshot, &closed).unwrap();
    assert!(!ui.exiting && !ui.quit_pending);
    assert_eq!(ui.draft.text(), "/quit");
}

#[tokio::test]
async fn error_storm_leaves_room_for_the_pending_default_acknowledgement() {
    for fail in [false, true] {
        let mut ui = Ui::new(&uncolored_config(), &fixture_snapshot());
        ui.saving_defaults = true;
        ui.draft_busy = true;
        ui.defaults_task = Some(tokio::task::spawn_blocking(move || {
            if fail {
                anyhow::bail!("default save failed");
            }
            Ok(Change::StatusColor(true))
        }));
        for index in 0..100 {
            ui.notify(format!("unrelated failure {index}"), true);
        }
        assert_eq!(ui.pending_notices.len(), 15);
        assert_eq!(ui.finish_pending_defaults().await.is_err(), fail);
        assert_eq!(ui.interface.status_color, !fail);
        assert_eq!(ui.pending_notices.len(), 16);
        let (text, level) = ui.pending_notices.back().unwrap();
        if fail {
            assert_eq!(*level, NoticeLevel::Error);
            assert!(text.contains("default save failed"));
        } else {
            assert_eq!(*level, NoticeLevel::Info);
            assert_eq!(text, "status-color enabled (default)");
        }
    }
}

fn rendered(ui: &Ui, snapshot: &Snapshot, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, ui, snapshot)).unwrap();
    terminal.backend().buffer().clone()
}

fn plain(buffer: &ratatui::buffer::Buffer) -> String {
    buffer.content.iter().map(|cell| cell.symbol()).collect()
}

fn press(ui: &mut Ui, code: KeyCode, snapshot: &Snapshot, tx: &mpsc::Sender<Work>) {
    ui.key(
        KeyEvent::new(code, KeyModifiers::NONE),
        snapshot,
        tx,
        110,
        20,
    )
    .unwrap();
}

fn type_text(ui: &mut Ui, text: &str, snapshot: &Snapshot, tx: &mpsc::Sender<Work>) {
    for ch in text.chars() {
        press(ui, KeyCode::Char(ch), snapshot, tx);
    }
}

fn select(ui: &mut Ui, label: &str, snapshot: &Snapshot, tx: &mpsc::Sender<Work>) {
    let index = ui
        .completion(snapshot)
        .unwrap()
        .items
        .iter()
        .position(|item| item.label == label)
        .unwrap();
    if !ui.completion_active {
        press(ui, KeyCode::Tab, snapshot, tx);
    }
    if !ui.completion_active {
        return;
    }
    if !ui.pager_selected {
        press(ui, KeyCode::Tab, snapshot, tx);
    }
    while ui.completion_selected != index {
        press(ui, KeyCode::Down, snapshot, tx);
    }
}

#[test]
fn sanitizes_all_terminal_controls_and_normalizes_cr() {
    let value = clean("a\x1b]52;c;secret\x07\r\nb\r\tc\u{009b}31m\0", 1024);
    assert_eq!(value, "a]52;c;secret\nb\n\tc31m");
    assert!(
        value
            .chars()
            .all(|c| !c.is_control() || c == '\n' || c == '\t')
    );
}

#[test]
fn byte_limits_respect_utf8() {
    assert_eq!(tail("a\u{1f600}b", 3), "b");
    assert_eq!(clean("a\u{1f600}b", 4), "a\n[display truncated]");
    assert_eq!(clean("a\u{1f600}b", 5), "a\u{1f600}\n[display truncated]");
    assert_eq!(sanitized("a\u{1f600}b", 4), ("a".into(), true));
    assert_eq!(
        sanitized("\n[display truncated]", 100),
        ("\n[display truncated]".into(), false)
    );
}

#[test]
fn wraps_graphemes_and_expands_tabs() {
    let rows = wrapped("a\u{0301}b\tZ", 2);
    let rows = rows.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(rows, ["a\u{0301}b", "  ", "  ", "Z"]);
}

#[test]
fn root_is_unboxed_compact_and_hides_technical_metadata() {
    let snapshot = fixture_snapshot();
    let ui = Ui::new(&Config::default(), &snapshot);
    let buffer = rendered(&ui, &snapshot, 100, INLINE_HEIGHT);
    let text = plain(&buffer);
    assert!(text.contains("\u{2503} "));
    assert!(text.contains("ready") && text.contains("local test fixture"));
    assert!(text.contains("/help"));
    assert!(!text.contains("hidden-in-root"));
    assert!(!text.contains("No active response"));
    assert!(!text.contains("acknowledged"));
    assert!(!text.contains('\u{250c}'));
    assert!(buffer.content.iter().all(|cell| matches!(
        cell.fg,
        Color::Reset | Color::Indexed(240 | 245 | 255)
    ) && cell.bg == Color::Reset
        && !cell.modifier.contains(Modifier::DIM)));
}

#[test]
fn details_keep_the_runtime_ids_and_checkpoint_information() {
    let mut snapshot = fixture_snapshot();
    snapshot.active = Some(crate::runtime::ActiveView {
        attempt_id: "attempt-visible-in-details".into(),
        text: "live tail".into(),
        truncated: false,
        message_id: String::new(),
        durable_bytes: 4,
    });
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Details;
    let text = plain(&rendered(&ui, &snapshot, 100, 24));
    for id in [
        &snapshot.session_id,
        &snapshot.agent_id,
        &snapshot.conversation_id,
    ] {
        assert!(text.contains(id));
    }
    assert!(text.contains("attempt-visible-in-details"));
    assert!(text.contains("9 bytes / 4 committed"));
    assert!(text.contains("Esc back"));
}

#[test]
fn transcript_uses_bold_user_rail_and_preserves_markdown_styles_when_wrapped() {
    let entry = Saved {
        seq: 1,
        attempt_id: "attempt".into(),
        kind: "user".into(),
        text: "hello world".into(),
        status: "accepted".into(),
    };
    let rows = message_rows(&entry, 8, 100);
    assert_eq!(rows[0].to_string(), "\u{2503} hello ");
    assert_eq!(rows[1].to_string(), "\u{2503} world");
    assert!(rows[0].style.add_modifier.contains(Modifier::BOLD));
    let rows = markdown_rows("**hello world** and `code`\n```rust\nlet x = 1;\n```", 8);
    assert!(
        rows.iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains("hello")
                && span.style.add_modifier.contains(Modifier::BOLD))
    );
    assert!(rows.iter().flat_map(|line| &line.spans).any(
        |span| span.content.contains("code") && span.style.fg == Some(Color::Indexed(245))
    ));
    assert!(
        !rows
            .iter()
            .any(|line| line.to_string().contains("**") || line.to_string().contains("```"))
    );
    assert!(rows.iter().all(|line| line.width() <= 10));
}

#[test]
fn errors_and_exit_confirmation_remain_visible_without_debug_panels() {
    let mut snapshot = fixture_snapshot();
    snapshot.phase = Phase::StorageError;
    snapshot.error = Some("writes paused".into());
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    let buffer = rendered(&ui, &snapshot, 80, INLINE_HEIGHT);
    assert!(plain(&buffer).contains("storage error: writes paused"));
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.fg == Color::Indexed(252))
    );
    ui.exit_armed = Some(Instant::now());
    assert!(
        plain(&rendered(&ui, &snapshot, 80, INLINE_HEIGHT)).contains("press ctrl+c again to exit")
    );
}

#[test]
fn narrow_and_short_views_do_not_panic() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("\u{1f600}\nhello\nmore input");
    for view in [
        View::Root,
        View::History,
        View::Queue,
        View::Help,
        View::Details,
    ] {
        ui.view = view;
        for (width, height) in [(1, 1), (2, 2), (20, 4)] {
            rendered(&ui, &snapshot, width, height);
        }
    }
}

#[test]
fn tab_extends_prefix_then_selects_and_previews_before_enter_commits() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.drafts = ["aaaa-one", "aaaa-two"]
        .into_iter()
        .enumerate()
        .map(|(index, id)| crate::runtime::DraftRecord {
            id: id.into(),
            text: format!("saved draft {index}"),
            revision: index as i64 + 1,
        })
        .collect();
    let (tx, mut rx) = mpsc::channel(8);
    type_text(&mut ui, "/send a", &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/send aaaa-");
    assert!(ui.completion_active);
    assert!(!ui.pager_selected);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 0);
    assert_eq!(ui.draft.text(), "/send aaaa-one");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 1);
    assert_eq!(ui.draft.text(), "/send aaaa-two");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/send aaaa-two");
    assert!(!ui.completion_active && !ui.exiting);
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(!ui.exiting && ui.draft_busy);
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::DispatchDraftAccumulated { id, revision: 2, expected_paused: false } if id == "aaaa-two")
    );
}

#[test]
fn exact_command_wins_and_dismissal_never_turns_a_slash_command_into_chat() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.append("/drafts");
    ui.completion_selected = 999;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Drafts);
    assert!(!ui.exiting);
    let read = rx.try_recv().unwrap();
    assert!(matches!(read.command, Command::Drafts));
    complete(&mut ui, read.op, Ok(Reply::Drafts(vec![])));
    ui.view = View::Root;
    ui.append("/he");
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(!ui.completion_visible());
    assert!(ui.submit(&snapshot, &tx).is_err());
    assert!(rx.try_recv().is_err());
    assert_eq!(ui.draft.text(), "/he");
    ui.execute(Action::Pause, "", false, &snapshot, &tx)
        .unwrap();
    assert!(!ui.completion_visible());
    assert!(matches!(rx.try_recv().unwrap().command, Command::Pause));
}

#[test]
fn completion_preserves_arguments_and_palette_preserves_the_original_draft() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("/s  abcdef12");
    ui.draft.set_cursor(2);
    select(&mut ui, "/send", &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/send  abcdef12");
    assert!(rx.try_recv().is_err());
    ui.draft.set_text("ordinary work in progress");
    ui.reset_completion();
    ui.execute(Action::Commands, "", false, &snapshot, &tx)
        .unwrap();
    type_text(&mut ui, "history", &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(ui.palette);
    assert_eq!(ui.palette_query.text(), "/history ");
    assert_eq!(ui.draft.text(), "ordinary work in progress");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::History);
    assert!(ui.full_history);
    assert_eq!(ui.draft.text(), "ordinary work in progress");
}

#[test]
fn stop_retains_draft_until_ack_and_cannot_be_submitted_twice() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.append("/stop");
    ui.submit(&snapshot, &tx).unwrap();
    assert_eq!(ui.draft.text(), "/stop");
    assert!(ui.draft_busy && ui.stop_pending);
    let work = rx.try_recv().unwrap();
    assert!(matches!(
        work.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(matches!(work.op, Op::Stop { clear_draft: true }));
    assert!(ui.execute(Action::Stop, "", false, &snapshot, &tx).is_err());
    assert!(rx.try_recv().is_err());
    complete(
        &mut ui,
        work.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    assert!(ui.draft.text().is_empty());
    assert!(ui.stopped && !ui.stop_pending && !ui.draft_busy);
}

#[test]
fn literal_slashes_multiline_pastes_and_leased_edits_are_not_commands() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for (draft, expected) in [
        ("//quit", "/quit"),
        ("/stop\nexplain this path", "/stop\nexplain this path"),
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.append(draft);
        assert!(!ui.completion_visible());
        ui.submit(&snapshot, &tx).unwrap();
        assert!(
            matches!(rx.try_recv().unwrap().command, Command::SubmitInputAccumulated { text, .. } if text == expected)
        );
    }
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.edit = Some(("lease-id".into(), 3));
    ui.append("/quit");
    ui.submit(&snapshot, &tx).unwrap();
    assert!(!ui.exiting);
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::FinishPendingEdit { id, revision: 3, text, expected_paused: false }
        if text == "/quit" && id == "lease-id")
    );
}

#[test]
fn message_target_prefixes_must_be_unique_and_bind_to_stable_id_and_revision() {
    let mut snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    snapshot.queue = vec![
        QueueItem {
            id: "abcdef12-1234-1111-1111-111111111111".into(),
            text: "one".into(),
            revision: 4,
            state: QueueState::Queued,
        },
        QueueItem {
            id: "abcdef12-5678-2222-2222-222222222222".into(),
            text: "two".into(),
            revision: 7,
            state: QueueState::Queued,
        },
    ];
    let mut ui = Ui::new(&Config::default(), &snapshot);
    assert!(ui.target("abcdef12", &snapshot).is_err());
    assert_eq!(
        queue_id(&snapshot.queue[0], &snapshot.queue),
        "abcdef12-123"
    );
    ui.view = View::Queue;
    let buffer = rendered(&ui, &snapshot, 100, 14);
    assert!(!plain(&buffer).contains('>'));
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.fg == Color::Indexed(255) && cell.modifier.contains(Modifier::BOLD))
    );
    ui.view = View::Root;
    ui.execute(Action::Cancel, "abcdef12-567", false, &snapshot, &tx)
        .unwrap();
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::Cancel { id, revision: 7 } if id == snapshot.queue[1].id)
    );
    assert!(!ui.draft_busy);
}

#[test]
fn completion_and_help_render_descriptions_and_effective_bindings() {
    let snapshot = fixture_snapshot();
    let mut config = Config::default();
    config.keys.history = vec!["alt+h".into(), "f2".into()];
    let mut ui = Ui::new(&config, &snapshot);
    ui.append("/s");
    let (tx, mut rx) = mpsc::channel(8);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    {
        let buffer = rendered(&ui, &snapshot, 110, 14);
        let text = plain(&buffer);
        assert!(text.contains("/send") && text.contains("/stop"));
        assert!(!text.contains("Tab activate"));
        assert!(text.contains("Enter accepts only") && text.contains("Esc restores"));
        let stop = crate::commands::COMMANDS
            .iter()
            .find(|spec| spec.action == Action::Stop)
            .unwrap();
        assert!(text.contains(stop.description));
        let input_y = buffer
            .content
            .chunks(110)
            .position(|row| row.iter().any(|cell| cell.symbol() == "\u{2503}"))
            .unwrap();
        let menu_y = buffer
            .content
            .chunks(110)
            .position(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("/send")
            })
            .unwrap();
        assert!(menu_y > input_y, "completion must remain below input");
    }
    let preview = (ui.draft.text().to_owned(), ui.draft.cursor());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(!ui.completion_active && ui.view == View::Root);
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        (preview.0.as_str(), preview.1)
    );
    assert!(rx.try_recv().is_err());
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    type_text(&mut ui, "history", &snapshot, &tx);
    let text = plain(&rendered(&ui, &snapshot, 110, 50));
    assert!(text.contains("Commands 1") && text.contains("/history"));
    assert!(text.contains("alt+h / f2"));
    ui.help_query.clear();
    type_text(&mut ui, "send", &snapshot, &tx);
    let selected = ui
        .help_commands()
        .iter()
        .position(|spec| spec.action == Action::Send)
        .unwrap();
    for _ in 0..selected {
        press(&mut ui, KeyCode::Down, &snapshot, &tx);
    }
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.help_detail.unwrap().action, Action::Send);
    let text = plain(&rendered(&ui, &snapshot, 110, 50));
    assert!(text.contains("/send <draft-id>"));
    assert!(text.contains(ui.help_detail.unwrap().description));
    assert!(text.contains("Run unavailable") && text.contains("Insert into command field"));
    assert!(rx.try_recv().is_err());
}

#[test]
fn palette_tab_cannot_replace_even_an_empty_leased_edit() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.edit = Some(("lease".into(), 1));
    ui.execute(Action::Commands, "", false, &snapshot, &tx)
        .unwrap();
    type_text(&mut ui, "settings status-color ", &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(ui.palette);
    assert!(ui.draft.text().is_empty());
    assert_eq!(ui.edit, Some(("lease".into(), 1)));
    assert!(rx.try_recv().is_err());
    ui.palette_query
        .set_text("send abcdef12-1234-1111-1111-111111111111");
    // The narrow composer follows the caret, so inspect the command at Home.
    press(&mut ui, KeyCode::Home, &snapshot, &tx);
    assert!(plain(&rendered(&ui, &snapshot, 24, 4)).contains("send"));
}

#[test]
fn ctrl_c_twice_exits_and_timeout_rearms_without_yes_no() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("keep this draft");
    let now = Instant::now();
    ui.ctrl_c(now, &snapshot, &tx);
    assert!(!ui.exiting);
    assert_eq!(ui.draft.text(), "keep this draft");
    assert!(rx.try_recv().is_err());
    ui.ctrl_c(now + EXIT_WINDOW, &snapshot, &tx);
    assert!(!ui.exiting);
    ui.ctrl_c(now + EXIT_WINDOW + Duration::from_millis(1), &snapshot, &tx);
    assert!(ui.exiting);
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
}

#[test]
fn first_ctrl_c_interrupts_the_selected_active_response_then_second_defers_shutdown() {
    let mut snapshot = fixture_snapshot();
    snapshot.active = Some(crate::runtime::ActiveView {
        attempt_id: "active".into(),
        text: "partial".into(),
        truncated: false,
        message_id: String::new(),
        durable_bytes: 0,
    });
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let now = Instant::now();
    ui.ctrl_c(now, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    assert!(matches!(work.command, Command::Interrupt { attempt_id } if attempt_id == "active"));
    ui.ctrl_c(now + Duration::from_millis(10), &snapshot, &tx);
    assert!(ui.exiting && ui.quit_pending);
    assert!(rx.try_recv().is_err());
    complete(&mut ui, work.op, Ok(Reply::Ack));
    assert!(!ui.try_shutdown(&tx));
    let notice = rx.try_recv().unwrap();
    assert!(
        matches!(notice.command, Command::RecordAction { text, level: NoticeLevel::Info, category } if text == "interrupted" && category == "queue")
    );
    complete(&mut ui, notice.op, Ok(Reply::Ack));
    assert!(ui.try_shutdown(&tx));
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
}

#[test]
fn ctrl_c_closes_the_history_view_without_interrupting_work_and_second_exits() {
    let mut snapshot = fixture_snapshot();
    snapshot.active = Some(crate::runtime::ActiveView {
        attempt_id: "active".into(),
        text: "partial".into(),
        truncated: false,
        message_id: String::new(),
        durable_bytes: 0,
    });
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::History;
    let now = Instant::now();
    ui.ctrl_c(now, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert!(rx.try_recv().is_err());
    ui.ctrl_c(now + Duration::from_millis(1), &snapshot, &tx);
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
}

#[test]
fn input_recall_restores_the_current_draft_and_does_not_show_completion_on_recall() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.remember("first prompt".into());
    ui.remember("/settings".into());
    ui.draft.set_text("s");
    assert!(ui.draft.set_cursor(0));
    ui.recall(true);
    assert_eq!(ui.draft.text(), "/settings");
    assert!(!ui.completion_visible());
    ui.recall(true);
    assert_eq!(ui.draft.text(), "first prompt");
    assert!(!ui.completion_visible());
    ui.recall(false);
    assert_eq!(ui.draft.text(), "/settings");
    ui.recall(false);
    assert_eq!(ui.draft.text(), "s");
    assert_eq!(ui.draft.cursor(), 0);
    assert!(!ui.completion_visible());
    ui.interface.prompt_history = false;
    ui.recall(true);
    assert_eq!(ui.draft.text(), "s");
    assert_eq!(ui.draft.cursor(), 0);
}

#[test]
fn native_stream_segments_preserve_all_utf8_bytes_without_duplicate_completion_text() {
    let text = format!("first line\n{}", "ab\u{1f600}".repeat(5000));
    let mut offset = 0;
    let mut printed = String::new();
    while let Some((end, last)) = native_end(&text, offset, true) {
        assert!(!last);
        printed.push_str(&text[offset..end]);
        offset = end;
    }
    let (end, last) = native_end(&text, offset, false).unwrap();
    assert!(last);
    printed.push_str(&text[offset..end]);
    assert_eq!(printed, text);
}

#[test]
fn settings_pager_has_only_values_and_short_descriptions() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.preferences_path = "/preferences.toml".into();
    ui.draft.set_text("/settings ");
    let (tx, mut rx) = mpsc::channel(8);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let text = plain(&rendered(&ui, &snapshot, 110, 28));
    assert!(
        !text.contains("Source:") && !text.contains("Examples:") && !text.contains("Tab activate")
    );
    assert!(text.contains("status-line") && text.contains("prompt-history"));
    assert!(text.contains("Current: true"));
    ui.reset_completion();
    ui.draft.set_text("/settings status braille ");
    press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
    let text = plain(&rendered(&ui, &snapshot, 110, 28));
    assert!(text.contains("--default"));
    assert!(text.contains("Make default"));
    assert!(!text.contains("Apply") && !text.contains("Provider/keys"));
    ui.execute(Action::Details, "", false, &snapshot, &tx)
        .unwrap();
    let text = plain(&rendered(&ui, &snapshot, 110, 40));
    assert!(text.contains("Built-in defaults"));
    press(&mut ui, KeyCode::PageDown, &snapshot, &tx);
    let text = plain(&rendered(&ui, &snapshot, 110, 40));
    assert!(text.contains("/preferences.toml"));
    assert_eq!(ui.interface, Config::default().interface);
    assert!(!ui.draft_busy && !ui.saving_defaults && ui.defaults_task.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_key_sequence_never_applies_before_final_enter() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let before = ui.interface.clone();
    let (tx, mut rx) = mpsc::channel(8);
    press(&mut ui, KeyCode::Char('/'), &snapshot, &tx);
    select(&mut ui, "/settings", &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings ");
    assert_eq!(ui.interface, before);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings ");
    assert_eq!(ui.completion(&snapshot).unwrap().stage, Stage::Setting);
    select(&mut ui, "status-color", &snapshot, &tx);
    assert_eq!(ui.interface, before);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings status-color ");
    assert_eq!(ui.completion(&snapshot).unwrap().stage, Stage::Value);
    select(&mut ui, "true", &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings status-color true ");
    assert_eq!(ui.completion(&snapshot).unwrap().stage, Stage::Option);
    assert_eq!(ui.interface, before);
    assert!(ui.inputs.is_empty());
    assert_eq!(ui.interface, before);
    assert!(ui.defaults_task.is_none() && !ui.saving_defaults && !ui.draft_busy);
    assert_eq!(ui.draft.text(), "/settings status-color true ");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.interface.status_color);
    assert!(ui.draft.text().is_empty());
    assert_eq!(ui.inputs, ["/settings status-color true"]);
    assert_eq!(ui.notice.as_ref().unwrap().0, "status-color enabled");
    assert!(rx.try_recv().is_err());
}

#[test]
fn tab_and_backtab_on_optional_flag_have_no_side_effects() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let before = ui.interface.clone();
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("/settings status-color true ");
    for code in [
        KeyCode::BackTab,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Down,
        KeyCode::Up,
    ] {
        press(&mut ui, code, &snapshot, &tx);
        assert_eq!(ui.interface, before);
        assert!(ui.draft.text().starts_with("/settings status-color true "));
        assert!(ui.defaults_task.is_none() && !ui.draft_busy && !ui.saving_defaults);
        assert!(ui.inputs.is_empty());
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn fully_typed_settings_executes_client_only_without_a_hidden_default_selection() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    type_text(&mut ui, "/settings status-color true", &snapshot, &tx);
    ui.completion_selected = 1;
    assert!(!ui.completion_active);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.interface.status_color);
    assert!(ui.defaults_task.is_none() && !ui.saving_defaults && !ui.draft_busy);
    assert_eq!(ui.notice.as_ref().unwrap().0, "status-color enabled");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn make_default_spawns_worker_and_persists_only_the_selected_change() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let dir = tempfile::tempdir().unwrap();
    ui.preferences_path = dir.path().join("preferences.toml");
    ui.interface.status_line = false;
    ui.interface.prompt_history = false;
    let before = ui.interface.clone();
    let (tx, mut rx) = mpsc::channel(8);
    type_text(&mut ui, "/settings status braille ", &snapshot, &tx);
    select(&mut ui, "--default", &snapshot, &tx);
    assert!(ui.defaults_task.is_none());
    assert!(!ui.preferences_path.exists());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.defaults_task.is_some() && ui.saving_defaults && ui.draft_busy);
    assert_eq!(ui.interface, before);
    assert!(ui.settings_feedback.is_none());
    assert_eq!(ui.draft.text(), "/settings status braille --default ");
    assert!(ui.inputs.is_empty());
    press(&mut ui, KeyCode::Char('x'), &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings status braille --default ");
    assert!(ui.submit(&snapshot, &tx).is_err());
    assert!(rx.try_recv().is_err());
    // Mirror the run tick after the blocking worker completes, without a terminal.
    let change = ui.defaults_task.take().unwrap().await.unwrap().unwrap();
    assert_eq!(change, Change::Status(StatusPreset::Braille));
    assert_eq!(ui.interface, before);
    ui.complete_defaults(Ok(change)).unwrap();
    assert_eq!(
        ui.settings_feedback,
        Some((
            "Status preset: bars -> braille (saved for future clients)".into(),
            false,
        ))
    );
    assert_eq!(ui.interface.status_preset, StatusPreset::Braille);
    assert!(!ui.interface.status_line && !ui.interface.prompt_history);
    assert_eq!(
        std::fs::read_to_string(&ui.preferences_path).unwrap(),
        "[interface]\nstatus_preset = \"braille\"\n"
    );
    let loaded = Config::load_with_preferences(None, &ui.preferences_path).unwrap();
    assert!(loaded.interface.status_line && loaded.interface.prompt_history);
    assert_eq!(loaded.interface.status_preset, StatusPreset::Braille);
    assert_eq!(ui.inputs, ["/settings status braille --default"]);
    assert_eq!(
        ui.notice.as_ref().unwrap().0,
        "status set to braille (default)"
    );
    assert!(ui.draft.text().is_empty());
}

#[tokio::test]
async fn palette_default_save_preserves_the_leased_draft_through_acknowledgement() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    let dir = tempfile::tempdir().unwrap();
    ui.preferences_path = dir.path().join("preferences.toml");
    ui.draft.set_text("leased unsent text");
    ui.draft.set_cursor(7);
    ui.edit = Some(("lease-id".into(), 9));
    let (tx, mut rx) = mpsc::channel(8);
    ui.execute(Action::Commands, "", false, &snapshot, &tx)
        .unwrap();
    type_text(&mut ui, "/settings status-color true ", &snapshot, &tx);
    select(&mut ui, "--default", &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.defaults_palette && ui.saving_defaults && ui.draft_busy);
    assert_eq!(ui.draft.text(), "leased unsent text");
    assert_eq!(ui.draft.cursor(), 7);
    assert!(!ui.interface.status_color);
    assert!(
        ui.execute(Action::Settings, "", false, &snapshot, &tx)
            .is_err()
    );
    ui.finish_pending_defaults().await.unwrap();
    assert!(ui.defaults_task.is_none() && !ui.saving_defaults && !ui.draft_busy);
    assert_eq!(
        std::fs::read_to_string(&ui.preferences_path).unwrap(),
        "[interface]\nstatus_color = true\n"
    );
    assert!(ui.interface.status_color && !ui.palette);
    assert!(ui.palette_query.text().is_empty());
    assert_eq!(ui.draft.text(), "leased unsent text");
    assert_eq!(ui.draft.cursor(), 7);
    assert_eq!(ui.edit, Some(("lease-id".into(), 9)));
    assert_eq!(ui.inputs, ["/settings status-color true --default"]);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn failed_and_locked_default_saves_retain_input_and_client_preferences() {
    use fs2::FileExt;
    let snapshot = fixture_snapshot();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preferences.toml");
    let original = "[interface]\nstatus_line = false\n";
    std::fs::write(&path, original).unwrap();
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.path().join("preferences.toml.lock"))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    let (tx, mut rx) = mpsc::channel(8);
    for (path, busy) in [(dir.path().to_owned(), false), (path.clone(), true)] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.preferences_path = path;
        let before = ui.interface.clone();
        ui.draft.set_text("/settings status-color true ");
        select(&mut ui, "--default", &snapshot, &tx);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(ui.saving_defaults && ui.draft_busy);
        let error = ui.defaults_task.take().unwrap().await.unwrap().unwrap_err();
        if busy {
            assert!(format!("{error:#}").contains("busy"));
        }
        ui.saving_defaults = false;
        ui.draft_busy = false;
        ui.notify(
            format!(
                "Default not confirmed; client unchanged: {}",
                clean(&format!("{error:#}"), 1024)
            ),
            true,
        );
        assert_eq!(ui.interface, before);
        assert_eq!(ui.draft.text(), "/settings status-color true --default ");
        assert!(ui.inputs.is_empty());
        assert!(ui.notice.as_ref().unwrap().2);
        assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains("client unchanged"));
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn make_default_without_a_path_fails_without_applying_or_clearing() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let before = ui.interface.clone();
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("/settings status-color true ");
    select(&mut ui, "--default", &snapshot, &tx);
    let error = ui
        .key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            110,
            20,
        )
        .unwrap_err();
    assert!(error.to_string().contains("preferences path"));
    assert_eq!(ui.interface, before);
    assert_eq!(ui.draft.text(), "/settings status-color true --default ");
    assert!(ui.defaults_task.is_none() && !ui.saving_defaults && !ui.draft_busy);
    assert!(rx.try_recv().is_err());
}

#[test]
fn palette_settings_preserve_ordinary_and_leased_draft_and_caret() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for leased in [false, true] {
        let mut ui = Ui::new(&uncolored_config(), &snapshot);
        ui.draft.set_text("ordinary work in progress");
        ui.draft.set_cursor(8);
        if leased {
            ui.edit = Some(("lease".into(), 4));
        }
        let edit = ui.edit.clone();
        ui.execute(Action::Commands, "", false, &snapshot, &tx)
            .unwrap();
        type_text(&mut ui, "settings", &snapshot, &tx);
        assert_eq!(ui.palette_query.text(), "settings");
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert_eq!(ui.view, View::Settings);
        assert!(!ui.interface.status_color && !ui.palette);
        for _ in 0..3 {
            press(&mut ui, KeyCode::Down, &snapshot, &tx);
        }
        assert_eq!(ui.settings_selected, 3);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert_eq!(ui.settings_editor, Some(1));
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        assert_eq!(ui.settings_editor, Some(0));
        assert!(!ui.interface.status_color);
        assert_eq!(ui.draft.text(), "ordinary work in progress");
        assert_eq!(ui.draft.cursor(), 8);
        assert_eq!(ui.edit, edit);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(ui.interface.status_color && !ui.palette);
        assert!(ui.settings_editor.is_none() && ui.view == View::Settings);
        assert_eq!(ui.draft.text(), "ordinary work in progress");
        assert_eq!(ui.draft.cursor(), 8);
        assert_eq!(ui.edit, edit);
        assert!(!ui.draft_busy && ui.defaults_task.is_none());
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert_eq!(ui.view, View::Root);
        assert!(ui.palette && ui.commands_focus);
        assert_eq!(ui.palette_query.text(), "settings");
        ui.execute(Action::Commands, "", false, &snapshot, &tx)
            .unwrap();
        type_text(&mut ui, "/settings status-color ", &snapshot, &tx);
        select(&mut ui, "false", &snapshot, &tx);
        assert!(ui.interface.status_color && ui.completion_active);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(ui.interface.status_color && !ui.completion_active);
        assert_eq!(ui.draft.text(), "ordinary work in progress");
        assert_eq!(ui.draft.cursor(), 8);
        assert_eq!(ui.edit, edit);
        assert!(rx.try_recv().is_err());
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(!ui.interface.status_color && !ui.palette && !ui.draft_busy);
        assert_eq!(ui.draft.text(), "ordinary work in progress");
        assert_eq!(ui.draft.cursor(), 8);
        assert_eq!(ui.edit, edit);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn palette_enter_runs_noargs_but_draft_id_command_only_seeds_input() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.drafts.push(crate::runtime::DraftRecord {
        id: "abcdef12-1234-1111-1111-111111111111".into(),
        text: "saved draft".into(),
        revision: 4,
    });
    ui.draft.set_text("keep ordinary draft");
    ui.execute(Action::Commands, "", false, &snapshot, &tx)
        .unwrap();
    type_text(&mut ui, "stop", &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let stop = rx.try_recv().unwrap();
    assert!(matches!(
        stop.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(matches!(stop.op, Op::Stop { clear_draft: false }));
    assert_eq!(ui.draft.text(), "keep ordinary draft");
    assert!(!ui.draft_busy);
    complete(
        &mut ui,
        stop.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    ui.execute(Action::Commands, "", false, &snapshot, &tx)
        .unwrap();
    type_text(&mut ui, "send", &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.palette_query.text(), "/send ");
    assert!(ui.palette);
    assert!(rx.try_recv().is_err());
    let id = ui.drafts[0].id.clone();
    select(&mut ui, &id, &snapshot, &tx);
    assert_eq!(ui.palette_query.text(), format!("/send {id}"));
    assert!(rx.try_recv().is_err());
    assert_eq!(ui.draft.text(), "keep ordinary draft");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(
        matches!(rx.try_recv().unwrap().command, Command::DispatchDraftAccumulated { id: sent, revision: 4, expected_paused: false } if sent == id)
    );
    assert_eq!(ui.draft.text(), "keep ordinary draft");
}

#[test]
fn composer_keys_edit_in_the_middle_and_backspace_whole_graphemes() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    type_text(&mut ui, "ac", &snapshot, &tx);
    press(&mut ui, KeyCode::Left, &snapshot, &tx);
    press(&mut ui, KeyCode::Char('b'), &snapshot, &tx);
    assert_eq!(ui.draft.text(), "abc");
    assert_eq!(ui.draft.cursor(), 2);
    press(&mut ui, KeyCode::Delete, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "ab");
    ui.draft.set_text("x\u{1f469}\u{200d}\u{1f4bb}a\u{301}z");
    press(&mut ui, KeyCode::Left, &snapshot, &tx);
    press(&mut ui, KeyCode::Backspace, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "x\u{1f469}\u{200d}\u{1f4bb}z");
    press(&mut ui, KeyCode::Backspace, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "xz");
    assert_eq!(ui.draft.cursor(), 1);
    assert!(rx.try_recv().is_err());
}

#[test]
fn composer_home_end_and_word_motion_address_the_caret() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("first\none two\nlast");
    ui.draft.set_cursor(10);
    press(&mut ui, KeyCode::Home, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 6);
    press(&mut ui, KeyCode::End, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 13);
    ui.key(
        KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.draft.cursor(), 10);
    ui.key(
        KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.draft.cursor(), 6);
    ui.key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.draft.cursor(), 9);
    ui.key(
        KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.draft.cursor(), 13);
    assert_eq!(ui.draft.text(), "first\none two\nlast");
    assert!(rx.try_recv().is_err());
}

#[test]
fn shift_arrows_move_unicode_words_in_primary_secondary_and_leased_editors() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let text = "na\u{00ef}ve, \u{65e5}\u{672c} cafe\u{301}!";
    for (secondary, leased) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.palette = true;
        ui.palette_pinned = true;
        ui.commands_focus = secondary;
        if leased {
            ui.edit = Some(("leased".into(), 1));
        }
        ui.draft.set_text(text);
        ui.palette_query.set_text(text);
        ui.draft.set_cursor(2);
        ui.palette_query.set_cursor(2);
        if secondary {
            ui.palette_query.set_cursor(text.len());
        } else {
            ui.draft.set_cursor(text.len());
        }
        for (code, cursor) in [
            (KeyCode::Left, "na\u{00ef}ve, \u{65e5}\u{672c} ".len()),
            (KeyCode::Left, "na\u{00ef}ve, \u{65e5}".len()),
            (KeyCode::Left, "na\u{00ef}ve, ".len()),
            (KeyCode::Left, 0),
            (KeyCode::Left, 0),
            (KeyCode::Right, "na\u{00ef}ve".len()),
            (KeyCode::Right, "na\u{00ef}ve, \u{65e5}".len()),
            (KeyCode::Right, "na\u{00ef}ve, \u{65e5}\u{672c}".len()),
            (
                KeyCode::Right,
                "na\u{00ef}ve, \u{65e5}\u{672c} cafe\u{301}".len(),
            ),
            (KeyCode::Right, text.len()),
            (KeyCode::Right, text.len()),
        ] {
            ui.key(
                KeyEvent::new(code, KeyModifiers::SHIFT),
                &snapshot,
                &tx,
                110,
                20,
            )
            .unwrap();
            assert_eq!(ui.editor().cursor(), cursor);
            assert_eq!(ui.draft.text(), text);
            assert_eq!(ui.palette_query.text(), text);
            let inactive = if secondary {
                &ui.draft
            } else {
                &ui.palette_query
            };
            assert_eq!(inactive.cursor(), 2);
            assert_eq!(ui.edit, leased.then(|| ("leased".into(), 1)));
            assert!(!ui.completion_active && !ui.draft_busy);
        }
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn shift_arrows_do_not_accept_ghost_suggestions_even_with_ctrl_or_alt() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for secondary in [false, true] {
        for modifiers in [
            KeyModifiers::SHIFT,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
            KeyModifiers::SHIFT | KeyModifiers::ALT,
        ] {
            for (code, cursor) in [(KeyCode::Left, 1), (KeyCode::Right, 4)] {
                let mut ui = Ui::new(&Config::default(), &snapshot);
                ui.palette = secondary;
                ui.commands_focus = secondary;
                ui.draft.set_text("/his");
                ui.palette_query.set_text("/his");
                let menu = ui.completion(&snapshot).unwrap();
                assert_eq!(
                    menu.ghost(ui.editor().text(), ui.editor().cursor(), 0),
                    Some("tory")
                );
                ui.key(KeyEvent::new(code, modifiers), &snapshot, &tx, 110, 20)
                    .unwrap();
                assert_eq!(ui.editor().text(), "/his");
                assert_eq!(ui.editor().cursor(), cursor);
                assert!(!ui.completion_active && !ui.draft_busy);
            }
        }
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn shift_arrows_move_the_editor_and_reset_selected_completion() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for (code, cursor) in [(KeyCode::Left, 1), (KeyCode::Right, "/details ".len())] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text("/d");
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        assert!(ui.completion_active && ui.pager_selected);
        assert_eq!(ui.draft.text(), "/details ");
        ui.key(
            KeyEvent::new(code, KeyModifiers::SHIFT),
            &snapshot,
            &tx,
            110,
            20,
        )
        .unwrap();
        assert_eq!(ui.draft.text(), "/details ");
        assert_eq!(ui.draft.cursor(), cursor);
        assert!(!ui.completion_active && !ui.pager_selected);
        assert!(!ui.pager_selected);
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn multiline_arrows_move_before_recalling_prompt_history() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.remember("old prompt".into());
    ui.remember("saved one\ntwo\nthree".into());
    ui.draft.set_text("one\ntwo\nthree");
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 7);
    assert!(ui.recall_cursor.is_none());
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 3);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 7);
    assert_eq!(ui.draft.text(), "one\ntwo\nthree");
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "saved one\ntwo\nthree");
    assert_eq!(ui.recall_cursor, Some(1));
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "one\ntwo\nthree");
    assert!(ui.recall_cursor.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn unreadable_selection_never_accepts_applies_or_executes() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for (text, palette) in [
        ("/s", false),
        ("/settings status-color ", false),
        ("quit", true),
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.palette = palette;
        if palette {
            ui.palette_query.set_text(text);
        } else {
            ui.draft.set_text(text);
        }
        press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        let preview = ui.editor().text().to_owned();
        for (width, height) in [(1, 1), (23, 14), (110, 3)] {
            assert!(
                ui.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &snapshot,
                    &tx,
                    width,
                    height
                )
                .is_err()
            );
            assert_eq!(ui.editor().text(), preview);
            assert_eq!(ui.interface, Config::default().interface);
            assert!(ui.defaults_task.is_none() && !ui.exiting);
            assert!(rx.try_recv().is_err());
        }
    }
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    ui.draft.set_text("/settings status-color true ");
    press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            23,
            20
        )
        .is_err()
    );
    assert!(!ui.interface.status_color);
    assert!(ui.defaults_task.is_none() && !ui.draft_busy && !ui.saving_defaults);
    assert_eq!(ui.draft.text(), "/settings status-color true --default ");
    assert!(rx.try_recv().is_err());
}

#[test]
fn clipboard_paste_with_final_newline_never_submits() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for text in ["message\n", "/quit\n", "/settings status-color true\r\n"] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        // Event::Paste in run only calls append; the final newline is input, not Enter.
        ui.append(text);
        assert!(ui.draft.text().ends_with('\n'));
        assert!(!ui.completion_visible());
        assert!(!ui.draft_busy && !ui.exiting);
        assert!(rx.try_recv().is_err());
        assert_eq!(ui.interface, Config::default().interface);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(
            matches!(rx.try_recv().unwrap().command, Command::SubmitInputAccumulated { text: sent, .. } if sent == text.replace("\r\n", "\n"))
        );
    }
}

#[test]
fn history_is_always_fullscreen_and_navigation_shortcuts_preserve_draft() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for args in ["", "fullscreen"] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text(&format!(
            "/history{}",
            if args.is_empty() { "" } else { " fullscreen" }
        ));
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert_eq!(ui.view, View::History);
        assert!(ui.full_history);
        assert!(matches!(
            rx.try_recv().unwrap().command,
            Command::Transcript { .. }
        ));
        assert!(matches!(
            rx.try_recv().unwrap().command,
            Command::Actions { .. }
        ));
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(!ui.full_history);
    }
    for action in [
        Action::Help,
        Action::History,
        Action::Queue,
        Action::Details,
        Action::Settings,
        Action::Commands,
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text("retained draft");
        ui.draft.set_cursor(4);
        ui.execute(action, "", false, &snapshot, &tx).unwrap();
        assert_eq!(ui.full_history, action == Action::History);
        assert_eq!(ui.draft.text(), "retained draft");
        assert_eq!(ui.draft.cursor(), 4);
        if action == Action::History {
            assert!(matches!(
                rx.try_recv().unwrap().command,
                Command::Transcript { .. }
            ));
            assert!(matches!(
                rx.try_recv().unwrap().command,
                Command::Actions { .. }
            ));
            ui.execute(action, "", false, &snapshot, &tx).unwrap();
            assert_eq!(ui.view, View::Root);
            assert_eq!(ui.draft.text(), "retained draft");
        }
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn unique_tab_inserts_space_and_next_enter_executes_without_an_acceptance_stage() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("/qui");
    assert!(!ui.completion_visible());
    assert!(ui.completion(&snapshot).is_some());
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/quit ");
    assert!(!ui.completion_active && !ui.pager_selected && !ui.draft_busy);
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
}

#[test]
fn completion_escape_restores_original_utf8_suffix_and_exact_caret_without_interrupting() {
    let mut snapshot = fixture_snapshot();
    snapshot.active = Some(crate::runtime::ActiveView {
        attempt_id: "unchanged-attempt".into(),
        text: "partial".into(),
        truncated: false,
        message_id: String::new(),
        durable_bytes: 0,
    });
    let (tx, mut rx) = mpsc::channel(8);
    for (text, cursor) in [("/s  \u{e9}lan", 2), ("/hisBAD\u{2003}fullscreen", 4)] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text(text);
        ui.draft.set_cursor(cursor);
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        if ui.completion_active {
            assert!(!ui.pager_selected);
            press(&mut ui, KeyCode::Tab, &snapshot, &tx);
            press(&mut ui, KeyCode::Right, &snapshot, &tx);
            assert_eq!(ui.draft.text(), "/settings  \u{e9}lan");
            press(&mut ui, KeyCode::Left, &snapshot, &tx);
            assert_eq!(ui.draft.text(), "/send  \u{e9}lan");
        } else {
            assert_eq!(ui.draft.text(), "/history\u{2003}fullscreen");
        }
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert_eq!(ui.draft.text(), text);
        assert_eq!(ui.draft.cursor(), cursor);
        assert!(!ui.completion_active && !ui.pager_selected);
        assert!(!ui.draft_busy && !ui.exiting && ui.inputs.is_empty());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            snapshot.active.as_ref().unwrap().attempt_id,
            "unchanged-attempt"
        );
    }
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("ordinary unfinished draft");
    ui.draft.set_cursor(8);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "ordinary unfinished draft");
    assert_eq!(ui.draft.cursor(), 8);
    assert!(rx.try_recv().is_err());
}

#[test]
fn printable_editing_commits_preview_but_space_never_accepts_an_invisible_candidate() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&uncolored_config(), &snapshot);
    ui.draft.set_text("/s");
    press(&mut ui, KeyCode::Char(' '), &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/s ");
    ui.draft.set_text("/s");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    press(&mut ui, KeyCode::Char('x'), &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/send x");
    assert!(!ui.completion_active && !ui.pager_selected);
    assert!(ui.submit(&snapshot, &tx).is_err());
    assert!(rx.try_recv().is_err());
    ui.draft.set_text("/settings status-color true");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings status-color true ");
    assert!(!ui.interface.status_color && ui.defaults_task.is_none());
    press(&mut ui, KeyCode::Char(' '), &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/settings status-color true  ");
    assert!(!ui.completion_active);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.interface.status_color);
    assert!(ui.defaults_task.is_none() && rx.try_recv().is_err());
}

#[test]
fn ghost_acceptance_binds_right_ctrl_f_and_suggested_word_without_execution() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for (code, modifiers, text, expected) in [
        (KeyCode::Right, KeyModifiers::NONE, "/his", "/history"),
        (KeyCode::End, KeyModifiers::NONE, "/his", "/history"),
        (
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
            "/his",
            "/history",
        ),
        (KeyCode::Char('f'), KeyModifiers::CONTROL, "/sto", "/stop"),
        (
            KeyCode::Right,
            KeyModifiers::ALT,
            "/settings sta",
            "/settings status",
        ),
        (
            KeyCode::Char('f'),
            KeyModifiers::ALT,
            "/settings sta",
            "/settings status",
        ),
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text(text);
        ui.key(KeyEvent::new(code, modifiers), &snapshot, &tx, 110, 20)
            .unwrap();
        assert_eq!(ui.draft.text(), expected);
        assert_eq!(ui.draft.cursor(), expected.len());
        assert!(!ui.completion_active && !ui.draft_busy && !ui.exiting);
        assert!(rx.try_recv().is_err());
    }
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("/his");
    ui.draft.set_cursor(2);
    press(&mut ui, KeyCode::Right, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/his");
    assert_eq!(ui.draft.cursor(), 3);
}

#[test]
fn explicit_pager_search_filters_cached_candidates_and_backspace_restores_matches() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for shift_first in [false, true] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text("/s");
        if shift_first {
            press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
        } else {
            press(&mut ui, KeyCode::Tab, &snapshot, &tx);
            ui.key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &snapshot,
                &tx,
                110,
                20,
            )
            .unwrap();
        }
        assert!(ui.pager_search.is_some() && !ui.pager_selected);
        type_text(&mut ui, "stop", &snapshot, &tx);
        assert_eq!(ui.pager_search.as_ref().unwrap().text(), "stop");
        press(&mut ui, KeyCode::Left, &snapshot, &tx);
        assert_eq!(ui.pager_search.as_ref().unwrap().cursor(), 3);
        press(&mut ui, KeyCode::Right, &snapshot, &tx);
        assert_eq!(ui.pager_search.as_ref().unwrap().cursor(), 4);
        assert_eq!(ui.completion(&snapshot).unwrap().items.len(), 1);
        assert_eq!(ui.draft.text(), "/stop ");
        assert!(ui.pager_selected);
        press(&mut ui, KeyCode::Char('x'), &snapshot, &tx);
        assert!(ui.completion(&snapshot).unwrap().items.is_empty());
        assert!(!ui.pager_selected);
        assert_eq!(ui.draft.text(), "/s");
        press(&mut ui, KeyCode::Backspace, &snapshot, &tx);
        assert_eq!(ui.completion(&snapshot).unwrap().items.len(), 1);
        assert_eq!(ui.draft.text(), "/stop ");
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert_eq!(ui.draft.text(), "/s");
        assert_eq!(ui.draft.cursor(), 2);
        assert!(ui.pager_search.is_none() && !ui.completion_active);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn pager_search_enter_commits_only_and_empty_search_results_never_submit() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for query in ["stop", "no matches"] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text("/s");
        press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
        type_text(&mut ui, query, &snapshot, &tx);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(!ui.completion_active && ui.pager_search.is_none() && !ui.exiting);
        assert!(rx.try_recv().is_err());
        if query == "stop" {
            press(&mut ui, KeyCode::Enter, &snapshot, &tx);
            let stop = rx.try_recv().unwrap();
            assert!(matches!(
                stop.command,
                Command::StopPending {
                    expected_attempt_id: None,
                    expected_paused: false,
                }
            ));
            assert!(matches!(stop.op, Op::Stop { clear_draft: true }));
            complete(
                &mut ui,
                stop.op,
                Ok(Reply::Stopped {
                    paused: true,
                    cancelled: false,
                }),
            );
        } else {
            assert_eq!(ui.draft.text(), "/s");
        }
    }
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("/s");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(!ui.pager_selected);
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &snapshot,
        &tx,
        1,
        1,
    )
    .unwrap();
    assert!(!ui.completion_active && !ui.exiting);
    assert!(rx.try_recv().is_err());
}

#[test]
fn pager_navigation_is_reversible_and_cached_ids_are_revalidated_before_dispatch() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.drafts = (1..=8)
        .map(|number| crate::runtime::DraftRecord {
            id: format!("aaaa{number:04}-1234-1111-1111-111111111111"),
            text: format!("saved draft {number}"),
            revision: number,
        })
        .collect();
    ui.draft.set_text("/send aaaa");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(!ui.pager_selected);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 0);
    press(&mut ui, KeyCode::PageDown, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 6);
    press(&mut ui, KeyCode::PageUp, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 0);
    press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 7);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 6);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.completion_selected, 7);
    let selected_id = ui.drafts[7].id.clone();
    ui.drafts.pop();
    assert_eq!(ui.completion(&snapshot).unwrap().items.len(), 8);
    assert_eq!(ui.draft.text(), format!("/send {selected_id}"));
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(rx.try_recv().is_err());
    assert!(ui.submit(&snapshot, &tx).is_err());
    assert!(!ui.draft_busy && rx.try_recv().is_err());
    assert_eq!(ui.drafts.len(), 7);
}

#[test]
fn completion_byte_budget_includes_original_suffix_and_rejects_without_truncation() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft = input::Editor::new(9);
    ui.draft.set_text("/his \u{e9}x");
    ui.draft.set_cursor(4);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &snapshot,
            &tx,
            110,
            20
        )
        .is_err()
    );
    assert_eq!(ui.draft.text(), "/his \u{e9}x");
    assert_eq!(ui.draft.cursor(), 4);
    assert!(!ui.completion_active && !ui.draft_busy && rx.try_recv().is_err());
    ui.reset_completion();
    ui.draft = input::Editor::new(5);
    ui.draft.set_text("/s");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(ui.completion_active && !ui.pager_selected);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &snapshot,
            &tx,
            110,
            20
        )
        .is_err()
    );
    assert_eq!(ui.draft.text(), "/s");
    assert!(!ui.pager_selected);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/s");
    assert_eq!(ui.draft.cursor(), 2);
}

#[test]
fn ctrl_home_end_address_full_editor_and_client_setting_sources_are_per_key() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("first\nsecond\nlast");
    ui.draft.set_cursor(8);
    ui.key(
        KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.draft.cursor(), 0);
    ui.key(
        KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.draft.cursor(), ui.draft.text().len());
    let source = ui.setting_sources["status"].clone();
    ui.draft.set_text("/settings status-color true");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.setting_sources["status-color"], "This client");
    assert_eq!(ui.setting_sources["status"], source);
    assert!(rx.try_recv().is_err());
}

#[test]
fn busy_and_unavailable_command_worker_retain_completed_text_and_runtime_targets() {
    let snapshot = fixture_snapshot();
    let (tx, rx) = mpsc::channel(8);
    drop(rx);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.drafts.push(crate::runtime::DraftRecord {
        id: "abcd1234-1234-1111-1111-111111111111".into(),
        text: "original".into(),
        revision: 9,
    });
    ui.draft.set_text("/send abcd");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let completed = ui.draft.text().to_owned();
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.draft.text(), completed);
    assert!(!ui.draft_busy && ui.outstanding == 0);
    assert!(ui.notice.as_ref().unwrap().2);
    assert_eq!(ui.drafts[0].revision, 9);
    assert_eq!(ui.drafts[0].text, "original");
    ui.draft_busy = true;
    for code in [
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Down,
        KeyCode::Esc,
        KeyCode::Char('x'),
        KeyCode::Enter,
    ] {
        press(&mut ui, code, &snapshot, &tx);
        assert_eq!(ui.draft.text(), completed);
    }
    assert!(!ui.completion_active && ui.edit.is_none() && ui.defaults_task.is_none());
    assert!(ui.submit(&snapshot, &tx).is_err());
}

#[test]
fn palette_completion_enter_never_launches_and_preserves_ordinary_or_leased_caret() {
    let snapshot = fixture_snapshot();
    for leased in [false, true] {
        let (tx, mut rx) = mpsc::channel(8);
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text("ordinary \u{e9}lan draft");
        ui.draft.set_cursor(9);
        if leased {
            ui.edit = Some(("lease-id".into(), 3));
        }
        let edit = ui.edit.clone();
        ui.execute(Action::Commands, "", false, &snapshot, &tx)
            .unwrap();
        type_text(&mut ui, "i", &snapshot, &tx);
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        assert!(ui.completion_active && !ui.pager_selected);
        select(&mut ui, "/quit", &snapshot, &tx);
        assert_eq!(ui.palette_query.text(), "/quit ");
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(ui.palette && !ui.completion_active && !ui.exiting);
        assert!(rx.try_recv().is_err());
        assert_eq!(ui.draft.text(), "ordinary \u{e9}lan draft");
        assert_eq!(ui.draft.cursor(), 9);
        assert_eq!(ui.edit, edit);
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(matches!(rx.try_recv().unwrap().command, Command::Shutdown));
        assert_eq!(ui.draft.text(), "ordinary \u{e9}lan draft");
        assert_eq!(ui.draft.cursor(), 9);
        assert_eq!(ui.edit, edit);
    }
}

#[test]
fn unique_value_replaces_unicode_token_under_caret_and_preserves_following_flag() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let text = "/settings\u{2003}status\tbr\u{e9}\u{2003}--default";
    ui.draft.set_text(text);
    let cursor = text.find('\u{e9}').unwrap();
    ui.draft.set_cursor(cursor);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(
        ui.draft.text(),
        "/settings\u{2003}status\tbraille\u{2003}--default"
    );
    assert_eq!(ui.draft.cursor(), "/settings\u{2003}status\tbraille".len());
    assert!(!ui.completion_active && ui.defaults_task.is_none());
    assert_eq!(ui.interface, Config::default().interface);
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.draft.text(), text);
    assert_eq!(ui.draft.cursor(), cursor);
}

#[test]
fn invalid_settings_flags_do_not_apply_clear_or_dispatch_and_palette_cannot_send_its_draft() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for text in [
        "/settings status text --save",
        "/settings status text --default --default",
        "/settings status text extra",
        "/settings status --default text",
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text(text);
        assert!(ui.submit(&snapshot, &tx).is_err());
        assert_eq!(ui.draft.text(), text);
        assert_eq!(ui.interface, Config::default().interface);
        assert!(ui.defaults_task.is_none() && ui.inputs.is_empty() && !ui.draft_busy);
        assert!(rx.try_recv().is_err());
    }
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("retained leased draft");
    ui.draft.set_cursor(5);
    ui.edit = Some(("lease".into(), 8));
    ui.execute(Action::Commands, "", false, &snapshot, &tx)
        .unwrap();
    ui.palette_query.set_text("//literal");
    assert!(ui.submit(&snapshot, &tx).is_err());
    assert_eq!(ui.draft.text(), "retained leased draft");
    assert_eq!(ui.draft.cursor(), 5);
    assert_eq!(ui.edit, Some(("lease".into(), 8)));
    assert!(rx.try_recv().is_err());
}

#[test]
fn filtered_recall_empty_draft_visits_unique_inputs_newest_first_and_back() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    for text in [
        "first",
        "repeated",
        "middle",
        "repeated",
        "/settings",
        "/settings",
    ] {
        ui.remember(text.into());
    }
    for text in ["/settings", "repeated", "middle", "first"] {
        ui.recall(true);
        assert_eq!(ui.draft.text(), text);
        assert!(!ui.completion_visible());
    }
    ui.recall(true);
    assert_eq!(ui.draft.text(), "first");
    for text in ["middle", "repeated", "/settings", ""] {
        ui.recall(false);
        assert_eq!(ui.draft.text(), text);
    }
    assert!(ui.recall_cursor.is_none());
    assert_eq!(ui.draft.cursor(), 0);
    ui.recall(false);
    assert_eq!(ui.draft.text(), "");
}

#[test]
fn filtered_recall_freezes_entire_case_insensitive_substring_query() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    for text in [
        "old NEEDLE phrase result",
        "unrelated",
        "new needle PHRASE result",
        "needle without the rest",
        "new needle PHRASE result",
    ] {
        ui.remember(text.into());
    }
    ui.draft.set_text("NeEdLe PhRaSe");
    assert!(ui.draft.set_cursor(3));
    ui.recall(true);
    assert_eq!(ui.draft.text(), "new needle PHRASE result");
    assert_eq!(ui.recall_cursor, Some(4));
    ui.recall(true);
    assert_eq!(ui.draft.text(), "old NEEDLE phrase result");
    assert_eq!(ui.recall_cursor, Some(0));
    ui.recall(false);
    assert_eq!(ui.draft.text(), "new needle PHRASE result");
    ui.recall(false);
    assert_eq!(ui.draft.text(), "NeEdLe PhRaSe");
    assert_eq!(ui.draft.cursor(), 3);
    assert!(ui.recall_cursor.is_none());
}

#[test]
fn filtered_recall_restores_exact_unicode_draft_and_grapheme_byte_caret() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let text = "\u{c9} \u{1f469}\u{200d}\u{1f4bb} e\u{301}";
    let cursor = "\u{c9} \u{1f469}\u{200d}\u{1f4bb}".len();
    ui.remember(format!("before {} after", text.to_lowercase()));
    ui.draft.set_text(text);
    assert!(ui.draft.set_cursor(cursor));
    ui.recall(true);
    assert_eq!(
        ui.draft.text(),
        format!("before {} after", text.to_lowercase())
    );
    ui.recall(false);
    assert_eq!(ui.draft.text(), text);
    assert_eq!(ui.draft.cursor(), cursor);
    assert!(ui.recall_cursor.is_none());
    ui.recall(true);
    ui.recall(false);
    assert_eq!(ui.draft.text(), text);
    assert_eq!(ui.draft.cursor(), cursor);
}

#[test]
fn filtered_recall_no_matches_leaves_text_caret_and_completion_unchanged() {
    let snapshot = fixture_snapshot();
    for has_history in [false, true] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        if has_history {
            ui.remember("prefix \u{e9} only".into());
            ui.remember("another input".into());
        }
        let text = "\u{e9} missing suffix";
        ui.draft.set_text(text);
        assert!(ui.draft.set_cursor("\u{e9}".len()));
        ui.completion_active = true;
        ui.completion_selected = 2;
        for previous in [false, true, true, false] {
            ui.recall(previous);
            assert_eq!(ui.draft.text(), text);
            assert_eq!(ui.draft.cursor(), "\u{e9}".len());
            assert!(ui.recall_cursor.is_none());
            assert!(ui.recall_draft.is_empty());
            assert!(ui.completion_active && !ui.completion_hidden);
            assert_eq!(ui.completion_selected, 2);
        }
    }
}

#[test]
fn filtered_recall_oldest_exhaustion_does_not_move_caret_or_wrap() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    for text in [
        "not a match",
        "old target",
        "unrelated",
        "new target",
        "other",
    ] {
        ui.remember(text.into());
    }
    ui.draft.set_text("TARGET");
    ui.recall(true);
    ui.recall(true);
    assert_eq!(ui.draft.text(), "old target");
    assert_eq!(ui.recall_cursor, Some(1));
    assert!(ui.draft.set_cursor(2));
    for _ in 0..3 {
        ui.recall(true);
        assert_eq!(ui.draft.text(), "old target");
        assert_eq!(ui.draft.cursor(), 2);
        assert_eq!(ui.recall_cursor, Some(1));
    }
    ui.recall(false);
    assert_eq!(ui.draft.text(), "new target");
    ui.recall(false);
    assert_eq!(ui.draft.text(), "TARGET");
    assert_eq!(ui.draft.cursor(), "TARGET".len());
}

#[test]
fn filtered_recall_respects_prompt_history_busy_and_edit_lease_guards() {
    let snapshot = fixture_snapshot();
    for guard in 0..3 {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.remember("old target".into());
        ui.draft.set_text("target");
        assert!(ui.draft.set_cursor(2));
        match guard {
            0 => ui.interface.prompt_history = false,
            1 => ui.draft_busy = true,
            _ => ui.edit = Some(("lease".into(), 8)),
        }
        for previous in [true, false] {
            ui.recall(previous);
            assert_eq!(ui.draft.text(), "target");
            assert_eq!(ui.draft.cursor(), 2);
            assert!(ui.recall_cursor.is_none());
        }
    }
}

#[test]
fn filtered_recall_multiline_arrows_keep_editor_movement_precedence() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.remember("prefix one\ntwo\nthree suffix".into());
    ui.draft.set_text("one\ntwo\nthree");
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 7);
    assert!(ui.recall_cursor.is_none());
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 3);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.draft.cursor(), 7);
    assert!(ui.recall_cursor.is_none());
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "prefix one\ntwo\nthree suffix");
    assert_eq!(ui.recall_cursor, Some(0));
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "prefix one\ntwo\nthree suffix");
    assert_eq!(ui.recall_cursor, Some(0));
    assert!(ui.draft.cursor() < ui.draft.text().len());
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.recall_cursor, Some(0));
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "one\ntwo\nthree");
    assert_eq!(ui.draft.cursor(), 3);
    assert!(ui.recall_cursor.is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn filtered_recall_editing_starts_a_new_query_session() {
    let snapshot = fixture_snapshot();
    let (tx, _rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.remember("older target plus match".into());
    ui.remember("target".into());
    ui.draft.set_text("target");
    ui.recall(true);
    ui.append(" plus");
    assert!(ui.recall_cursor.is_none());
    ui.recall(true);
    assert_eq!(ui.draft.text(), "older target plus match");
    ui.recall(false);
    assert_eq!(ui.draft.text(), "target plus");
    ui.recall(true);
    press(&mut ui, KeyCode::Backspace, &snapshot, &tx);
    assert!(ui.recall_cursor.is_none());
    ui.recall(true);
    assert_eq!(ui.draft.text(), "older target plus match");
    ui.recall(false);
    assert_eq!(ui.draft.text(), "older target plus matc");
}

#[test]
fn settings_panel_opens_only_after_enter_and_never_submits_primary_input() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    type_text(&mut ui, "/settings", &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!(ui.interface, Config::default().interface);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Settings);
    assert!(ui.draft.text().is_empty());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.settings_editor.is_some());
    assert!(ui.interface.status_line);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert!(ui.interface.status_line);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(!ui.interface.status_line);
    assert!(ui.settings_editor.is_none());
    assert_eq!(ui.view, View::Settings);
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_panel_toggle_enum_and_escape_preserve_both_fields_and_edit_lease() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("protected draft");
    ui.draft.set_cursor(4);
    ui.edit = Some(("lease".into(), 7));
    ui.palette = true;
    ui.palette_pinned = true;
    ui.commands_focus = false;
    ui.palette_query.set_text("retained command");
    ui.palette_query.set_cursor(3);
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    press(&mut ui, KeyCode::Char(' '), &snapshot, &tx);
    assert!(!ui.interface.status_line);
    assert_eq!(
        ui.settings_feedback,
        Some(("Status line: on -> off".into(), false))
    );
    ui.settings_selected = 2;
    let before = ui.interface.status_preset;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.settings_editor, Some(3));
    assert_eq!(
        crate::settings::catalog(&ui.interface)[2].choices,
        ["text", "braille", "minimal", "bars"]
    );
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.settings_editor, Some(3));
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.interface.status_preset, before);
    assert_eq!(ui.view, View::Settings);
    assert_eq!(
        ui.settings_feedback,
        Some(("Status line: on -> off".into(), false))
    );
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert_eq!(ui.settings_editor, Some(2));
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.interface.status_preset, StatusPreset::Minimal);
    assert_eq!(
        ui.settings_feedback,
        Some(("Status preset: bars -> minimal".into(), false))
    );
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("protected draft", 4));
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("retained command", 3)
    );
    assert!(ui.palette && ui.palette_pinned && !ui.commands_focus);
    assert_eq!(ui.edit, Some(("lease".into(), 7)));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn settings_panel_save_ack_preserves_newer_input_and_all_four_defaults() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let dir = tempfile::tempdir().unwrap();
    ui.preferences_path = dir.path().join("preferences.toml");
    crate::settings::save_keys(&ui.preferences_path, &ui.keys).unwrap();
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("primary");
    ui.draft.set_cursor(2);
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    ui.interface.status_line = false;
    ui.interface.prompt_history = false;
    ui.interface.status_color = false;
    ui.interface.status_preset = StatusPreset::Minimal;
    let saved_interface = ui.interface.clone();
    ui.settings_selected = 4;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(ui.saving_defaults && ui.defaults_task.is_some());
    assert!(!ui.draft_busy && ui.notice.is_none());
    assert!(ui.settings_feedback.is_none());
    assert!(!plain(&rendered(&ui, &snapshot, 110, 20)).contains("saved for future clients"));
    let task = ui.defaults_task.as_ref().unwrap().id();
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            100,
            20
        )
        .is_err()
    );
    assert_eq!(ui.defaults_task.as_ref().unwrap().id(), task);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    ui.draft.set_text("newer primary");
    ui.draft.set_cursor(3);
    ui.palette = true;
    ui.palette_query.set_text("newer command");
    ui.palette_query.set_cursor(5);
    ui.interface.status_line = true;
    ui.draft_busy = true; // A separate operation must retain its busy flag.
    ui.finish_pending_defaults().await.unwrap();
    assert!(ui.draft_busy && ui.interface.status_line);
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("newer primary", 3));
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("newer command", 5)
    );
    assert_eq!(ui.view, View::Root);
    assert!(!ui.saving_defaults);
    assert_eq!(
        ui.notice.as_ref().unwrap().0,
        "UI defaults saved for future clients"
    );
    assert_eq!(
        ui.settings_feedback,
        Some(("UI defaults saved for future clients".into(), false))
    );
    let saved = Config::load_with_preferences(None, &ui.preferences_path).unwrap();
    assert_eq!(saved.interface, saved_interface);
    assert_eq!(saved.keys.commands, ui.keys.commands);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn settings_panel_save_errors_do_not_change_fields_navigation_or_preferences() {
    use fs2::FileExt;
    let snapshot = fixture_snapshot();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preferences.toml");
    std::fs::write(&path, "[interface]\nstatus_line = false\n").unwrap();
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.path().join("preferences.toml.lock"))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    let (tx, mut rx) = mpsc::channel(8);
    for preferences in [
        std::path::PathBuf::new(),
        dir.path().to_owned(),
        path.clone(),
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.preferences_path = preferences;
        ui.draft.set_text("protected");
        ui.draft.set_cursor(2);
        ui.execute(Action::Settings, "", false, &snapshot, &tx)
            .unwrap();
        ui.settings_selected = 4;
        let before = ui.interface.clone();
        let result = ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            100,
            20,
        );
        if ui.preferences_path.as_os_str().is_empty() {
            assert!(result.is_err());
            assert!(ui.defaults_task.is_none());
        } else {
            result.unwrap();
            assert!(ui.finish_pending_defaults().await.is_err());
            assert!(ui.notice.as_ref().unwrap().2);
        }
        assert_eq!(ui.view, View::Settings);
        assert_eq!(ui.interface, before);
        assert_eq!((ui.draft.text(), ui.draft.cursor()), ("protected", 2));
        assert!(!ui.saving_defaults && !ui.draft_busy);
        let (feedback, warning) = ui.settings_feedback.as_ref().unwrap();
        assert!(
            *warning
                && feedback.contains(if ui.preferences_path.as_os_str().is_empty() {
                    "Defaults not saved"
                } else {
                    "Defaults not confirmed; client unchanged"
                })
        );
        assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains("Defaults not"));
    }
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "[interface]\nstatus_line = false\n"
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_feedback_displays_old_and_new_values_after_elapsed_time_and_during_generation() {
    let mut snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    ui.settings_selected = 2;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert!(ui.settings_feedback.is_none());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let expected = "Status preset: bars -> minimal";
    assert_eq!(ui.settings_feedback, Some((expected.into(), false)));
    ui.notice.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(30);
    assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains(expected));
    snapshot.phase = Phase::Receiving;
    snapshot.active = Some(crate::runtime::ActiveView {
        attempt_id: "generating".into(),
        text: "partial reply".into(),
        truncated: false,
        message_id: String::new(),
        durable_bytes: 0,
    });
    assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains(expected));
    ui.notice = None;
    assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains(expected));
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.settings_feedback, Some((expected.into(), false)));
    assert_eq!(ui.interface.status_preset, StatusPreset::Minimal);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains(expected));
    for (index, label) in [
        (0, "Status line"),
        (1, "Prompt history"),
        (3, "Status color"),
    ] {
        ui.settings_selected = index;
        for transition in ["on -> off", "off -> on"] {
            press(&mut ui, KeyCode::Char(' '), &snapshot, &tx);
            let expected = format!("{label}: {transition}");
            assert_eq!(ui.settings_feedback, Some((expected.clone(), false)));
            assert!(plain(&rendered(&ui, &snapshot, 110, 20)).contains(&expected));
        }
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_feedback_rejected_apply_warns_without_changing_values_or_closing_the_choice() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    ui.settings_selected = 2;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    ui.saving_defaults = true;
    let before = ui.interface.clone();
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            110,
            20,
        )
        .is_err()
    );
    assert_eq!(ui.interface, before);
    assert_eq!(ui.settings_editor, Some(2));
    assert!(ui.inputs.is_empty() && ui.pending_notices.is_empty());
    let (text, warning) = ui.settings_feedback.as_ref().unwrap();
    assert!(*warning && text.starts_with("Setting not applied:"));
    let displayed = plain(&rendered(&ui, &snapshot, 110, 20));
    assert!(displayed.contains("Setting not applied:"));
    assert!(!displayed.contains("bars -> minimal"));
    let feedback = ui.settings_feedback.clone();
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.settings_feedback, feedback);
    ui.saving_defaults = false;
    ui.execute(Action::Settings, "status bars", false, &snapshot, &tx)
        .unwrap();
    assert_eq!(
        ui.settings_feedback,
        Some(("Status preset: bars (unchanged)".into(), false))
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_feedback_has_a_fixed_bottom_row_and_tiny_dropdowns_prioritize_the_choice() {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.view = View::Settings;
    let render_body = |ui: &Ui, height| {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(100, height + 1)).unwrap();
        terminal
            .draw(|frame| {
                panels::render_settings(
                    frame,
                    ui,
                    Rect::new(0, 0, 100, 1),
                    Rect::new(0, 1, 100, height),
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    };
    let row = |buffer: &Buffer, y| {
        (0..100)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let preferred = panels::body_rows(&ui);
    assert_eq!(preferred, 9);
    assert_eq!(row(&render_body(&ui, preferred), preferred), "");
    let expected = "Status preset: bars -> minimal";
    ui.settings_feedback = Some((expected.into(), false));
    assert_eq!(panels::body_rows(&ui), preferred);
    ui.settings_selected = 5;
    for height in [1, 2, 4, preferred] {
        let buffer = render_body(&ui, height);
        assert_eq!(row(&buffer, height), format!("  {expected}"));
        if height > 1 {
            assert!(plain(&buffer).contains("Bindings"));
        }
    }
    ui.settings_selected = 2;
    ui.settings_editor = Some(2);
    assert_eq!(panels::body_rows(&ui), 13);
    for height in [1, 2, 4, 13] {
        let buffer = render_body(&ui, height);
        assert!(plain(&buffer).contains("minimal"));
        let choice_y = (1..=height)
            .find(|y| row(&buffer, *y) == "    \u{25cb} minimal")
            .unwrap();
        assert!(buffer[(4, choice_y)].modifier.contains(Modifier::BOLD));
        if height == 1 {
            assert_eq!(row(&buffer, height), "    \u{25cb} minimal");
        } else {
            assert_eq!(row(&buffer, height), format!("  {expected}"));
        }
    }
    render_body(&ui, 0);
    ui.settings_editor = None;
    ui.settings_feedback = Some(("Defaults not saved: retry".into(), true));
    let buffer = render_body(&ui, 1);
    assert_eq!(row(&buffer, 1), "  Defaults not saved: retry");
    assert_eq!(buffer[(2, 1)].fg, Color::Indexed(252));
}

#[test]
fn interactive_help_nested_settings_and_bindings_escape_returns_with_selection() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("protected draft");
    ui.draft.set_cursor(4);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    ui.help_category = 4;
    ui.help_selected = ui
        .help_commands()
        .iter()
        .position(|spec| spec.action == Action::Settings)
        .unwrap();
    let selected = ui.help_selected;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Settings);
    ui.settings_selected = 5;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Bindings);
    press(&mut ui, KeyCode::Char('n'), &snapshot, &tx);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Bindings);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Settings);
    assert_eq!(ui.settings_selected, 5);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Help);
    assert_eq!(ui.help_selected, selected);
    assert_eq!(ui.help_category, 4);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("protected draft", 4));
    assert!(rx.try_recv().is_err());
}

#[test]
fn interactive_help_actions_require_explicit_run_or_insert_and_escape_is_local() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("protected draft");
    ui.draft.set_cursor(4);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    type_text(&mut ui, "s", &snapshot, &tx);
    assert!(ui.help_commands().len() > 1);
    ui.help_selected = ui
        .help_commands()
        .iter()
        .position(|spec| spec.action == Action::Stop)
        .unwrap();
    let selected = ui.help_selected;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.help_detail.unwrap().action, Action::Stop);
    assert_eq!(ui.help_detail_selected, 1);
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!(ui.palette_query.text(), "/stop");
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("protected draft", 4));
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Help);
    assert_eq!(ui.help_selected, selected);
    assert_eq!(ui.help_query.text(), "s");
    assert!(ui.help_detail.is_some());
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let stop = rx.try_recv().unwrap();
    assert!(matches!(
        stop.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(matches!(stop.op, Op::Stop { clear_draft: false }));
    complete(
        &mut ui,
        stop.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    assert!(ui.help_detail.is_none());
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert!(rx.try_recv().is_err());
}

#[test]
fn interactive_help_categories_and_panel_links_keep_the_primary_caret() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    for (action, view) in [
        (Action::Settings, View::Settings),
        (Action::Bindings, View::Bindings),
        (Action::Drafts, View::Drafts),
        (Action::Details, View::Details),
        (Action::History, View::History),
        (Action::Commands, View::Root),
    ] {
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.draft.set_text("primary draft");
        ui.draft.set_cursor(3);
        ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
        assert!(
            crate::commands::COMMANDS
                .iter()
                .all(|spec| state::HELP_CATEGORIES.contains(&spec.category())),
            "Every command category must have a help tab"
        );
        for (category, label) in state::HELP_CATEGORIES.iter().enumerate().skip(1) {
            ui.help_category = category;
            let commands = ui.help_commands();
            assert!(!commands.is_empty(), "category {label} must have commands");
            assert!(commands.iter().all(|spec| spec.category() == *label));
        }
        ui.help_category = 0;
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        assert!(
            ui.help_commands()
                .iter()
                .all(|spec| spec.category() == "AM")
        );
        press(&mut ui, KeyCode::BackTab, &snapshot, &tx);
        assert_eq!(ui.help_category, 0);
        ui.help_selected = ui
            .help_commands()
            .iter()
            .position(|spec| spec.action == action)
            .unwrap();
        let selected = ui.help_selected;
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert_eq!(ui.view, view);
        assert_eq!(ui.full_history, action == Action::History);
        assert_eq!((ui.draft.text(), ui.draft.cursor()), ("primary draft", 3));
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert_eq!(ui.view, View::Help);
        assert_eq!(ui.help_selected, selected);
        while let Ok(work) = rx.try_recv() {
            assert!(matches!(
                work.command,
                Command::Drafts | Command::Transcript { .. } | Command::Actions { .. }
            ));
        }
    }
}

#[test]
fn interactive_help_argument_actions_do_not_run_and_alt_enter_only_inserts() {
    let snapshot = fixture_snapshot();
    let (tx, mut rx) = mpsc::channel(8);
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("primary draft");
    ui.draft.set_cursor(4);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    ui.help_query.set_text("/send");
    ui.help_selected = ui
        .help_commands()
        .iter()
        .position(|spec| spec.action == Action::Send)
        .unwrap();
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    assert!(
        ui.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &snapshot,
            &tx,
            100,
            20
        )
        .is_err()
    );
    assert_eq!(ui.help_detail.unwrap().action, Action::Send);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(ui.help_detail.is_none());
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        &snapshot,
        &tx,
        100,
        20,
    )
    .unwrap();
    assert_eq!(ui.palette_query.text(), "/send ");
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("primary draft", 4));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn settings_panel_save_ack_does_not_release_an_unrelated_command_chain() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let dir = tempfile::tempdir().unwrap();
    ui.preferences_path = dir.path().join("preferences.toml");
    let (tx, mut rx) = mpsc::channel(8);
    ui.start_chain(vec!["/stop".into(), "/settings status minimal".into()])
        .unwrap();
    ui.advance_chain(&snapshot, &tx);
    let stop = rx.try_recv().unwrap();
    assert!(matches!(
        stop.command,
        Command::StopPending {
            expected_attempt_id: None,
            expected_paused: false,
        }
    ));
    assert!(
        matches!(&stop.op, Op::Chain(op) if matches!(op.as_ref(), Op::Stop { clear_draft: false }))
    );
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    ui.settings_selected = 4;
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    ui.finish_pending_defaults().await.unwrap();
    ui.advance_chain(&snapshot, &tx);
    assert_eq!(ui.interface.status_preset, StatusPreset::Bars);
    complete(
        &mut ui,
        stop.op,
        Ok(Reply::Stopped {
            paused: true,
            cancelled: false,
        }),
    );
    ui.advance_chain(&snapshot, &tx);
    assert_eq!(ui.interface.status_preset, StatusPreset::Minimal);
    assert!(rx.try_recv().is_err());
}

#[test]
fn inline_panel_renderers_use_available_rows_and_fx_selection_without_markers() {
    use ratatui::layout::Rect;
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    for view in [View::Settings, View::Help] {
        ui.view = view;
        for (width, height) in [(100, 14), (32, 5), (1, 1)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let title = Rect::new(0, 0, width, 1);
                    let body = Rect::new(0, 1, width, height.saturating_sub(1));
                    if view == View::Settings {
                        panels::render_settings(frame, &ui, title, body);
                    } else {
                        panels::render_help(frame, &ui, title, body);
                    }
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!(!plain(buffer).contains('>'));
            if width == 100 {
                assert!(plain(buffer).contains(if view == View::Settings {
                    "4 UI preferences"
                } else {
                    "commands"
                }));
                assert!(buffer[(2, 1)].modifier.contains(Modifier::BOLD));
                assert_eq!(buffer[(2, 1)].fg, Color::Indexed(255));
            }
        }
    }
}

#[tokio::test]
async fn settings_panel_control_modified_keys_do_not_apply_or_start_default_saves() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let dir = tempfile::tempdir().unwrap();
    ui.preferences_path = dir.path().join("preferences.toml");
    ui.draft.set_text("protected primary");
    ui.draft.set_cursor(4);
    let (tx, mut rx) = mpsc::channel(8);
    ui.execute(Action::Settings, "", false, &snapshot, &tx)
        .unwrap();
    let before = ui.interface.clone();
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.settings_editor, Some(1));
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.interface, before);
    assert_eq!(ui.settings_editor, Some(1));
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    ui.key(
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.interface, before);
    ui.settings_selected = 4;
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert!(ui.defaults_task.is_none() && !ui.saving_defaults);
    assert!(!ui.preferences_path.exists());
    ui.settings_selected = 5;
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.view, View::Settings);
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("protected primary", 4)
    );
    assert!(ui.inputs.is_empty() && !ui.draft_busy);
    assert!(rx.try_recv().is_err());
}

#[test]
fn interactive_help_control_enter_does_not_open_insert_or_run() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.draft.set_text("protected primary");
    ui.draft.set_cursor(4);
    let (tx, mut rx) = mpsc::channel(8);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    type_text(&mut ui, "settings", &snapshot, &tx);
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.view, View::Help);
    assert!(ui.help_detail.is_none());
    ui.help_query.set_text("stop");
    ui.help_selected = ui
        .help_commands()
        .iter()
        .position(|spec| spec.action == Action::Stop)
        .unwrap();
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.help_detail_selected, 1);
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.view, View::Help);
    assert!(!ui.palette && ui.palette_query.text().is_empty());
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    ui.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &snapshot,
        &tx,
        110,
        20,
    )
    .unwrap();
    assert_eq!(ui.help_detail.unwrap().action, Action::Stop);
    assert!(!ui.stop_pending);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("protected primary", 4)
    );
    assert!(ui.inputs.is_empty());
}

#[test]
fn help_clear_shortcut_targets_the_visible_filter_not_retained_editors() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, mut rx) = mpsc::channel(8);
    ui.draft.set_text("protected primary draft");
    ui.draft.set_cursor(4);
    ui.palette = true;
    ui.palette_pinned = true;
    ui.palette_query.set_text("retained command");
    ui.palette_query.set_cursor(3);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    ui.help_query.set_text("settings");
    ui.help_selected = 2;
    let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    let action = ui
        .keys
        .compile()
        .unwrap()
        .into_iter()
        .find(|(chord, _)| chord.matches(key))
        .unwrap()
        .1;
    assert_eq!(action, Action::ClearDraft);
    let retained_command = (
        ui.palette_query.text().to_owned(),
        ui.palette_query.cursor(),
    );
    ui.shortcut(action, &snapshot, &tx).unwrap();
    assert!(ui.help_query.text().is_empty());
    assert_eq!(ui.help_selected, 0);
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("protected primary draft", 4)
    );
    assert_eq!(
        (
            ui.palette_query.text().to_owned(),
            ui.palette_query.cursor()
        ),
        retained_command
    );
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("retained command", 3)
    );
    ui.shortcut(Action::ClearDraft, &snapshot, &tx).unwrap();
    assert!(ui.palette_query.text().is_empty());
    assert_eq!(
        (ui.draft.text(), ui.draft.cursor()),
        ("protected primary draft", 4)
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn history_toggle_returns_to_its_help_parent_without_leaving_a_stale_escape_target() {
    let snapshot = fixture_snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    let (tx, _rx) = mpsc::channel(8);
    ui.draft.set_text("protected draft");
    ui.draft.set_cursor(4);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    ui.help_query.set_text("history");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::History);
    assert!(ui.full_history);
    ui.shortcut(Action::History, &snapshot, &tx).unwrap();
    assert_eq!(ui.view, View::Help);
    assert!(!ui.full_history);
    assert_eq!(ui.help_query.text(), "history");
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("protected draft", 4));
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
}

#[test]
fn panel_bindings_editor_escape_returns_locally_before_restoring_help() {
    let snapshot = fixture_snapshot();
    let mut config = Config::default();
    config.keys.macros.push(crate::commands::CommandBinding {
        key: "f8".into(),
        commands: vec!["/drafts".into()],
    });
    let mut ui = Ui::new(&config, &snapshot);
    ui.draft.set_text("leased primary");
    ui.draft.set_cursor(4);
    ui.edit = Some(("lease".into(), 7));
    ui.palette = true;
    ui.palette_pinned = true;
    ui.commands_focus = false;
    ui.palette_query.set_text("retained secondary");
    ui.palette_query.set_cursor(5);
    let (tx, mut rx) = mpsc::channel(8);
    ui.execute(Action::Help, "", false, &snapshot, &tx).unwrap();
    type_text(&mut ui, "settings", &snapshot, &tx);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Settings);
    for _ in 0..5 {
        press(&mut ui, KeyCode::Down, &snapshot, &tx);
    }
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Bindings);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.binding_key.as_deref(), Some("f8"));
    ui.binding_commands.set_text("/s");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(ui.completion_active);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.binding_commands.text(), "/s");
    assert!(ui.binding_key.is_some() && !ui.completion_active);
    assert_eq!(ui.view, View::Bindings);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(ui.binding_key.is_none());
    assert_eq!(ui.view, View::Bindings);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Settings);
    assert_eq!(ui.settings_selected, 5);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Help);
    assert_eq!(ui.help_query.text(), "settings");
    assert_eq!(
        ui.help_commands()[ui.help_selected].action,
        Action::Settings
    );
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert_eq!((ui.draft.text(), ui.draft.cursor()), ("leased primary", 4));
    assert_eq!(
        (ui.palette_query.text(), ui.palette_query.cursor()),
        ("retained secondary", 5)
    );
    assert!(ui.palette && ui.palette_pinned && !ui.commands_focus);
    assert_eq!(ui.edit, Some(("lease".into(), 7)));
    assert_eq!(ui.keys.macros, config.keys.macros);
    assert!(ui.bindings_task.is_none() && ui.defaults_task.is_none() && !ui.draft_busy);
    assert!(rx.try_recv().is_err());
}

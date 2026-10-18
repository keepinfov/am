use super::{
    history::{
        Load,
        browser::{Focus, Selection},
    },
    *,
};
use crate::{
    config::Config,
    runtime::{HistoryEntry, HistoryMatch, Phase, Reply, Snapshot},
};

fn fixture() -> (Ui, Snapshot, mpsc::Sender<Work>, mpsc::Receiver<Work>) {
    let now = Instant::now();
    let snapshot = Snapshot {
        session_id: "session".into(),
        agent_id: "agent".into(),
        conversation_id: "conversation".into(),
        phase: Phase::Idle,
        phase_started: now,
        last_event: now,
        active: None,
        queue: Vec::new(),
        paused: false,
        recovered: false,
        cursor: 0,
        error: None,
    };
    let ui = Ui::new(&Config::default(), &snapshot);
    let (tx, rx) = mpsc::channel(OUTSTANDING);
    (ui, snapshot, tx, rx)
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

fn complete(ui: &mut Ui, op: Op, result: anyhow::Result<Reply>) {
    assert!(!ui.completed_with(Done { op, result }, |_| Ok(())).unwrap());
}

fn entry(seq: i64) -> HistoryEntry {
    HistoryEntry {
        seq,
        attempt_id: "saved-attempt".into(),
        kind: "user".into(),
        text: format!("saved message {seq}"),
        status: "accepted".into(),
    }
}

fn matches() -> Reply {
    Reply::HistoryMatches(vec![HistoryMatch {
        seq: 42,
        kind: "user".into(),
        preview: "saved needle".into(),
    }])
}

#[test]
fn async_history_tab_without_initial_items_inserts_but_enter_only_accepts() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.draft.set_text("/history needle");
    assert!(ui.completion(&snapshot).unwrap().items.is_empty());
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(ui.history_search_busy.is_some());
    assert!(!ui.draft_busy);
    let work = rx.try_recv().unwrap();
    assert!(
        matches!(&work.command, Command::SearchHistory { query, limit: 32 } if query == "needle")
    );
    assert!(rx.try_recv().is_err());
    complete(&mut ui, work.op, Ok(matches()));
    assert!(ui.history_search_busy.is_none());
    assert_eq!(ui.draft.text(), "/history 42");
    assert!(ui.completion_active);
    assert_eq!(ui.view, View::Root);
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(!ui.completion_active);
    assert_eq!(ui.view, View::Root);
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let anchor = rx.try_recv().unwrap();
    assert!(matches!(
        anchor.command,
        Command::TranscriptAnchor { seq: 42, limit: 8 }
    ));
    assert!(matches!(
        rx.try_recv().unwrap().command,
        Command::Actions { .. }
    ));
    assert_eq!(ui.view, View::History);
    assert!(ui.full_history);
    complete(
        &mut ui,
        anchor.op,
        Ok(Reply::HistoryAnchor {
            seq: 42,
            entries: vec![entry(42)],
        }),
    );
    assert_eq!(
        ui.history_browser.selection(),
        Some(&Selection::Message(42))
    );
    assert!(!ui.history.follows_tail());
}

#[test]
fn deferred_history_results_do_not_reopen_after_escape_caret_or_text_changes() {
    for change in [
        KeyCode::Esc,
        KeyCode::Left,
        KeyCode::Char('x'),
        KeyCode::Enter,
    ] {
        let (mut ui, snapshot, tx, mut rx) = fixture();
        ui.draft.set_text("/history needle");
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        let pending = rx.try_recv().unwrap();
        press(&mut ui, change, &snapshot, &tx);
        // Returning to identical text/caret must not revive the cancelled request.
        if change == KeyCode::Left {
            press(&mut ui, KeyCode::Right, &snapshot, &tx);
        }
        if change == KeyCode::Char('x') {
            press(&mut ui, KeyCode::Backspace, &snapshot, &tx);
        }
        let text = ui.draft.text().to_owned();
        let cursor = ui.draft.cursor();
        complete(&mut ui, pending.op, Ok(matches()));
        assert_eq!(ui.draft.text(), text);
        assert_eq!(ui.draft.cursor(), cursor);
        assert!(!ui.completion_active && ui.history_search_busy.is_none());
        assert_eq!(ui.view, View::Root);
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn stale_history_reply_cannot_clear_or_replace_a_new_request_for_the_same_query() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.draft.set_text("/history needle");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let old = rx.try_recv().unwrap();
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let new = rx.try_recv().unwrap();
    let busy = ui.history_search_busy;
    complete(&mut ui, old.op, Ok(matches()));
    assert_eq!(ui.history_search_busy, busy);
    assert_eq!(ui.draft.text(), "/history needle");
    assert!(!ui.completion_active);
    complete(&mut ui, new.op, Ok(matches()));
    assert_eq!(ui.draft.text(), "/history 42");
    assert!(ui.completion_active && ui.history_search_busy.is_none());
}

#[test]
fn failed_or_unexpected_history_search_reply_clears_busy_and_allows_retry() {
    for result in [Err(anyhow::anyhow!("fixture search fault")), Ok(Reply::Ack)] {
        let (mut ui, snapshot, tx, mut rx) = fixture();
        ui.draft.set_text("/history needle");
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        let work = rx.try_recv().unwrap();
        complete(&mut ui, work.op, result);
        assert!(ui.history_search_busy.is_none() && !ui.completion_active);
        assert_eq!(ui.draft.text(), "/history needle");
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        assert!(matches!(
            rx.try_recv().unwrap().command,
            Command::SearchHistory { .. }
        ));
        assert!(ui.history_search_busy.is_some());
    }
}

#[test]
fn history_search_attaches_using_palette_and_binding_editor_ranges_without_executing() {
    for binding in [false, true] {
        let (mut ui, snapshot, tx, mut rx) = fixture();
        ui.draft.set_text("protected composer");
        if binding {
            ui.view = View::Bindings;
            ui.binding_key = Some("f9".into());
            ui.binding_commands
                .set_text("/help ; /history \u{00e4}pfel old ; /queue pause");
            let cursor = ui.binding_commands.text().find(" ; /queue").unwrap();
            ui.binding_commands.set_cursor(cursor);
        } else {
            ui.palette = true;
            ui.palette_query.set_text("history \u{00e4}pfel old");
        }
        press(&mut ui, KeyCode::Tab, &snapshot, &tx);
        let work = rx.try_recv().unwrap();
        assert!(
            matches!(&work.command, Command::SearchHistory { query, .. } if query == "\u{00e4}pfel old")
        );
        complete(&mut ui, work.op, Ok(matches()));
        assert!(ui.completion_active);
        if binding {
            assert_eq!(
                ui.binding_commands.text(),
                "/help ; /history 42 ; /queue pause"
            );
        } else {
            assert_eq!(ui.palette_query.text(), "history 42");
        }
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        assert!(!ui.completion_active);
        assert!(ui.keys.macros.is_empty() && rx.try_recv().is_err());
        assert_eq!(ui.draft.text(), "protected composer");
    }
}

#[test]
fn history_search_context_switch_and_direct_caret_change_discard_pending_results() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.palette = true;
    ui.palette_pinned = true;
    ui.palette_query.set_text("history needle");
    ui.draft.set_text("protected");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    ui.execute(Action::FocusInput, "", false, &snapshot, &tx)
        .unwrap();
    ui.execute(Action::FocusInput, "", false, &snapshot, &tx)
        .unwrap();
    complete(&mut ui, work.op, Ok(matches()));
    assert_eq!(ui.palette_query.text(), "history needle");
    assert!(!ui.completion_active);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    ui.palette_query.set_cursor(9);
    complete(&mut ui, work.op, Ok(matches()));
    assert_eq!(ui.palette_query.text(), "history needle");
    assert!(!ui.completion_active && ui.history_search_busy.is_none());
}

#[test]
fn narrow_history_enter_escape_returns_to_list_before_leaving_fullscreen() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.execute(Action::History, "", false, &snapshot, &tx)
        .unwrap();
    let page = rx.try_recv().unwrap();
    complete(
        &mut ui,
        page.op,
        Ok(Reply::History(vec![entry(1), entry(2)])),
    );
    let actions = rx.try_recv().unwrap();
    complete(&mut ui, actions.op, Ok(Reply::Actions(vec![])));
    let key = |ui: &mut Ui, code| {
        ui.key(
            KeyEvent::new(code, KeyModifiers::NONE),
            &snapshot,
            &tx,
            60,
            12,
        )
        .unwrap()
    };
    key(&mut ui, KeyCode::Enter);
    assert_eq!(ui.history_browser.focus, Focus::Preview);
    key(&mut ui, KeyCode::Esc);
    assert_eq!(ui.history_browser.focus, Focus::List);
    assert_eq!(ui.view, View::History);
    assert!(ui.full_history);
    key(&mut ui, KeyCode::Tab);
    assert_eq!(ui.history_browser.focus, Focus::Preview);
    key(&mut ui, KeyCode::Esc);
    key(&mut ui, KeyCode::Esc);
    assert_eq!(ui.view, View::Root);
    assert!(!ui.full_history && rx.try_recv().is_err());
}

#[test]
fn history_anchor_failures_invalid_replies_and_late_receipts_do_not_select_fake_entries() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.execute(Action::History, "42", false, &snapshot, &tx)
        .unwrap();
    let anchor = rx.try_recv().unwrap();
    let actions = rx.try_recv().unwrap();
    complete(&mut ui, actions.op, Ok(Reply::Actions(vec![])));
    complete(
        &mut ui,
        anchor.op,
        Err(anyhow::anyhow!("saved history entry 42 does not exist")),
    );
    assert!(ui.history_browser.selection().is_none());
    assert!(!ui.history_busy);
    ui.history_request(&tx, Load::Anchor(42));
    let anchor = rx.try_recv().unwrap();
    complete(
        &mut ui,
        anchor.op,
        Ok(Reply::HistoryAnchor {
            seq: 43,
            entries: vec![entry(43)],
        }),
    );
    assert!(ui.history_browser.selection().is_none());
    ui.history_request(&tx, Load::Anchor(42));
    let anchor = rx.try_recv().unwrap();
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    complete(
        &mut ui,
        anchor.op,
        Ok(Reply::HistoryAnchor {
            seq: 42,
            entries: vec![entry(42)],
        }),
    );
    assert_eq!(ui.view, View::Root);
    assert!(!ui.full_history && ui.history_browser.selection().is_none());
}

#[test]
fn history_page_navigation_and_chronological_scroll_keep_their_distinct_dimensions() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.view = View::History;
    ui.full_history = true;
    complete(
        &mut ui,
        Op::History(Load::Latest, false),
        Ok(Reply::History(vec![entry(9)])),
    );
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    let older = rx.try_recv().unwrap();
    assert!(matches!(
        older.command,
        Command::Transcript {
            before: Some(9),
            ..
        }
    ));
    complete(&mut ui, older.op, Ok(Reply::History(vec![entry(8)])));
    assert_eq!(ui.history_browser.selection(), Some(&Selection::Message(8)));
    assert_eq!(
        history::browser::HistoryBrowser::preview_dimensions(120, render::history_height(20, true)),
        (79, 16)
    );
    press(&mut ui, KeyCode::Char('c'), &snapshot, &tx);
    assert!(ui.history_browser.chronological);
    assert_eq!(render::history_height(20, true).saturating_sub(1), 16);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert!(!ui.history_browser.chronological && ui.full_history);
}

#[test]
fn async_history_multiple_matches_cycle_and_accept_without_execution() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.draft.set_text("/history needle");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    complete(
        &mut ui,
        work.op,
        Ok(Reply::HistoryMatches(vec![
            HistoryMatch {
                seq: 42,
                kind: "user".into(),
                preview: "needle one".into(),
            },
            HistoryMatch {
                seq: 43,
                kind: "assistant".into(),
                preview: "needle two".into(),
            },
        ])),
    );
    assert_eq!(ui.draft.text(), "/history needle");
    assert!(ui.completion_active);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/history 42");
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/history 43");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert!(!ui.completion_active && rx.try_recv().is_err());
}

#[test]
fn history_search_cache_is_invalidated_by_new_durable_events() {
    let (mut ui, mut snapshot, tx, mut rx) = fixture();
    ui.draft.set_text("/history needle ");
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    complete(&mut ui, work.op, Ok(matches()));
    assert_eq!(ui.draft.text(), "/history 42 ");
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.draft.text(), "/history needle ");
    assert_eq!(ui.completion(&snapshot).unwrap().items[0].value, "42");
    snapshot.cursor += 1;
    assert!(ui.completion(&snapshot).unwrap().items.is_empty());
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    assert!(matches!(
        rx.try_recv().unwrap().command,
        Command::SearchHistory { .. }
    ));
}

#[test]
fn selected_message_preview_is_not_evicted_by_an_in_flight_tail_refresh() {
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.view = View::History;
    ui.full_history = true;
    complete(
        &mut ui,
        Op::History(Load::Latest, false),
        Ok(Reply::History(vec![entry(9), entry(10)])),
    );
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert!(!ui.history.follows_tail());
    complete(
        &mut ui,
        Op::History(Load::Latest, true),
        Ok(Reply::History(vec![entry(11), entry(12)])),
    );
    assert_eq!(
        ui.history_browser.selection(),
        Some(&Selection::Message(10))
    );
    assert!(ui.history.anchor(9));
    press(&mut ui, KeyCode::Char('c'), &snapshot, &tx);
    assert!(ui.history_browser.chronological);
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    assert_eq!(ui.history_browser.focus, Focus::List);
    assert!(ui.full_history && !ui.history_browser.chronological);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn deferred_history_completion_reads_old_durable_matches_and_opens_the_saved_anchor() {
    use crate::{
        runtime::Client,
        storage::{Completion, Store},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let config = Config::default();
    let mut store = Store::open(&path, &config).unwrap();
    for index in 0..24 {
        store
            .apply(
                &format!("seed-{index}"),
                &Command::Send {
                    text: if index == 0 {
                        "old \u{00c4}PFEL saved message".into()
                    } else {
                        format!("recent prompt {index}")
                    },
                },
                &config,
                None,
            )
            .unwrap();
        let attempt = store.start_next(&config).unwrap().unwrap();
        store
            .finish(Completion {
                attempt_id: &attempt.attempt_id,
                text: "durable response",
                status: "completed",
            })
            .unwrap();
    }
    let cached = store.transcript(None, None, 16).unwrap();
    assert!(cached.iter().all(|entry| entry.seq > 1));
    drop(store);
    let client = Client::start(&path, config.clone()).await.unwrap();
    let snapshot = client.snapshot();
    let mut ui = Ui::new(&config, &snapshot);
    ui.history.apply(cached, Load::Latest);
    ui.draft.set_text("/history \u{00e4}pfel");
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    press(&mut ui, KeyCode::Tab, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    let result = client.command(work.command).await;
    complete(&mut ui, work.op, result);
    assert_eq!(ui.draft.text(), "/history 1");
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    assert_eq!(ui.view, View::Root);
    assert!(rx.try_recv().is_err());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let work = rx.try_recv().unwrap();
    let result = client.command(work.command).await;
    complete(&mut ui, work.op, result);
    assert_eq!(ui.history_browser.selection(), Some(&Selection::Message(1)));
    assert!(ui.full_history);
    ui.prepare_history(110, 20);
    assert!(ui.history.lines(80, 1)[0].to_string().contains("old"));
    client.shutdown().await.unwrap();
}

#[test]
fn browser_artifact_navigation_uses_byte_pages_and_rejects_old_selection_receipts() {
    use crate::runtime::{ActionRecord, ArtifactChunk, ArtifactRecord};
    let (mut ui, snapshot, tx, mut rx) = fixture();
    ui.view = View::History;
    ui.full_history = true;
    ui.journal.append_actions(
        (1..=2)
            .map(|seq| ActionRecord {
                seq,
                id: format!("action-{seq}"),
                group_id: "group".into(),
                category: "tool".into(),
                label: "output".into(),
                status: "done".into(),
                output: Some(ArtifactRecord {
                    id: format!("artifact-{seq}"),
                    path: "output.txt".into(),
                    bytes: 30_000,
                    incomplete: false,
                }),
            })
            .collect(),
    );
    ui.prepare_history(110, 20);
    ui.poll(&tx);
    let mut old_artifact = None;
    while let Ok(work) = rx.try_recv() {
        match work.command {
            Command::ReadArtifact { .. } => old_artifact = Some(work.op),
            Command::ActionsAfter { .. } => complete(&mut ui, work.op, Ok(Reply::Actions(vec![]))),
            Command::Transcript { .. } => complete(&mut ui, work.op, Ok(Reply::History(vec![]))),
            Command::Events { .. } => complete(&mut ui, work.op, Ok(Reply::Events(vec![]))),
            command => panic!("unexpected read {command:?}"),
        }
    }
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(
        ui.history_browser.selection(),
        Some(&Selection::Action("action-2".into()))
    );
    let chunk = |id: &str, text: &str, next_offset| ArtifactChunk {
        id: id.into(),
        text: text.into(),
        next_offset,
        eof: false,
        path: "output.txt".into(),
        incomplete: false,
    };
    complete(
        &mut ui,
        old_artifact.unwrap(),
        Ok(Reply::Artifact(chunk("artifact-1", "stale", 5))),
    );
    assert!(ui.journal.preview_text.is_empty());
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    let read = rx.try_recv().unwrap();
    assert!(
        matches!(read.command, Command::ReadArtifact { ref id, offset: 0, .. } if id == "artifact-2")
    );
    complete(
        &mut ui,
        read.op,
        Ok(Reply::Artifact(chunk("artifact-2", "first page", 8192))),
    );
    press(&mut ui, KeyCode::PageDown, &snapshot, &tx);
    let next = rx.try_recv().unwrap();
    assert!(matches!(
        next.command,
        Command::ReadArtifact { offset: 8192, .. }
    ));
    complete(
        &mut ui,
        next.op,
        Ok(Reply::Artifact(chunk("artifact-2", "second page", 16_384))),
    );
    press(&mut ui, KeyCode::PageUp, &snapshot, &tx);
    let previous = rx.try_recv().unwrap();
    assert!(matches!(
        previous.command,
        Command::ReadArtifactBefore { before: 8192, .. }
    ));
    complete(
        &mut ui,
        previous.op,
        Ok(Reply::ArtifactPage {
            offset: 0,
            chunk: chunk("artifact-2", "first page", 8192),
        }),
    );
    assert_eq!(ui.journal.preview_text, "first page");
    press(&mut ui, KeyCode::Char(' '), &snapshot, &tx);
    assert!(ui.journal.collapsed);
}

async fn drain_history_reads(
    ui: &mut Ui,
    rx: &mut mpsc::Receiver<Work>,
    client: &crate::runtime::Client,
) -> Vec<Command> {
    let mut commands = Vec::new();
    while let Ok(work) = rx.try_recv() {
        assert!(matches!(
            work.command,
            Command::Actions { .. }
                | Command::ActionsAfter { .. }
                | Command::Transcript { .. }
                | Command::Events { .. }
                | Command::ReadArtifact { .. }
                | Command::ReadArtifactBefore { .. }
        ));
        commands.push(work.command.clone());
        complete(ui, work.op, client.command(work.command).await);
    }
    commands
}

async fn open_saved_history(
    count: usize,
    page: usize,
) -> (
    tempfile::TempDir,
    crate::runtime::Client,
    Ui,
    Snapshot,
    mpsc::Sender<Work>,
    mpsc::Receiver<Work>,
) {
    use crate::storage::{Completion, Store};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let mut config = Config::default();
    config.limits.history_page = page;
    config.provider.first_event_ms = 60_000;
    let mut store = Store::open(&path, &config).unwrap();
    for index in 0..count {
        store
            .apply(
                &format!("seed-{index}"),
                &Command::Send {
                    text: format!("saved prompt {index}"),
                },
                &config,
                None,
            )
            .unwrap();
        let attempt = store.start_next(&config).unwrap().unwrap();
        store
            .finish(Completion {
                attempt_id: &attempt.attempt_id,
                text: "saved artifact output",
                status: "completed",
            })
            .unwrap();
    }
    drop(store);
    let client = crate::runtime::Client::start(&path, config.clone())
        .await
        .unwrap();
    let snapshot = client.snapshot();
    let mut ui = Ui::new(&config, &snapshot);
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.execute(Action::History, "", false, &snapshot, &tx)
        .unwrap();
    drain_history_reads(&mut ui, &mut rx, &client).await;
    ui.prepare_history(110, 20);
    (directory, client, ui, snapshot, tx, rx)
}

#[tokio::test]
async fn background_history_poll_preserves_down_enter_artifact_navigation() {
    for finish_before_enter in [false, true] {
        let (_directory, client, mut ui, snapshot, tx, mut rx) = open_saved_history(1, 8).await;
        assert_eq!(ui.history_browser.selection(), Some(&Selection::Message(2)));
        assert_eq!(ui.history_browser.focus, Focus::List);
        let action = ui.journal.actions[0].clone();
        let artifact = action.output.unwrap();
        ui.poll(&tx);
        let mut background = None;
        while let Ok(work) = rx.try_recv() {
            let result = client.command(work.command).await;
            if matches!(work.op, Op::History(Load::Latest, true)) {
                background = Some((work.op, result));
            } else {
                complete(&mut ui, work.op, result);
            }
        }
        assert!(background.is_some(), "poll must issue a real tail refresh");
        press(&mut ui, KeyCode::Down, &snapshot, &tx);
        assert_eq!(
            ui.history_browser.selection(),
            Some(&Selection::Action(action.id.clone())),
            "background polling must not discard cached list navigation"
        );
        if finish_before_enter {
            let (op, result) = background.take().unwrap();
            complete(&mut ui, op, result);
        }
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        if let Some((op, result)) = background.take() {
            complete(&mut ui, op, result);
        }
        let commands = drain_history_reads(&mut ui, &mut rx, &client).await;
        assert!(commands.iter().any(|command| matches!(
            command, Command::ReadArtifact { id, offset: 0, .. } if id == &artifact.id
        )));
        assert_eq!(
            ui.history_browser.selection(),
            Some(&Selection::Action(action.id))
        );
        assert_eq!(ui.history_browser.focus, Focus::Preview);
        assert_eq!(ui.journal.path, artifact.path);
        assert_eq!(ui.journal.preview_text, "saved artifact output");
        ui.prepare_history(110, 20);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 20)).unwrap();
        terminal
            .draw(|frame| ui.history_browser.render(frame, frame.area(), &ui.journal))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Preview"));
        assert!(ui.pending_notices.is_empty());
        client.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_history_page_during_refresh_is_not_stranded_or_reset() {
    for foreground_first in [false, true] {
        let (_directory, client, mut ui, snapshot, tx, mut rx) = open_saved_history(8, 2).await;
        ui.poll(&tx);
        let mut background = None;
        while let Ok(work) = rx.try_recv() {
            let result = client.command(work.command).await;
            if matches!(work.op, Op::History(Load::Latest, true)) {
                background = Some((work.op, result));
            } else {
                complete(&mut ui, work.op, result);
            }
        }
        let (background, refresh_result) = background.unwrap();
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        assert_eq!(
            ui.history_browser.selection(),
            Some(&Selection::Message(15))
        );
        press(&mut ui, KeyCode::Up, &snapshot, &tx);
        let page = rx.try_recv().expect("navigation must admit the older page");
        assert!(matches!(page.op, Op::History(Load::Older, false)));
        assert!(matches!(
            page.command,
            Command::Transcript {
                before: Some(15),
                after: None,
                limit: 2
            }
        ));
        let page_result = client.command(page.command).await;
        if foreground_first {
            complete(&mut ui, page.op, page_result);
            complete(&mut ui, background, refresh_result);
        } else {
            complete(&mut ui, background, refresh_result);
            assert!(ui.history_busy, "refresh must not unlock a pending page");
            complete(&mut ui, page.op, page_result);
        }
        assert!(!ui.history_busy);
        assert!(!ui.history.follows_tail());
        assert_eq!(
            ui.history_browser.selection(),
            Some(&Selection::Message(14))
        );
        assert_eq!(
            ui.history
                .entries()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            [13, 14, 15, 16]
        );
        press(&mut ui, KeyCode::Down, &snapshot, &tx);
        assert_eq!(
            ui.history_browser.selection(),
            Some(&Selection::Message(15))
        );
        assert!(rx.try_recv().is_err());
        assert!(ui.pending_notices.is_empty());
        client.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn reopening_history_serializes_initial_page_and_preserves_explicit_edges() {
    let (_directory, client, mut ui, snapshot, tx, mut rx) = open_saved_history(1, 8).await;
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    ui.execute(Action::History, "", false, &snapshot, &tx)
        .unwrap();
    let page = rx.try_recv().unwrap();
    assert!(matches!(page.op, Op::History(Load::Latest, false)));
    let result = client.command(page.command).await;
    drain_history_reads(&mut ui, &mut rx, &client).await;
    assert!(ui.history_busy);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    assert_eq!(ui.history_browser.selection(), Some(&Selection::Message(2)));
    complete(&mut ui, page.op, result);
    press(&mut ui, KeyCode::Down, &snapshot, &tx);
    let action = ui.journal.selected_action().unwrap().id.clone();
    assert_eq!(
        ui.history_browser.selection(),
        Some(&Selection::Action(action))
    );
    press(&mut ui, KeyCode::Enter, &snapshot, &tx);
    drain_history_reads(&mut ui, &mut rx, &client).await;
    assert_eq!(ui.history_browser.focus, Focus::Preview);
    assert_eq!(ui.journal.preview_text, "saved artifact output");
    press(&mut ui, KeyCode::Esc, &snapshot, &tx);
    for (key, selected) in [(KeyCode::Home, 1), (KeyCode::End, 2)] {
        press(&mut ui, key, &snapshot, &tx);
        drain_history_reads(&mut ui, &mut rx, &client).await;
        assert_eq!(
            ui.history_browser.selection(),
            Some(&Selection::Message(selected))
        );
    }
    assert!(ui.pending_notices.is_empty());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn superseded_running_history_refresh_is_silent_and_does_not_unlock_a_page() {
    for foreground_first in [false, true] {
        let (_directory, client, mut ui, _, tx, mut rx) = open_saved_history(1, 2).await;
        client
            .command(Command::Send {
                text: "waiting attempt".into(),
            })
            .await
            .unwrap();
        client.command(Command::Resume).await.unwrap();
        let mut updates = client.subscribe();
        let snapshot = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = updates.borrow_and_update().clone();
                if snapshot.active.is_some() {
                    break snapshot;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        ui.history_request(&tx, Load::Latest);
        drain_history_reads(&mut ui, &mut rx, &client).await;
        assert_eq!(ui.history_browser.selection(), Some(&Selection::Message(4)));
        press(&mut ui, KeyCode::Enter, &snapshot, &tx);
        press(&mut ui, KeyCode::Esc, &snapshot, &tx);
        assert!(!ui.history.follows_tail());
        ui.poll(&tx);
        let mut background = None;
        while let Ok(work) = rx.try_recv() {
            let result = client.command(work.command).await;
            if matches!(work.op, Op::HistoryRefresh) {
                background = Some((work.op, result));
            } else {
                complete(&mut ui, work.op, result);
            }
        }
        let (background, refresh_result) =
            background.expect("poll must refresh the cached running entry");
        press(&mut ui, KeyCode::Home, &snapshot, &tx);
        let page = rx.try_recv().expect("Home must admit a foreground page");
        assert!(matches!(page.op, Op::History(Load::Oldest, false)));
        let page_result = client.command(page.command).await;
        if foreground_first {
            complete(&mut ui, page.op, page_result);
            complete(&mut ui, background, refresh_result);
        } else {
            complete(&mut ui, background, refresh_result);
            assert!(ui.history_busy);
            complete(&mut ui, page.op, page_result);
        }
        assert!(!ui.history_busy);
        assert_eq!(ui.history_browser.selection(), Some(&Selection::Message(1)));
        assert_eq!(
            ui.history
                .entries()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(ui.pending_notices.is_empty());
        assert!(
            ui.notice.is_none(),
            "a superseded read is not an unexpected reply"
        );
        client.shutdown().await.unwrap();
    }
}

#[test]
fn history_poll_refreshes_cached_running_action_completion_and_empty_artifact() {
    use crate::storage::{Completion, Store};
    let page = "saved page\n".repeat(20);
    for (initial, status) in [
        ("", "completed"),
        ("", "cancelled"),
        (page.as_str(), "completed"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        let config = Config::default();
        let mut store = Store::open(&path, &config).unwrap();
        store
            .apply(
                "seed",
                &Command::Send {
                    text: "prompt".into(),
                },
                &config,
                None,
            )
            .unwrap();
        let attempt = store.start_next(&config).unwrap().unwrap();
        store
            .append_output(&attempt.attempt_id, initial, &config.limits)
            .unwrap();
        // Route UI-emitted reads to the real Store without synthetic action replies.
        let drain = |ui: &mut Ui, rx: &mut mpsc::Receiver<Work>, store: &Store| {
            let mut commands = Vec::new();
            while let Ok(work) = rx.try_recv() {
                let reply = match &work.command {
                    Command::Actions { before, limit } => Reply::Actions(
                        store
                            .actions(*before, (*limit).clamp(1, config.limits.history_page))
                            .unwrap(),
                    ),
                    Command::ActionsAfter { after, limit } => Reply::Actions(
                        store
                            .actions_after(*after, (*limit).clamp(1, config.limits.history_page))
                            .unwrap(),
                    ),
                    Command::Transcript {
                        before,
                        after,
                        limit,
                    } => Reply::History(
                        store
                            .transcript(
                                *before,
                                *after,
                                (*limit).clamp(1, config.limits.history_page),
                            )
                            .unwrap(),
                    ),
                    Command::Events { after, limit } => {
                        Reply::Events(store.events(*after, *limit).unwrap())
                    }
                    Command::ReadArtifact { id, offset, limit } => {
                        Reply::Artifact(store.read_artifact(id, *offset, *limit).unwrap())
                    }
                    command => panic!("unexpected history read {command:?}"),
                };
                commands.push(work.command);
                complete(ui, work.op, Ok(reply));
            }
            commands
        };
        let (mut ui, snapshot, tx, mut rx) = fixture();
        ui.view = View::History;
        ui.full_history = true;
        for _ in 0..2 {
            ui.poll(&tx);
            drain(&mut ui, &mut rx, &store);
        }
        let cached = ui.journal.selected_action().unwrap().clone();
        assert_eq!(cached.status, "running");
        assert_eq!(cached.output.as_ref().unwrap().bytes, initial.len() as u64);
        assert_eq!(ui.journal.preview_text, initial);
        assert_eq!(ui.journal.next_offset, initial.len() as u64);
        assert!(ui.journal.eof);
        if !initial.is_empty() {
            press(&mut ui, KeyCode::Enter, &snapshot, &tx);
            for _ in 0..3 {
                press(&mut ui, KeyCode::Down, &snapshot, &tx);
            }
            assert_eq!(ui.journal.preview_offset, 3);
            assert!(rx.try_recv().is_err());
        }
        let scroll = ui.journal.preview_offset;
        let suffix = if status == "completed" {
            "new bytes after EOF\n"
        } else {
            ""
        };
        store
            .append_output(&attempt.attempt_id, suffix, &config.limits)
            .unwrap();
        let text = format!("{initial}{suffix}");
        store
            .finish(Completion {
                attempt_id: &attempt.attempt_id,
                text: &text,
                status,
            })
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let counts = || {
            connection
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM history), (SELECT COUNT(*) FROM events), (SELECT COUNT(*) FROM command_results)",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
                )
                .unwrap()
        };
        let before = counts();
        ui.poll(&tx);
        let commands = drain(&mut ui, &mut rx, &store);
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, Command::ReadArtifact { .. }))
        );
        assert_eq!(ui.journal.selected_action().unwrap().status, status);
        assert_eq!(ui.journal.preview_text, initial);
        assert_eq!(ui.journal.preview_offset, scroll);
        assert_eq!(ui.journal.next_offset, initial.len() as u64);
        assert_eq!(ui.journal.eof, suffix.is_empty());
        assert_eq!(ui.journal.incomplete, status != "completed");
        if initial.is_empty() {
            ui.poll(&tx);
        } else {
            press(&mut ui, KeyCode::PageDown, &snapshot, &tx);
        }
        let commands = drain(&mut ui, &mut rx, &store);
        if !suffix.is_empty() {
            assert!(commands.iter().any(|command| matches!(
                command,
                Command::ReadArtifact { id, offset, limit: 8192 }
                    if id == &cached.output.as_ref().unwrap().id && *offset == initial.len() as u64
            )));
        }
        let refreshed = ui.journal.selected_action().unwrap();
        assert_eq!(refreshed.id, cached.id);
        assert_eq!(refreshed.seq, cached.seq);
        assert_eq!(refreshed.status, status);
        assert_eq!(
            refreshed.output.as_ref().unwrap().incomplete,
            status != "completed"
        );
        assert_eq!(ui.journal.incomplete, status != "completed");
        assert!(ui.journal.eof);
        assert_eq!(ui.journal.preview_text, suffix);
        assert_eq!(
            ui.journal.next_offset,
            refreshed.output.as_ref().unwrap().bytes
        );
        assert_eq!(counts(), before, "history polling must be read-only");
        assert!(ui.pending_notices.is_empty());
    }
}

#[tokio::test]
async fn history_poll_page_one_refresh_does_not_starve_action_tail_catchup() {
    use crate::{config::Scenario, runtime::Client};
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.limits.history_page = 1;
    config.provider.scenario = Scenario::Burst;
    config.provider.first_event_ms = 60_000;
    let client = Client::start(directory.path().join("session.sqlite3"), config.clone())
        .await
        .unwrap();
    client
        .command(Command::Send {
            text: "prompt".into(),
        })
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = client.snapshot();
            if snapshot.active.is_some() {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut ui = Ui::new(&config, &snapshot);
    ui.view = View::History;
    ui.full_history = true;
    ui.journal.collapsed = true;
    let (tx, mut rx) = mpsc::channel(OUTSTANDING);
    ui.poll(&tx);
    drain_history_reads(&mut ui, &mut rx, &client).await;
    let cached = ui.journal.selected_action().unwrap().clone();
    for index in 0..24 {
        client
            .command(Command::RecordAction {
                text: format!("intervening action {index}"),
                level: crate::runtime::NoticeLevel::Info,
                category: "queue".into(),
            })
            .await
            .unwrap();
    }
    for index in 1..=25 {
        if index == 5 {
            client
                .command(Command::Interrupt {
                    attempt_id: snapshot.active.as_ref().unwrap().attempt_id.clone(),
                })
                .await
                .unwrap();
        }
        ui.poll(&tx);
        let commands = drain_history_reads(&mut ui, &mut rx, &client).await;
        assert!(commands.iter().any(|command| matches!(
            command, Command::ActionsAfter { after, .. } if *after == cached.seq + (index - 1).min(24)
        )));
        let refreshes = commands.iter().filter(|command| matches!(
            command, Command::Actions { before: Some(before), limit: 1 } if *before == cached.seq + 1
        )).count();
        assert_eq!(refreshes, usize::from(index <= 5));
        assert_eq!(
            ui.journal.actions.last().unwrap().seq,
            cached.seq + index.min(24)
        );
        assert_eq!(ui.journal.actions.len(), 1 + index.min(24) as usize);
        assert_eq!(ui.journal.selected_action().unwrap().id, cached.id);
        assert_eq!(
            ui.journal.selected_action().unwrap().status,
            if index < 5 { "running" } else { "cancelled" }
        );
        assert_eq!(ui.history_browser.actions_newest, index == 25);
    }
    assert!(ui.pending_notices.is_empty());
    client.shutdown().await.unwrap();
}

#[test]
fn history_action_refresh_round_robin_preserves_preview_and_browsing_window() {
    use crate::runtime::{ActionRecord, ArtifactRecord};
    let (mut ui, snapshot, tx, mut rx) = fixture();
    let action = |seq| ActionRecord {
        seq,
        id: format!("action-{seq}"),
        group_id: "group".into(),
        category: "fixture".into(),
        label: "output".into(),
        status: if [101, 150, 250].contains(&seq) {
            "running"
        } else {
            "completed"
        }
        .into(),
        output: (seq == 101).then(|| ArtifactRecord {
            id: "artifact-101".into(),
            path: "old-path".into(),
            bytes: 600,
            incomplete: true,
        }),
    };
    ui.view = View::History;
    ui.full_history = true;
    ui.journal.append_actions((101..=300).map(action).collect());
    ui.prepare_history(110, 20);
    ui.journal.preview_text = "saved page\n".repeat(20);
    ui.journal.rewrap(70);
    ui.journal.preview_offset = 3;
    ui.journal.next_offset = 600;
    ui.journal.eof = true;
    for expected in [101, 150, 250, 101] {
        ui.poll(&tx);
        let mut refreshes = 0;
        while let Ok(work) = rx.try_recv() {
            let reply = match work.op {
                Op::ActionRefresh { ref id, seq } => {
                    refreshes += 1;
                    assert_eq!(seq, expected);
                    assert_eq!(id, &format!("action-{expected}"));
                    assert!(
                        matches!(work.command, Command::Actions { before: Some(before), limit: 1 } if before == seq + 1)
                    );
                    let mut updated = action(seq);
                    if let Some(output) = &mut updated.output {
                        output.path = "new-path".into();
                        output.incomplete = false;
                    }
                    Reply::Actions(vec![updated])
                }
                Op::Actions { .. } => Reply::Actions(vec![]),
                Op::History(_, _) => Reply::History(vec![]),
                Op::Events => Reply::Events(vec![]),
                _ => panic!("unexpected history read"),
            };
            complete(&mut ui, work.op, Ok(reply));
        }
        assert_eq!(refreshes, 1);
        assert_eq!(ui.journal.preview_offset, 3);
        assert_eq!(ui.journal.next_offset, 600);
        assert_eq!(ui.journal.preview_text, "saved page\n".repeat(20));
        assert!(ui.journal.eof, "unchanged byte count must not reopen EOF");
        assert_eq!(ui.journal.path, "new-path");
        assert!(!ui.journal.incomplete);
    }
    press(&mut ui, KeyCode::Up, &snapshot, &tx);
    let older = rx.try_recv().unwrap();
    assert!(matches!(
        older.command,
        Command::Actions {
            before: Some(101),
            ..
        }
    ));
    let mut delayed = None;
    for expected in [150, 250] {
        ui.poll(&tx);
        while let Ok(work) = rx.try_recv() {
            match work.op {
                Op::ActionRefresh { seq, .. } => {
                    assert_eq!(seq, expected);
                    if seq == 250 {
                        delayed = Some(work.op);
                    } else {
                        complete(&mut ui, work.op, Ok(Reply::Actions(vec![action(seq)])));
                    }
                }
                Op::History(_, _) => complete(&mut ui, work.op, Ok(Reply::History(vec![]))),
                Op::Events => complete(&mut ui, work.op, Ok(Reply::Events(vec![]))),
                _ => panic!("browsing must not request the action tail"),
            }
        }
        assert_eq!(ui.journal.selected_action().unwrap().seq, 101);
    }
    complete(
        &mut ui,
        older.op,
        Ok(Reply::Actions((1..=100).map(action).collect())),
    );
    assert_eq!(
        ui.journal.selected_action().unwrap().seq,
        100,
        "refresh must not consume pending navigation"
    );
    assert_eq!(ui.journal.actions.len(), 200);
    let mut completed = action(250);
    completed.status = "completed".into();
    complete(
        &mut ui,
        delayed.unwrap(),
        Ok(Reply::Actions(vec![completed])),
    );
    assert_eq!(ui.journal.actions.first().unwrap().seq, 1);
    assert_eq!(ui.journal.actions.last().unwrap().seq, 200);
    assert_eq!(ui.journal.selected_action().unwrap().seq, 100);
    assert_eq!(
        ui.history_browser.selection(),
        Some(&Selection::Action("action-100".into()))
    );
    assert!(ui.pending_notices.is_empty());
}

#[test]
fn history_action_refresh_failures_and_mismatched_replies_are_read_only_and_retry() {
    use crate::runtime::ActionRecord;
    let (mut ui, _, tx, mut rx) = fixture();
    ui.view = View::History;
    ui.full_history = true;
    let action = |seq| ActionRecord {
        seq,
        id: format!("action-{seq}"),
        group_id: "group".into(),
        category: "fixture".into(),
        label: "output".into(),
        status: "running".into(),
        output: None,
    };
    ui.journal.append_actions(vec![action(1), action(2)]);
    for (index, result) in [
        Err(anyhow::anyhow!("metadata read failed")),
        Ok(Reply::Ack),
        Ok(Reply::Actions(vec![action(2)])),
        Ok(Reply::Actions(vec![ActionRecord {
            seq: 99,
            ..action(2)
        }])),
    ]
    .into_iter()
    .enumerate()
    {
        ui.poll(&tx);
        let mut refresh = None;
        while let Ok(work) = rx.try_recv() {
            let reply = match work.op {
                Op::ActionRefresh { seq, .. } => {
                    assert_eq!(seq, 1 + index as i64 % 2);
                    refresh = Some(work.op);
                    continue;
                }
                Op::Actions { .. } => Reply::Actions(vec![]),
                Op::History(_, _) => Reply::History(vec![]),
                Op::Events => Reply::Events(vec![]),
                _ => panic!("unexpected history read"),
            };
            complete(&mut ui, work.op, Ok(reply));
        }
        complete(&mut ui, refresh.unwrap(), result);
        assert_eq!(ui.outstanding, 0);
        assert_eq!(ui.journal.actions.len(), 2);
        assert!(
            ui.journal
                .actions
                .iter()
                .all(|action| action.status == "running")
        );
        assert!(ui.pending_notices.is_empty());
        assert!(ui.notice.is_none());
    }
}

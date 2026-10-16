use std::time::Instant;

use tokio::sync::mpsc;

use super::state::{Done, Ui, View};
use crate::{
    config::Config,
    runtime::{Command, HistoryEntry, NativeEntry, Phase, Reply, Snapshot},
};

fn snapshot() -> Snapshot {
    let now = Instant::now();
    Snapshot {
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
    }
}

fn entry(seq: i64, kind: &str, text: &str) -> HistoryEntry {
    HistoryEntry {
        seq,
        attempt_id: if kind == "notice" {
            String::new()
        } else {
            "attempt".into()
        },
        kind: kind.into(),
        text: text.into(),
        status: if kind == "notice" {
            "info".into()
        } else {
            "completed".into()
        },
    }
}

fn publish_all(ui: &mut Ui, snapshot: &Snapshot) -> String {
    let mut rows = Vec::new();
    for _ in 0..8 {
        ui.publish_with(snapshot, 100, |batch| {
            rows.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
    }
    ui.exiting = true;
    ui.publish_with(snapshot, 100, |batch| {
        rows.extend(batch.iter().map(ToString::to_string));
        Ok(())
    })
    .unwrap();
    rows.join("\n")
}

#[test]
fn later_receipt_does_not_bypass_completed_messages_or_older_typed_notice() {
    let snapshot = snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.published = Some(0);
    ui.pending.insert(1, entry(1, "user", "older user input"));
    ui.pending
        .insert(2, entry(2, "assistant", "older completed response"));
    ui.pending
        .insert(3, entry(3, "notice", "older queue outcome"));
    ui.action_categories.insert(3, "queue");
    ui.live_notices
        .push_back(entry(4, "notice", "later settings outcome"));
    ui.action_categories.insert(4, "settings");
    let rendered = publish_all(&mut ui, &snapshot);
    let user = rendered.find("older user input").unwrap();
    let response = rendered.find("older completed response").unwrap();
    let queue = rendered.find("older queue outcome").unwrap();
    let settings = rendered.find("later settings outcome").unwrap();
    assert!(
        user < response && response < queue && queue < settings,
        "{rendered}"
    );
    assert_eq!(rendered.matches("older queue outcome").count(), 1);
    assert_eq!(rendered.matches("later settings outcome").count(), 1);
    assert_eq!(rendered.matches("\u{25cf} Queue").count(), 1);
    assert_eq!(rendered.matches("\u{25cf} Settings").count(), 1);
}

#[test]
fn fullscreen_return_waits_for_unloaded_native_backlog_before_later_receipt() {
    let snapshot = snapshot();
    let mut ui = Ui::new(&Config::default(), &snapshot);
    ui.published = Some(0);
    ui.view = View::History;
    ui.full_history = true;
    ui.observe_live(&snapshot);
    ui.live_notices
        .push_back(entry(3, "notice", "later settings outcome"));
    ui.action_categories.insert(3, "settings");
    ui.view = View::Root;
    ui.full_history = false;
    ui.observe_live(&snapshot);
    assert!(ui.plan_publication(&snapshot, 100).rows.is_none());
    let (tx, mut rx) = mpsc::channel(8);
    ui.sync_native(&tx);
    let request = rx.try_recv().unwrap();
    assert!(matches!(
        request.command,
        Command::NativeTranscript { after: Some(0), .. }
    ));
    ui.completed_with(
        Done {
            op: request.op,
            result: Ok(Reply::NativeTranscript(vec![
                NativeEntry {
                    entry: entry(1, "user", "older user input"),
                    category: None,
                },
                NativeEntry {
                    entry: entry(2, "assistant", "older completed response"),
                    category: None,
                },
                NativeEntry {
                    entry: entry(3, "notice", "later settings outcome"),
                    category: Some("settings".into()),
                },
            ])),
        },
        |_| Ok(()),
    )
    .unwrap();
    assert!(!ui.tail_busy);
    assert!(
        matches!(ui.plan_publication(&snapshot, 100).rows.as_ref(), Some(rows) if rows.iter().any(|row| row.to_string().contains("older user input")))
    );
    let rendered = publish_all(&mut ui, &snapshot);
    assert!(
        rendered.find("older user input").unwrap()
            < rendered.find("older completed response").unwrap()
    );
    assert!(
        rendered.find("older completed response").unwrap()
            < rendered.find("later settings outcome").unwrap()
    );
    assert_eq!(rendered.matches("later settings outcome").count(), 1);
    assert_eq!(ui.published, Some(3));
    assert!(ui.live_notices.is_empty());
    assert!(ui.pending.is_empty());
}

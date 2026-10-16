use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

use crate::{
    commands::Action,
    runtime::{Client, Command, NoticeLevel},
};

mod completion;
mod history;
mod input;
mod journal;
mod panels;
mod render;
mod state;
mod terminal;
mod theme;
mod transcript;

#[cfg(test)]
mod history_integration_tests;
#[cfg(test)]
mod native_order_tests;
#[cfg(test)]
mod tests;

use self::{
    render::render,
    state::*,
    terminal::{Restore, Screen},
    theme::{muted, strong},
    transcript::clean,
};

const OUTSTANDING: usize = 8;
const ACTIVE_BYTES: usize = 4096;
#[cfg(test)]
const INLINE_HEIGHT: u16 = 6;
const EXIT_WINDOW: Duration = Duration::from_secs(3);
const NATIVE_CHUNK: usize = 1024;

pub async fn run(
    client: Client,
    config: &crate::config::Config,
    config_path: Option<&Path>,
    preferences_path: &Path,
) -> Result<()> {
    let setting_sources = crate::config::interface_sources(config_path, preferences_path)?;
    let _restore = Restore;
    let mut screen = Screen::open()?;
    let mut snapshots = client.subscribe();
    let mut snapshot = client.snapshot();
    let (tx, mut work) = mpsc::channel::<Work>(OUTSTANDING);
    let (results, mut rx) = mpsc::channel::<Done>(OUTSTANDING);
    let worker_client = client.clone();
    let worker = tokio::spawn(async move {
        while let Some(Work { op, command }) = work.recv().await {
            let result = worker_client.command(command).await;
            if results.send(Done { op, result }).await.is_err() {
                break;
            }
        }
    });
    // Aborting on an IO error drops the worker's future, not the runtime. Normal
    // exit waits for the runtime's durable Shutdown acknowledgement below.
    struct Worker(tokio::task::JoinHandle<()>);
    impl Drop for Worker {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _worker = Worker(worker);
    let mut ui = Ui::new(config, &snapshot);
    ui.preferences_path = preferences_path.to_owned();
    ui.setting_sources = setting_sources;
    if let Some(path) = config_path {
        ui.config_source = clean(&path.display().to_string(), 512);
    }
    if snapshot.recovered {
        ui.record_action("session paused", NoticeLevel::Status, "queue");
    }
    screen.publish_rows(&[Line::from(vec![
        Span::styled("am nyam", strong()),
        Span::styled(
            format!(
                "  v{} \u{00b7} local test provider",
                env!("CARGO_PKG_VERSION")
            ),
            muted(),
        ),
    ])])?;
    let mut events = EventStream::new();
    let mut snapshots_open = true;
    let mut termination_requested = false;
    let terminate = terminate();
    tokio::pin!(terminate);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ui.restore_composer(&tx);
    ui.poll(&tx);
    ui.enqueue(&tx, Op::Inputs, Command::Inputs { limit: 100 });

    loop {
        if termination_requested && !ui.exiting {
            ui.execute(Action::Quit, "", false, &snapshot, &tx)?;
        }
        if ui.exiting {
            ui.full_history = false;
            ui.view = View::Root;
        }
        ui.flush_notices(&tx);
        ui.flush_edit_release(&tx);
        ui.flush_pending_edit(&tx);
        ui.advance_chain(&snapshot, &tx);
        ui.try_shutdown(&tx);
        ui.full_history = ui.view == View::History && !ui.exiting;
        let frame = screen.begin_frame()?;
        ui.observe_live(&snapshot);
        ui.sync_native(&tx);
        screen.fullscreen(ui.full_history)?;
        let (width, physical_height) = crossterm::terminal::size()?;
        if !ui.full_history {
            let batches = if ui.exiting {
                ui.live_notices.len() + 1
            } else {
                1
            };
            for _ in 0..batches {
                ui.publish_root(&snapshot, &mut screen)?;
            }
        }
        let owned_height = render::inline_height(&ui, &snapshot, width, physical_height);
        if !ui.full_history {
            screen.set_height(owned_height)?;
        }
        screen.capture_mouse(ui.full_history)?;
        if ui.view == View::History {
            ui.history.sync_active(snapshot.active.as_ref());
            ui.prepare_history(width, owned_height);
        }
        if let Some(terminal) = &mut screen.full {
            terminal.draw(|frame| render(frame, &ui, &snapshot))?;
        } else {
            screen.inline.draw(|frame| render(frame, &ui, &snapshot))?;
        }
        frame.finish()?;
        tokio::select! {
            _ = tick.tick() => {
                if ui.exit_armed.is_some_and(|at|at.elapsed()>=EXIT_WINDOW) {ui.exit_armed=None;}
                if ui.stop_armed.is_some_and(|at| at.elapsed() > Duration::from_millis(700)) { ui.stop_armed = None; }
                if ui.defaults_task.as_ref().is_some_and(|task| task.is_finished()) {
                    let _ = ui.finish_pending_defaults().await;
                }
                if ui.bindings_task.as_ref().is_some_and(|task| task.is_finished()) {
                    let _ = ui.finish_pending_bindings().await;
                }
                ui.autosave(&tx, ui.exiting);
                if ui.last_poll.elapsed() >= Duration::from_millis(300) {
                    ui.poll(&tx);
                }
            }
            changed = snapshots.changed(), if snapshots_open => {
                if changed.is_err() {
                    if !ui.exiting { anyhow::bail!("runtime snapshot channel closed"); }
                    snapshots_open = false;
                } else {
                    snapshots.borrow_and_update();
                    snapshot = client.snapshot();
                     if ui.view == View::Queue { ui.selected = ui.selected.min(snapshot.queue.len().saturating_sub(1)); }
                }
            }
            done = rx.recv() => {
                let done = done.context("command worker stopped")?;
                // A chain's next step must see the acknowledged queue state even
                // when the watch notification loses the select race to this reply.
                snapshot = client.snapshot();
                if termination_requested && matches!(done.op, Op::Shutdown) && done.result.is_err() {
                    ui.finish_pending_defaults().await?;
                    ui.finish_pending_bindings().await?;
                    return done.result.map(|_| ()).context("SIGTERM shutdown failed");
                }
                if ui.completed(done, &mut screen)? {
                    ui.finish_pending_defaults().await?;
                    ui.finish_pending_bindings().await?;
                    if let Some(error) = ui.exit_error.take() { anyhow::bail!("{error}"); }
                    break;
                }
            }
            result = &mut terminate, if !termination_requested => {
                result?;
                termination_requested = true;
            }
            event = events.next() => {
                let event = event.context("terminal input stream closed")??;
                match event {
                    Event::Paste(text) if !ui.exiting && !ui.draft_busy && ui.pending_send.is_none() && ui.view == View::Root => {
                        ui.exit_armed=None;
                        ui.append(&text);
                    },
                    Event::Paste(text) if ui.view == View::Bindings && ui.binding_key.is_some() && ui.bindings_task.is_none() => {
                        ui.stop_armed=None;
                        ui.binding_commands.insert(&clean(&text, 4096).replace('\n', " "));
                        ui.reset_completion();
                    },
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL && matches!(key.code,KeyCode::Char(ch) if ch.eq_ignore_ascii_case(&'c')) {
                            ui.ctrl_c(Instant::now(),&snapshot,&tx);
                            continue;
                        }
                         if ui.exiting { continue; }
                         if key.code != KeyCode::Esc { ui.stop_armed=None; }
                         ui.exit_armed=None;
                         if ui.view == View::Bindings || ui.pending_send.is_some() {
                             if let Err(error) = ui.key(key, &snapshot, &tx, width, owned_height) { ui.notify(format!("{error:#}"), true); }
                             continue;
                         }
                         let keymap = ui.keys.compile()?;
                         let macros = ui.keys.compile_macros()?;
                          if let Some((_, commands)) = macros.iter().find(|(chord, _)| chord.matches(key)) {
                              ui.stop_armed=None;
                             if let Err(error) = ui.start_chain(commands.clone()) { ui.notify(format!("{error:#}"), true); }
                             continue;
                         }
                        if let Some((_, action))=keymap.iter().find(|(chord,_)|chord.matches(key)) {
                             if let Err(error)=ui.shortcut(*action,&snapshot,&tx) { ui.notify(format!("{error:#}"),true); }
                            continue;
                        }
                        if let Err(error) = ui.key(key, &snapshot, &tx, width, owned_height) {
                            ui.notify(clean(&format!("{error:#}"),1024), true);
                        }
                    }
                    Event::Mouse(mouse) if ui.full_history => {
                         ui.stop_armed=None;
                        ui.exit_armed=None;
                         let delta: i32=match mouse.kind {MouseEventKind::ScrollUp=>-3,MouseEventKind::ScrollDown=>3,_=>0};
                         for _ in 0..delta.unsigned_abs() {
                             let code = if delta < 0 { KeyCode::Up } else { KeyCode::Down };
                             if let Err(error) = ui.key(KeyEvent::new(code, KeyModifiers::NONE), &snapshot, &tx, width, owned_height) { ui.notify(format!("{error:#}"), true); break; }
                         }
                    }
                    _ => {}
                }
            }
        }
    }
    screen.fullscreen(false)?;
    screen.inline.clear()?;
    screen.inline.show_cursor()?;
    Ok(())
}

async fn terminate() -> Result<()> {
    #[cfg(unix)]
    {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        signal
            .recv()
            .await
            .context("termination signal listener stopped")?;
        Ok(())
    }
    #[cfg(not(unix))]
    std::future::pending::<Result<()>>().await
}

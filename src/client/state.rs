use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use tokio::sync::mpsc;

use crate::{
    commands::{self, Action, Arguments, KeyBindings},
    runtime::{
        Command, DeliveryMode, DraftRecord, HistoryEntry, HistoryMatch, MAX_NOTICE_BYTES,
        NoticeLevel, Reply, Snapshot,
    },
};

use super::{
    EXIT_WINDOW, NATIVE_CHUNK, OUTSTANDING, completion,
    history::{
        HistoryView, Load,
        browser::{Effect, Focus as HistoryFocus, HistoryBrowser, Selection},
    },
    input::Editor,
    journal::JournalView,
    render,
    terminal::Screen,
    transcript::{clean, sanitized, transcript_rows},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum View {
    Root,
    History,
    Queue,
    Help,
    Settings,
    Details,
    Drafts,
    Bindings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputMode {
    Steer,
    After,
    Draft,
}

pub(super) struct PendingSend {
    pub(super) command: Command,
    pub(super) text: String,
    pub(super) cursor: usize,
    pub(super) selected: usize,
    pub(super) clear: bool,
    pub(super) session: String,
    action: bool,
    composer_revision: Option<i64>,
    draft_id: Option<String>,
}

const NOTICE_LIMIT: usize = 16;

pub(super) const HELP_CATEGORIES: &[&str] = &["All", "AM", "Input", "Session", "Preferences"];

struct PanelReturn {
    view: View,
    selected: usize,
    info_scroll: u16,
    palette: bool,
    pinned: bool,
    commands_focus: bool,
    query: String,
    cursor: usize,
    help: Option<HelpReturn>,
    settings: Option<(usize, Option<usize>)>,
}

struct HelpReturn {
    query: String,
    cursor: usize,
    category: usize,
    selected: usize,
    detail: Option<&'static commands::Spec>,
    detail_selected: usize,
}

pub(super) struct HistorySearch {
    generation: u64,
    query: String,
    source: String,
    cursor: usize,
    context: (View, bool, bool, Option<String>),
    menu: completion::Completion,
    search: bool,
    reverse: bool,
    journal_cursor: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActionLoad {
    Latest,
    Older,
    Newer,
}

pub(super) enum Op {
    Chain(Box<Op>),
    Notice,
    BeginEdit,
    FinishEdit,
    CancelEdit,
    ReleaseEdit,
    Cancel {
        id: String,
        clear_draft: bool,
    },
    Stop {
        clear_draft: bool,
    },
    Control {
        clear_draft: bool,
        pause: bool,
        outcome: &'static str,
    },
    Tail,
    History(Load, bool),
    HistoryRefresh,
    HistorySearch(HistorySearch),
    Inputs,
    Events,
    Shutdown,
    RestoreComposer,
    SaveComposer {
        text: String,
        cursor: usize,
    },
    Drafts,
    SaveDraft {
        text: String,
        clear: bool,
    },
    DeleteDraft {
        id: String,
        clear: bool,
    },
    LoadDraft(DraftRecord),
    Dispatch {
        clear: bool,
        text: String,
        action: bool,
        composer_revision: Option<i64>,
        draft_id: Option<String>,
    },
    Actions {
        load: ActionLoad,
    },
    ActionRefresh {
        id: String,
        seq: i64,
    },
    Artifact {
        id: String,
        offset: u64,
        generation: u64,
    },
}

pub(super) struct Work {
    pub(super) op: Op,
    pub(super) command: Command,
}
pub(super) struct Done {
    pub(super) op: Op,
    pub(super) result: Result<Reply>,
}

pub(super) type Saved = HistoryEntry;

struct LiveAttempt {
    id: String,
    text: String,
    start: usize,
    offset: usize,
    complete: bool,
}

struct QueuePreview {
    id: String,
    text: String,
    truncated: bool,
}

enum PublicationChange {
    None,
    Notice {
        entry: Saved,
        category: &'static str,
        live: bool,
        ordered: bool,
    },
    FlushNotice,
    SkipNotices {
        through: i64,
    },
    Text {
        seq: i64,
        end: usize,
        last: bool,
        attempt: Option<String>,
        user_attempt: Option<String>,
    },
}

pub(super) struct PublicationPlan {
    pub(super) rows: Option<Vec<Line<'static>>>,
    change: PublicationChange,
}

pub(super) struct LiveProjection<'a> {
    notice: Option<&'a Saved>,
    advance: Option<(&'a str, usize, bool)>,
    through: Option<i64>,
    retire_user: Option<&'a str>,
}

pub(super) struct Ui {
    pub(super) view: View,
    pub(super) draft: Editor,
    pub(super) edit: Option<(String, i64)>,
    // The ordinary composer is suspended while a queue item is leased.
    pub(super) edit_draft: Option<(String, usize)>,
    pub(super) draft_busy: bool,
    pub(super) selected: usize,
    pub(super) history: HistoryView,
    pub(super) history_browser: HistoryBrowser,
    pub(super) history_rows: Vec<Line<'static>>,
    pub(super) history_busy: bool,
    history_refresh_busy: bool,
    history_refresh_stale: bool,
    pub(super) info_scroll: u16,
    pub(super) settings_selected: usize,
    pub(super) settings_editor: Option<usize>,
    pub(super) settings_feedback: Option<(String, bool)>,
    pub(super) help_query: Editor,
    pub(super) help_category: usize,
    pub(super) help_selected: usize,
    pub(super) help_detail: Option<&'static commands::Spec>,
    pub(super) help_detail_selected: usize,
    panel_returns: Vec<PanelReturn>,
    pub(super) keys: KeyBindings,
    pub(super) palette: bool,
    pub(super) palette_pinned: bool,
    pub(super) commands_focus: bool,
    pub(super) mode: InputMode,
    pub(super) drafts: Vec<DraftRecord>,
    pub(super) drafts_busy: bool,
    drafts_loaded: bool,
    pub(super) pending_send: Option<PendingSend>,
    pub(super) journal: JournalView,
    pub(super) actions_busy: bool,
    actions_follow: bool,
    actions_oldest: bool,
    action_refresh_busy: bool,
    action_refresh_cursor: i64,
    release_edit: Option<(String, i64)>,
    pending_edit: Option<(String, i64)>,
    pub(super) artifact_busy: bool,
    preview_start: u64,
    preview_generation: u64,
    browser_artifact: Option<(String, Option<String>)>,
    history_preview_size: (u16, u16),
    pub(super) binding_capture: bool,
    pub(super) binding_key: Option<String>,
    pub(super) binding_commands: Editor,
    pub(super) bindings_task: Option<tokio::task::JoinHandle<Result<KeyBindings>>>,
    binding_clear: Option<String>,
    binding_palette: Option<String>,
    pub(super) command_chain: VecDeque<String>,
    chain_wait: bool,
    chain_execution: bool,
    chain_pending: usize,
    composer_ready: bool,
    composer_revision: i64,
    composer_saved: (String, usize),
    composer_observed: (String, usize),
    composer_changed: Instant,
    composer_busy: bool,
    composer_error: bool,
    pub(super) palette_query: Editor,
    pub(super) completion_selected: usize,
    pub(super) completion_active: bool,
    pub(super) pager_selected: bool,
    pub(super) pager_search: Option<Editor>,
    completion_cache: Option<completion::Completion>,
    completion_original: Option<(String, usize)>,
    pub(super) completion_hidden: bool,
    pub(super) history_search_busy: Option<u64>,
    history_search_generation: u64,
    history_search_cache: Option<(String, Vec<HistoryMatch>, i64)>,
    pub(super) full_history: bool,
    pub(super) pause_pending: bool,
    pub(super) stop_pending: bool,
    pub(super) stopped: bool,
    pub(super) stop_armed: Option<Instant>,
    pub(super) notice: Option<(String, Instant, bool)>,
    pub(super) pending_notices: VecDeque<(String, NoticeLevel)>,
    pending_categories: VecDeque<Option<&'static str>>,
    pub(super) notice_busy: bool,
    pub(super) notice_in_flight: Option<(String, NoticeLevel)>,
    notice_category: Option<&'static str>,
    pub(super) action_categories: BTreeMap<i64, &'static str>,
    native_group: Option<(i64, &'static str)>,
    native_notice: Option<Saved>,
    pub(super) exit_armed: Option<Instant>,
    pub(super) exiting: bool,
    pub(super) quit_pending: bool,
    pub(super) exit_error: Option<String>,
    pub(super) outstanding: usize,
    pub(super) tail_busy: bool,
    pub(super) events_busy: bool,
    pub(super) event_cursor: i64,
    pub(super) published: Option<i64>,
    pub(super) pending: BTreeMap<i64, Saved>,
    pub(super) native_offset: usize,
    live_attempts: VecDeque<LiveAttempt>,
    observed_active: Option<String>,
    observed_front: Option<QueuePreview>,
    accepted_previews: VecDeque<(String, QueuePreview)>,
    native_fullscreen: bool,
    native_sync_required: bool,
    native_tail_stale: bool,
    pub(super) live_notices: VecDeque<Saved>,
    pub(super) printed_notices: VecDeque<(i64, i64)>,
    native_separator: bool,
    pub(super) inputs: Vec<String>,
    pub(super) recall_cursor: Option<usize>,
    pub(super) recall_draft: String,
    recall_draft_cursor: usize,
    pub(super) interface: crate::config::Interface,
    pub(super) config_source: String,
    pub(super) setting_sources: BTreeMap<&'static str, String>,
    pub(super) preferences_path: PathBuf,
    pub(super) saving_defaults: bool,
    pub(super) defaults_task: Option<tokio::task::JoinHandle<Result<crate::settings::Change>>>,
    pub(super) defaults_palette: bool,
    defaults_clear: bool,
    defaults_chain: bool,
    defaults_panel: bool,
    page: usize,
    message_limit: usize,
    pub(super) last_poll: Instant,
}

impl Ui {
    pub(super) fn new(config: &crate::config::Config, snapshot: &Snapshot) -> Self {
        Self {
            view: View::Root,
            draft: Editor::new(config.limits.message_bytes),
            edit: None,
            edit_draft: None,
            draft_busy: false,
            selected: 0,
            history: HistoryView::new(config.limits.history_page),
            history_browser: HistoryBrowser::default(),
            history_rows: Vec::new(),
            history_busy: false,
            history_refresh_busy: false,
            history_refresh_stale: false,
            info_scroll: 0,
            settings_selected: 0,
            settings_editor: None,
            settings_feedback: None,
            help_query: Editor::new(256),
            help_category: 0,
            help_selected: 0,
            help_detail: None,
            help_detail_selected: 0,
            panel_returns: Vec::new(),
            keys: config.keys.clone(),
            palette: false,
            palette_pinned: false,
            commands_focus: true,
            mode: InputMode::After,
            drafts: Vec::new(),
            drafts_busy: false,
            drafts_loaded: false,
            pending_send: None,
            journal: JournalView::default(),
            actions_busy: false,
            actions_follow: true,
            actions_oldest: false,
            action_refresh_busy: false,
            action_refresh_cursor: 0,
            release_edit: None,
            pending_edit: None,
            artifact_busy: false,
            preview_start: 0,
            preview_generation: 0,
            browser_artifact: None,
            history_preview_size: (80, 1),
            binding_capture: false,
            binding_key: None,
            binding_commands: Editor::new(4096),
            bindings_task: None,
            binding_clear: None,
            binding_palette: None,
            command_chain: VecDeque::new(),
            chain_wait: false,
            chain_execution: false,
            chain_pending: 0,
            composer_ready: false,
            composer_revision: 0,
            composer_saved: (String::new(), 0),
            composer_observed: (String::new(), 0),
            composer_changed: Instant::now(),
            composer_busy: false,
            composer_error: false,
            palette_query: Editor::new(4096),
            completion_selected: 0,
            completion_active: false,
            pager_selected: false,
            pager_search: None,
            completion_cache: None,
            completion_original: None,
            completion_hidden: false,
            history_search_busy: None,
            history_search_generation: 0,
            history_search_cache: None,
            full_history: false,
            pause_pending: false,
            stop_pending: false,
            stopped: false,
            stop_armed: None,
            notice: None,
            pending_notices: VecDeque::new(),
            pending_categories: VecDeque::new(),
            notice_busy: false,
            notice_in_flight: None,
            notice_category: None,
            action_categories: BTreeMap::new(),
            native_group: None,
            native_notice: None,
            exit_armed: None,
            exiting: false,
            quit_pending: false,
            exit_error: None,
            outstanding: 0,
            tail_busy: false,
            events_busy: false,
            event_cursor: snapshot.cursor,
            published: None,
            pending: BTreeMap::new(),
            native_offset: 0,
            live_attempts: VecDeque::new(),
            observed_active: None,
            observed_front: None,
            accepted_previews: VecDeque::new(),
            native_fullscreen: false,
            native_sync_required: false,
            native_tail_stale: false,
            live_notices: VecDeque::new(),
            printed_notices: VecDeque::new(),
            native_separator: true,
            inputs: Vec::new(),
            recall_cursor: None,
            recall_draft: String::new(),
            recall_draft_cursor: 0,
            interface: config.interface.clone(),
            config_source: "Built-in defaults (no config file loaded)".into(),
            setting_sources: crate::settings::catalog(&config.interface)
                .into_iter()
                .map(|setting| (setting.key, "Built-in defaults".into()))
                .collect(),
            preferences_path: PathBuf::new(),
            saving_defaults: false,
            defaults_task: None,
            defaults_palette: false,
            defaults_clear: true,
            defaults_chain: false,
            defaults_panel: false,
            page: config.limits.history_page.clamp(1, 200),
            message_limit: config.limits.message_bytes,
            last_poll: Instant::now(),
        }
    }

    fn composer_dirty(&self) -> bool {
        self.composer_ready
            && self.edit.is_none()
            && (self.draft.text() != self.composer_saved.0
                || self.draft.cursor() != self.composer_saved.1)
    }

    fn change_binding(&mut self, action: Action, args: &str) -> Result<()> {
        anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
        anyhow::ensure!(
            self.bindings_task.is_none() && !self.saving_defaults,
            "Wait for the pending binding save"
        );
        let (key, commands, save) = if action == Action::Bind {
            commands::parse_bind(args)?
        } else {
            let tokens = args.split_whitespace().collect::<Vec<_>>();
            anyhow::ensure!(
                matches!(tokens.as_slice(), [_] | [_, "--default"]),
                "Usage: /unbind <key> [--default]"
            );
            (tokens[0].to_owned(), Vec::new(), tokens.len() == 2)
        };
        let chord = commands::Chord::parse(&key)?;
        let mut keys = self.keys.clone();
        keys.macros
            .retain(|binding| commands::Chord::parse(&binding.key).ok() != Some(chord));
        if action == Action::Bind {
            keys.macros.push(commands::CommandBinding { key, commands });
        }
        keys.compile_macros()?;
        if save {
            self.save_bindings(keys)?;
            self.binding_palette = if self.view == View::Root
                && self.palette
                && self.commands_focus
                && !self.chain_execution
            {
                Some(self.palette_query.text().to_owned())
            } else {
                None
            };
        } else {
            self.keys = keys;
            self.record_action(
                "Binding updated for this client",
                NoticeLevel::Info,
                "bindings",
            );
            self.notify("Binding updated for this client", false);
            self.binding_key = None;
            self.binding_capture = false;
            if self.view == View::Root {
                self.palette_query.clear();
                self.palette = self.palette_pinned;
            }
        }
        Ok(())
    }

    fn save_bindings(&mut self, keys: KeyBindings) -> Result<()> {
        anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
        anyhow::ensure!(
            self.bindings_task.is_none() && !self.saving_defaults && self.defaults_task.is_none(),
            "Wait for the pending preferences save"
        );
        anyhow::ensure!(
            !self.preferences_path.as_os_str().is_empty(),
            "No user preferences path available"
        );
        keys.compile_macros()?;
        let path = self.preferences_path.clone();
        self.bindings_task = Some(tokio::task::spawn_blocking(move || {
            crate::settings::save_keys(&path, &keys)?;
            Ok(keys)
        }));
        Ok(())
    }

    pub(super) async fn finish_pending_bindings(&mut self) -> Result<()> {
        if let Some(task) = self.bindings_task.take() {
            self.chain_wait = false;
            match task
                .await
                .context("binding worker failed")
                .and_then(|result| result)
            {
                Ok(keys) => {
                    self.keys = keys;
                    self.binding_key = None;
                    self.binding_capture = false;
                    if self
                        .binding_palette
                        .take()
                        .is_some_and(|source| self.palette_query.text() == source)
                    {
                        self.palette_query.clear();
                        self.palette = self.palette_pinned;
                    }
                    if let Some(source) = self.binding_clear.take() {
                        if self.draft.text() == source {
                            self.draft.clear();
                        }
                        self.draft_busy = false;
                    }
                    self.record_action("Binding defaults saved", NoticeLevel::Info, "bindings");
                    self.notify("Binding defaults saved", false);
                }
                Err(error) => {
                    self.binding_palette = None;
                    if self.binding_clear.take().is_some() {
                        self.draft_busy = false;
                    }
                    self.command_chain.clear();
                    if self.exiting {
                        self.exit_error = Some(format!("Bindings not saved: {error:#}"));
                    }
                    let text = format!("Bindings not saved: {error:#}");
                    self.record_action(format!("error: {text}"), NoticeLevel::Error, "bindings");
                    self.notify(text, true);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn binding_key_event(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
        width: u16,
        height: u16,
    ) -> Result<()> {
        if key.code == KeyCode::Esc {
            if self.completion_original.is_some() {
                self.restore_completion();
                self.reset_completion();
                return Ok(());
            }
            self.reset_completion();
            if self.binding_capture || self.binding_key.is_some() {
                self.binding_capture = false;
                self.binding_key = None;
            } else {
                self.return_panel();
            }
            return Ok(());
        }
        if self.bindings_task.is_some() {
            return Ok(());
        }
        if self.binding_capture {
            let (label, chord) = commands::Chord::from_event(key)?;
            anyhow::ensure!(
                !self
                    .keys
                    .compile()?
                    .iter()
                    .any(|(existing, _)| *existing == chord),
                "Shortcut is already assigned to a built-in action"
            );
            anyhow::ensure!(label != "ctrl+c", "Ctrl+C is reserved for exit");
            self.binding_key = Some(label);
            self.binding_commands.clear();
            self.binding_capture = false;
            self.reset_completion();
            return Ok(());
        }
        if let Some(label) = self.binding_key.clone() {
            let reverse = key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
            if key.code == KeyCode::Tab || reverse {
                if self.completion_active {
                    self.navigate_completion(reverse, 1)?;
                } else {
                    self.start_history_completion(snapshot, false, reverse, tx)?;
                }
                return Ok(());
            }
            if self.completion_active {
                match key.code {
                    KeyCode::Enter => {
                        if self.pager_selected {
                            anyhow::ensure!(
                                render::selection_visible(self, snapshot, width, height),
                                "Enlarge the terminal to read the selected completion"
                            );
                        }
                        self.reset_completion();
                        return Ok(());
                    }
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                        self.navigate_completion(
                            matches!(key.code, KeyCode::Up | KeyCode::PageUp),
                            if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                                6
                            } else {
                                1
                            },
                        )?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            if key.code == KeyCode::Enter && self.history_search_busy.is_some() {
                self.reset_completion();
                return Ok(());
            }
            match key.code {
                KeyCode::Enter => {
                    self.reset_completion();
                    let default = key.modifiers.contains(KeyModifiers::ALT);
                    self.change_binding(
                        Action::Bind,
                        &format!(
                            "{label} {}{}",
                            self.binding_commands.text(),
                            if default
                                && self.binding_commands.text().split_whitespace().last()
                                    != Some("--default")
                            {
                                " --default"
                            } else {
                                ""
                            }
                        ),
                    )?;
                }
                KeyCode::Backspace => {
                    self.binding_commands.backspace();
                }
                KeyCode::Delete => {
                    self.binding_commands.delete();
                }
                KeyCode::Left => {
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.binding_commands.word_left();
                    } else {
                        self.binding_commands.left();
                    }
                }
                KeyCode::Right => {
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.binding_commands.word_right();
                    } else {
                        self.binding_commands.right();
                    }
                }
                KeyCode::Home => {
                    self.binding_commands.home();
                }
                KeyCode::End => {
                    self.binding_commands.end();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.binding_commands.insert(&ch.to_string());
                }
                _ => {}
            }
            if key.code != KeyCode::Enter {
                self.reset_completion();
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.keys.macros.len()),
            KeyCode::Char('n') => self.binding_capture = true,
            KeyCode::Char('d') => {
                self.save_bindings(self.keys.clone())?;
                self.binding_palette = None;
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(binding) = self.keys.macros.get(self.selected) {
                    let text = binding.commands.join("; ");
                    anyhow::ensure!(
                        text.len() <= self.binding_commands.max_bytes(),
                        "Binding chain exceeds the 4096-byte editor limit; original binding retained"
                    );
                    self.binding_key = Some(binding.key.clone());
                    self.binding_commands.set_text(&text);
                    self.reset_completion();
                } else {
                    self.binding_capture = true;
                }
            }
            KeyCode::Delete => {
                let binding = self
                    .keys
                    .macros
                    .get(self.selected)
                    .context("No binding selected")?
                    .clone();
                self.change_binding(Action::Unbind, &binding.key)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn restore_composer(&mut self, tx: &mpsc::Sender<Work>) {
        let accepted = self.enqueue(tx, Op::RestoreComposer, Command::Composer);
        self.draft_busy = true;
        self.composer_error = !accepted;
    }

    pub(super) fn autosave(&mut self, tx: &mpsc::Sender<Work>, drain: bool) {
        if !self.composer_ready
            || self.edit.is_some()
            || self.draft_busy
            || self.pending_send.is_some()
        {
            return;
        }
        let current = (self.draft.text().to_owned(), self.draft.cursor());
        if current != self.composer_observed {
            self.composer_observed = current.clone();
            self.composer_changed = Instant::now();
            self.composer_error = false;
        }
        if self.composer_busy
            || self.composer_error
            || !self.composer_dirty()
            || (!drain && self.composer_changed.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        let (text, cursor) = current;
        if self.enqueue(
            tx,
            Op::SaveComposer {
                text: text.clone(),
                cursor,
            },
            Command::SaveComposer {
                text,
                cursor,
                revision: self.composer_revision,
            },
        ) {
            self.composer_busy = true;
        }
    }

    fn draft_target(&self, prefix: &str) -> Result<DraftRecord> {
        anyhow::ensure!(
            prefix.len() >= 4,
            "Use at least four characters of a draft ID"
        );
        let mut matches = self
            .drafts
            .iter()
            .filter(|draft| draft.id.starts_with(prefix));
        let draft = matches
            .next()
            .context("No saved draft with that ID; open /drafts first")?;
        anyhow::ensure!(
            matches.next().is_none(),
            "Ambiguous draft ID; use more characters"
        );
        Ok(draft.clone())
    }

    fn load_actions(&mut self, tx: &mpsc::Sender<Work>, load: ActionLoad) {
        if self.actions_busy {
            return;
        }
        let command = match load {
            ActionLoad::Latest => {
                if let Some(last) = self.journal.actions.last() {
                    Command::ActionsAfter {
                        after: last.seq,
                        limit: 100,
                    }
                } else {
                    Command::Actions {
                        before: None,
                        limit: 100,
                    }
                }
            }
            ActionLoad::Older => {
                if self.actions_oldest {
                    return;
                }
                let Some(first) = self.journal.actions.first() else {
                    return;
                };
                Command::Actions {
                    before: Some(first.seq),
                    limit: 100,
                }
            }
            ActionLoad::Newer => {
                let Some(last) = self.journal.actions.last() else {
                    return;
                };
                Command::ActionsAfter {
                    after: last.seq,
                    limit: 100,
                }
            }
        };
        if load == ActionLoad::Older {
            self.actions_follow = false;
        }
        self.actions_busy = self.enqueue(tx, Op::Actions { load }, command);
    }

    fn refresh_action(&mut self, tx: &mpsc::Sender<Work>) {
        if self.action_refresh_busy || self.outstanding >= OUTSTANDING {
            return;
        }
        let mut running = self
            .journal
            .actions
            .iter()
            .filter(|action| action.status == "running");
        let Some(action) = running
            .clone()
            .find(|action| action.seq > self.action_refresh_cursor)
            .or_else(|| running.next())
        else {
            return;
        };
        // Independent of forward paging, including page-one catchup and browsing.
        // Background refresh failures must not create durable error notices.
        if tx
            .try_send(Work {
                op: Op::ActionRefresh {
                    id: action.id.clone(),
                    seq: action.seq,
                },
                command: Command::Actions {
                    before: action.seq.checked_add(1),
                    limit: 1,
                },
            })
            .is_ok()
        {
            self.action_refresh_cursor = action.seq;
            self.action_refresh_busy = true;
            self.outstanding += 1;
        }
    }

    pub(super) fn flush_edit_release(&mut self, tx: &mpsc::Sender<Work>) {
        if let Some((id, revision)) = self.release_edit.clone()
            && self.enqueue(tx, Op::ReleaseEdit, Command::CancelEdit { id, revision })
        {
            self.release_edit = None;
        }
    }

    pub(super) fn flush_pending_edit(&mut self, tx: &mpsc::Sender<Work>) {
        if self.composer_busy || self.pending_edit.is_none() {
            return;
        }
        if self.composer_error {
            self.pending_edit = None;
            self.edit_draft = None;
            self.draft_busy = false;
            self.notify("Composer save not confirmed; no edit lease acquired", true);
            return;
        }
        if self.composer_dirty() {
            self.draft_busy = false;
            self.autosave(tx, true);
            self.draft_busy = true;
            if self.composer_busy {
                return;
            }
            self.pending_edit = None;
            self.edit_draft = None;
            self.draft_busy = false;
            return;
        }
        let (id, revision) = self.pending_edit.take().unwrap();
        self.draft_busy = self.enqueue(tx, Op::BeginEdit, Command::BeginEdit { id, revision });
        if !self.draft_busy {
            self.edit_draft = None;
        }
    }

    fn begin_edit(
        &mut self,
        item: crate::runtime::QueueItem,
        tx: &mpsc::Sender<Work>,
    ) -> Result<bool> {
        anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
        anyhow::ensure!(
            !self.draft_busy && self.edit.is_none(),
            "Wait for input or finish your queued edit"
        );
        anyhow::ensure!(
            item.text.len() <= self.message_limit.min(self.draft.max_bytes()),
            "Queued message exceeds the configured byte limit; no edit lease acquired"
        );
        // Save the suspended composer, never the unsubmitted leased text.
        self.autosave(tx, true);
        self.edit_draft = Some((self.draft.text().to_owned(), self.draft.cursor()));
        self.pending_edit = Some((item.id, item.revision));
        self.draft_busy = true;
        self.reset_completion();
        self.flush_pending_edit(tx);
        Ok(self.draft_busy)
    }

    fn restore_edit_draft(&mut self) {
        self.edit = None;
        if let Some((text, cursor)) = self.edit_draft.take() {
            self.draft.set_text(&text);
            self.draft.set_cursor(cursor);
        } else {
            self.draft.clear();
        }
        self.reset_completion();
    }

    fn load_artifact(&mut self, tx: &mpsc::Sender<Work>, offset: u64, before: bool) {
        if self.artifact_busy {
            return;
        }
        let Some(output) = self
            .journal
            .actions
            .get(self.journal.selected)
            .and_then(|action| action.output.as_ref())
        else {
            return;
        };
        let id = output.id.clone();
        self.artifact_busy = self.enqueue(
            tx,
            Op::Artifact {
                id: id.clone(),
                offset,
                generation: self.preview_generation,
            },
            if before {
                Command::ReadArtifactBefore {
                    id,
                    before: offset,
                    limit: 8192,
                }
            } else {
                Command::ReadArtifact {
                    id,
                    offset,
                    limit: 8192,
                }
            },
        );
    }

    pub(super) fn start_chain(&mut self, commands: Vec<String>) -> Result<()> {
        anyhow::ensure!(
            self.command_chain.is_empty() && !self.chain_wait,
            "A command chain is already running"
        );
        self.command_chain = commands.into();
        Ok(())
    }

    pub(super) fn advance_chain(&mut self, snapshot: &Snapshot, tx: &mpsc::Sender<Work>) {
        if self.exiting
            || self.chain_wait
            || self.pending_send.is_some()
            || self.bindings_task.is_some()
            || self.saving_defaults
            || self.draft_busy
        {
            return;
        }
        while let Some(command) = self.command_chain.pop_front() {
            let field = (
                self.palette,
                self.palette_pinned,
                self.commands_focus,
                self.palette_query.text().to_owned(),
                self.palette_query.cursor(),
            );
            let mut preserve_field = true;
            self.chain_execution = true;
            let result = (|| -> Result<bool> {
                let (spec, args) =
                    commands::parse(&command)?.context("Bindings may execute commands only")?;
                preserve_field = !matches!(
                    spec.action,
                    Action::Commands | Action::Exit | Action::FocusInput
                );
                anyhow::ensure!(
                    spec.action != Action::Settings
                        || args.is_empty()
                        || args.split_whitespace().count() >= 2,
                    "A settings chain step needs a complete key and value"
                );
                let before = self.outstanding;
                anyhow::ensure!(
                    self.execute(spec.action, args, false, snapshot, tx)?,
                    "Command was not accepted"
                );
                Ok(self.outstanding > before
                    || self.saving_defaults
                    || self.bindings_task.is_some()
                    || self.pending_send.is_some())
            })();
            self.chain_execution = false;
            if preserve_field {
                self.palette = field.0;
                self.palette_pinned = field.1;
                self.commands_focus = field.2;
                self.palette_query.set_text(&field.3);
                self.palette_query.set_cursor(field.4);
            }
            match result {
                Ok(true) => {
                    self.chain_wait = true;
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    self.command_chain.clear();
                    self.notify(format!("Command chain stopped: {error:#}"), true);
                    return;
                }
            }
        }
    }

    fn send_command(
        &mut self,
        command: Command,
        clear: bool,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
        explicit: bool,
    ) -> bool {
        let text = self.draft.text().to_owned();
        let composer_revision = match &command {
            Command::SubmitInput {
                composer_revision, ..
            }
            | Command::SubmitInputAccumulated {
                composer_revision, ..
            } => Some(*composer_revision),
            _ => None,
        };
        let draft_id = match &command {
            Command::DispatchDraftChecked { id, .. }
            | Command::DispatchDraftAccumulated { id, .. } => Some(id.clone()),
            _ => None,
        };
        let action = matches!(
            command,
            Command::DispatchDraftChecked { .. }
                | Command::DispatchDraftAccumulated { .. }
                | Command::SubmitInput {
                    mode: DeliveryMode::Force,
                    ..
                }
                | Command::SubmitInputAccumulated {
                    mode: DeliveryMode::Force,
                    ..
                }
        );
        if snapshot.paused && !explicit {
            self.view = View::Root;
            self.full_history = false;
            self.reset_completion();
            self.pending_send = Some(PendingSend {
                command,
                text,
                cursor: self.draft.cursor(),
                selected: 0,
                clear,
                session: snapshot.session_id.clone(),
                action,
                composer_revision,
                draft_id,
            });
            self.notify("Queue paused: choose hold / run / cancel", false);
            true
        } else {
            let accepted = self.enqueue(
                tx,
                Op::Dispatch {
                    clear,
                    text,
                    action,
                    composer_revision,
                    draft_id,
                },
                command,
            );
            self.draft_busy = accepted;
            accepted
        }
    }

    fn ensure_send_ready(&self) -> Result<()> {
        anyhow::ensure!(
            !self.composer_busy,
            "Wait for the composer save acknowledgement, then retry send"
        );
        anyhow::ensure!(
            !self.pause_pending && !self.stop_pending,
            "Wait for the pause/resume acknowledgement, then retry send"
        );
        Ok(())
    }

    fn input_command(&self, mode: DeliveryMode, resume: bool, snapshot: &Snapshot) -> Command {
        let source = self.draft.text().to_owned();
        let text = if source.starts_with("//") {
            source[1..].to_owned()
        } else {
            source.clone()
        };
        Command::SubmitInputAccumulated {
            source,
            text,
            composer_revision: self.composer_revision,
            mode,
            resume,
            expected_paused: snapshot.paused,
        }
    }

    pub(super) fn notify(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            let durable = notice_text(&format!(
                "error: {}",
                text.strip_prefix("error: ").unwrap_or(&text)
            ));
            let repeated = self.notice.as_ref().is_some_and(|(current, _, error)| {
                *error
                    && notice_text(&format!(
                        "error: {}",
                        current.strip_prefix("error: ").unwrap_or(current)
                    )) == durable
            });
            if !repeated {
                self.record_notice(durable, NoticeLevel::Error);
            }
        }
        self.notice = Some((text, Instant::now(), error));
    }

    fn notice_load(&self) -> usize {
        // Reserve room for results that can arrive after an error storm.
        self.pending_notices.len()
            + self.live_notices.len()
            + self.printed_notices.len()
            + self.outstanding
            + usize::from(self.saving_defaults || self.defaults_task.is_some())
            + usize::from(self.bindings_task.is_some())
    }

    pub(super) fn record_notice(&mut self, text: impl Into<String>, level: NoticeLevel) -> bool {
        let text = notice_text(&text.into());
        if text.is_empty() {
            return false;
        }
        if level == NoticeLevel::Error {
            if self
                .pending_notices
                .iter()
                .chain(self.notice_in_flight.iter())
                .any(|(pending, pending_level)| pending == &text && *pending_level == level)
            {
                return true;
            }
            if self.notice_load() >= NOTICE_LIMIT {
                return false;
            }
        }
        if self.pending_notices.len() >= NOTICE_LIMIT {
            self.notice = Some(("wait for history".into(), Instant::now(), true));
            return false;
        }
        self.pending_notices.push_back((text, level));
        self.pending_categories.push_back(None);
        true
    }

    pub(super) fn record_action(
        &mut self,
        text: impl Into<String>,
        level: NoticeLevel,
        category: &'static str,
    ) -> bool {
        let text = text.into();
        // Identical failures in distinct action categories are separate receipts.
        if level == NoticeLevel::Error {
            let text = notice_text(&text);
            if text.is_empty() {
                return false;
            }
            if self
                .pending_notices
                .iter()
                .zip(&self.pending_categories)
                .any(|((pending, pending_level), pending_category)| {
                    pending == &text
                        && *pending_level == level
                        && *pending_category == Some(category)
                })
                || (self.notice_category == Some(category)
                    && self
                        .notice_in_flight
                        .as_ref()
                        .is_some_and(|(pending, pending_level)| {
                            pending == &text && *pending_level == level
                        }))
            {
                return true;
            }
            if self.notice_load() >= NOTICE_LIMIT {
                return false;
            }
            self.pending_notices.push_back((text, level));
            self.pending_categories.push_back(Some(category));
            return true;
        }
        let before = self.pending_notices.len();
        let accepted = self.record_notice(text, level);
        if self.pending_notices.len() > before {
            *self.pending_categories.back_mut().unwrap() = Some(category);
        }
        accepted
    }

    pub(super) fn flush_notices(&mut self, tx: &mpsc::Sender<Work>) {
        if self.notice_busy
            || self.outstanding >= OUTSTANDING
            || self.live_notices.len() + self.printed_notices.len() >= NOTICE_LIMIT
        {
            return;
        }
        let Some((text, level)) = self.pending_notices.front() else {
            return;
        };
        let command = match self.pending_categories.front().copied().flatten() {
            Some(category) => Command::RecordAction {
                text: text.clone(),
                level: *level,
                category: category.into(),
            },
            None => Command::RecordNotice {
                text: text.clone(),
                level: *level,
            },
        };
        if tx
            .try_send(Work {
                op: Op::Notice,
                command,
            })
            .is_ok()
        {
            self.notice_in_flight = self.pending_notices.pop_front();
            self.notice_category = self.pending_categories.pop_front().flatten();
            self.notice_busy = true;
            self.outstanding += 1;
        }
    }

    pub(super) fn try_shutdown(&mut self, tx: &mpsc::Sender<Work>) -> bool {
        if !self.quit_pending {
            return false;
        }
        self.autosave(tx, true);
        self.flush_pending_edit(tx);
        self.flush_edit_release(tx);
        if self.release_edit.is_some() {
            return false;
        }
        if self.composer_busy
            || (!self.composer_error && self.composer_dirty())
            || self.bindings_task.is_some()
        {
            return false;
        }
        self.flush_notices(tx);
        if !self.pending_notices.is_empty()
            || self.notice_busy
            || !self.live_notices.is_empty()
            || self.native_notice.is_some()
            || self.native_sync_required
            || self
                .pending
                .values()
                .any(|entry| entry.kind == "notice" && !self.notice_printed(entry.seq))
            || self.defaults_task.is_some()
            || self.saving_defaults
            || self.outstanding != 0
        {
            return false;
        }
        if self.enqueue(tx, Op::Shutdown, Command::Shutdown) {
            self.quit_pending = false;
            return true;
        }
        false
    }

    fn request_quit(&mut self, tx: &mpsc::Sender<Work>) -> bool {
        if self.exiting {
            return true;
        }
        self.command_chain.clear();
        self.pending_send = None;
        if self.composer_error && !self.composer_ready {
            self.exit_error =
                Some("Composer restoration failed; startup input was not loaded".into());
        }
        if self.composer_error && self.composer_dirty() {
            self.composer_error = false;
        }
        self.autosave(tx, true);
        self.flush_pending_edit(tx);
        self.flush_edit_release(tx);
        if !self.composer_busy
            && !self.composer_dirty()
            && self.release_edit.is_none()
            && self.bindings_task.is_none()
            && self.pending_notices.is_empty()
            && !self.notice_busy
            && self.live_notices.is_empty()
            && self.native_notice.is_none()
            && !self.native_sync_required
            && !self
                .pending
                .values()
                .any(|entry| entry.kind == "notice" && !self.notice_printed(entry.seq))
            && self.defaults_task.is_none()
            && !self.saving_defaults
            && self.outstanding == 0
        {
            let accepted = self.enqueue(tx, Op::Shutdown, Command::Shutdown);
            self.exiting = accepted;
            return accepted;
        }
        self.exiting = true;
        self.quit_pending = true;
        true
    }

    pub(super) fn enqueue(&mut self, tx: &mpsc::Sender<Work>, op: Op, command: Command) -> bool {
        if self.outstanding >= OUTSTANDING {
            self.notify("Too many pending commands; wait", true);
            return false;
        }
        let op = if self.chain_execution {
            Op::Chain(Box::new(op))
        } else {
            op
        };
        match tx.try_send(Work { op, command }) {
            Ok(()) => {
                if self.chain_execution {
                    self.chain_pending += 1;
                }
                self.outstanding += 1;
                true
            }
            Err(_) => {
                self.notify("Command worker unavailable; nothing submitted", true);
                false
            }
        }
    }

    pub(super) fn history_request(&mut self, tx: &mpsc::Sender<Work>, load: Load) {
        if !self.history_busy
            && self.enqueue(tx, Op::History(load, false), self.history.command(load))
        {
            self.history_busy = true;
            // An explicit page supersedes an older background projection.
            self.history_refresh_stale = self.history_refresh_busy;
        }
    }

    pub(super) fn sync_history_browser(&mut self) {
        let initialized = self.history_browser.selection().is_some();
        self.history_browser.actions_oldest = self.actions_oldest;
        self.history_browser.sync(&self.history, &mut self.journal);
        let selected = match self.history_browser.selection() {
            Some(Selection::Action(id)) => self.journal.selected_action().map(|action| {
                (
                    id.clone(),
                    action.output.as_ref().map(|output| output.id.clone()),
                )
            }),
            _ => None,
        };
        if selected != self.browser_artifact {
            if initialized {
                self.preview_generation = self.preview_generation.wrapping_add(1);
            }
            if self.journal.preview_text.is_empty() {
                self.preview_start = 0;
            }
            self.browser_artifact = selected;
        }
    }

    pub(super) fn prepare_history(&mut self, width: u16, height: u16) {
        self.sync_history_browser();
        let body = render::history_height(height, self.full_history);
        self.history_preview_size = HistoryBrowser::preview_dimensions(width, body);
        self.journal.rewrap(self.history_preview_size.0 as usize);
        self.history_browser.prepare(width, body, &mut self.history);
    }

    pub(super) fn poll(&mut self, tx: &mpsc::Sender<Work>) {
        if self.exiting {
            return;
        }
        self.autosave(tx, false);
        self.flush_pending_edit(tx);
        self.flush_edit_release(tx);
        if self.view == View::History {
            self.sync_history_browser();
        }
        let query = self.editor().text().trim_start_matches('/');
        if !self.drafts_busy
            && !self.drafts_loaded
            && (self.view == View::Drafts
                || query.starts_with("send")
                || query.starts_with("draft delete")
                || query
                    .split_whitespace()
                    .next()
                    .is_some_and(|command| command.eq_ignore_ascii_case("bind"))
                || (self.view == View::Bindings && self.binding_key.is_some()))
        {
            self.drafts_busy = self.enqueue(tx, Op::Drafts, Command::Drafts);
        }
        if self.view == View::History && !self.actions_busy && self.actions_follow {
            self.load_actions(tx, ActionLoad::Latest);
        }
        if self.view == View::History {
            self.refresh_action(tx);
        }
        if self.view == View::History
            && !self.history_browser.chronological
            && matches!(self.history_browser.selection(), Some(Selection::Action(_)))
            && !self.journal.collapsed
            && self.journal.preview.is_empty()
            && !self.artifact_busy
            && !self.journal.eof
        {
            self.load_artifact(tx, 0, false);
        }
        if !self.full_history
            && !self.tail_busy
            && (self.pending.len() < self.page
                || self
                    .pending
                    .first_key_value()
                    .is_some_and(|(_, entry)| entry.status == "running"))
        {
            self.request_native_tail(tx, self.published);
        }
        if self.view == View::History
            && self.history.follows_tail()
            && !self.history_busy
            && !self.history_refresh_busy
            && self.enqueue(
                tx,
                Op::History(Load::Latest, true),
                self.history.command(Load::Latest),
            )
        {
            self.history_refresh_busy = true;
            self.history_refresh_stale = false;
        }
        if self.view == View::History
            && !self.history.follows_tail()
            && !self.history_busy
            && !self.history_refresh_busy
            && let Some(command) = self.history.refresh_command()
            && self.enqueue(tx, Op::HistoryRefresh, command)
        {
            self.history_refresh_busy = true;
            self.history_refresh_stale = false;
        }
        if !self.events_busy && self.outstanding < OUTSTANDING - 2 {
            let after = self.event_cursor;
            if self.enqueue(
                tx,
                Op::Events,
                Command::Events {
                    after,
                    limit: self.page,
                },
            ) {
                self.events_busy = true;
            }
        }
        self.last_poll = Instant::now();
    }

    pub(super) fn append(&mut self, text: &str) {
        self.stop_armed = None;
        if self.exiting || self.draft_busy || self.pending_send.is_some() {
            return;
        }
        if let Some(search) = &mut self.pager_search {
            let (text, truncated) =
                sanitized_graphemes(text, search.max_bytes().saturating_sub(search.text().len()));
            let inserted = search.insert(&text.replace('\n', " "));
            self.filter_completion();
            if truncated || inserted.truncated {
                self.notify("Pager search byte limit reached", true);
            }
            return;
        }
        self.recall_cursor = None;
        let remaining = self
            .editor()
            .max_bytes()
            .saturating_sub(self.editor().text().len());
        let (text, truncated) = sanitized_graphemes(text, remaining);
        let text = if self.palette && self.commands_focus {
            text.replace('\n', " ")
        } else {
            text
        };
        let inserted = self.editor_mut().insert(&text);
        self.reset_completion();
        if truncated || inserted.truncated {
            self.notify(
                format!("Draft limit: {} bytes", self.editor().max_bytes()),
                true,
            );
        }
    }

    pub(super) fn reset_completion(&mut self) {
        self.history_search_generation = self.history_search_generation.wrapping_add(1);
        self.history_search_busy = None;
        self.completion_selected = 0;
        self.completion_active = false;
        self.pager_selected = false;
        self.pager_search = None;
        self.completion_cache = None;
        self.completion_original = None;
        self.completion_hidden = false;
        self.notice = None;
    }

    pub(super) fn remember(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.inputs.push(text);
        if self.inputs.len() > 100 {
            self.inputs.remove(0);
        }
        self.recall_cursor = None;
    }

    pub(super) fn recall(&mut self, previous: bool) {
        if !self.interface.prompt_history
            || self.draft_busy
            || self.edit.is_some()
            || self.inputs.is_empty()
        {
            return;
        }
        let query = if self.recall_cursor.is_some() {
            &self.recall_draft
        } else {
            self.draft.text()
        }
        .to_lowercase();
        // Use the newest occurrence of each exact text in both directions.
        let matches = |index: &usize| {
            self.inputs[*index].to_lowercase().contains(&query)
                && !self.inputs[*index + 1..].contains(&self.inputs[*index])
        };
        let index = match (self.recall_cursor, previous) {
            (cursor, true) => (0..cursor.unwrap_or(self.inputs.len())).rev().find(matches),
            (Some(index), false) => (index + 1..self.inputs.len()).find(matches),
            (None, false) => return,
        };
        if previous && index.is_none() {
            return;
        }
        if self.recall_cursor.is_none() {
            self.recall_draft = self.draft.text().to_owned();
            self.recall_draft_cursor = self.draft.cursor();
        }
        let text = index.map_or_else(
            || self.recall_draft.clone(),
            |index| self.inputs[index].clone(),
        );
        self.draft.set_text(&text);
        if index.is_none() {
            self.draft.set_cursor(self.recall_draft_cursor);
        }
        self.recall_cursor = index;
        self.reset_completion();
        self.completion_hidden = true;
        self.notice = None;
    }

    pub(super) fn ctrl_c(&mut self, now: Instant, snapshot: &Snapshot, tx: &mpsc::Sender<Work>) {
        self.stop_armed = None;
        if self.exiting {
            return;
        }
        if self
            .exit_armed
            .is_some_and(|at| now.duration_since(at) < EXIT_WINDOW)
        {
            self.request_quit(tx);
            self.exit_armed = None;
            return;
        }
        let closing_view = self.view != View::Root || self.palette || self.pending_send.is_some();
        if self.pending_send.take().is_some() {
            self.command_chain.clear();
            self.chain_wait = false;
        }
        self.view = View::Root;
        self.full_history = false;
        self.panel_returns.clear();
        self.settings_editor = None;
        self.help_detail = None;
        self.palette = self.palette_pinned;
        if !self.saving_defaults || !self.defaults_palette {
            self.palette_query.clear();
        }
        self.reset_completion();
        self.completion_hidden = true;
        if !closing_view
            && snapshot.active.is_some()
            && let Err(error) = self.execute(Action::Interrupt, "", false, snapshot, tx)
        {
            self.notify(format!("{error:#}"), true);
        }
        self.exit_armed = Some(now);
    }

    pub(super) fn publish_root(&mut self, snapshot: &Snapshot, screen: &mut Screen) -> Result<()> {
        let size = screen.inline.size()?;
        let plan = self.plan_publication(snapshot, size.width as usize);
        let height = render::publication_height(self, snapshot, size.width, size.height, &plan);
        // The insertion must own the post-state height, otherwise shrinking after
        // a bottom-edge insertion scrolls more native rows than the handoff needs.
        screen.set_height(height)?;
        if let Some(rows) = &plan.rows {
            screen.publish_rows(rows)?;
        }
        self.commit_publication(plan);
        Ok(())
    }

    fn request_native_tail(&mut self, tx: &mpsc::Sender<Work>, after: Option<i64>) {
        if !self.full_history
            && !self.tail_busy
            && self.outstanding < OUTSTANDING - 2
            && self.enqueue(
                tx,
                Op::Tail,
                Command::NativeTranscript {
                    before: None,
                    after,
                    limit: self.page,
                },
            )
        {
            self.tail_busy = true;
            self.native_tail_stale = false;
        }
    }

    pub(super) fn sync_native(&mut self, tx: &mpsc::Sender<Work>) {
        if self.native_sync_required {
            self.request_native_tail(tx, self.published);
        } else if let Some(receipt) = self.live_notices.front()
            && !self.receipt_ready(receipt.seq)
            && self
                .pending
                .first_key_value()
                .is_none_or(|(_, entry)| entry.status == "running")
        {
            let (through, blocked) = self.receipt_position(receipt.seq);
            // Refresh a stale running head when known later records block the receipt;
            // otherwise page beyond the proven running/printed prefix to fill the gap.
            self.request_native_tail(
                tx,
                if blocked {
                    self.published
                } else {
                    Some(through)
                },
            );
        }
    }

    fn receipt_position(&self, seq: i64) -> (i64, bool) {
        let mut through = self.published.unwrap_or(0);
        let mut known: Vec<_> = self
            .printed_notices
            .iter()
            .map(|&(start, end)| (start, end, false))
            .chain(self.pending.range(..seq).map(|(&seq, entry)| {
                (
                    seq,
                    seq,
                    !(entry.kind == "assistant" && entry.status == "running")
                        && !(entry.kind == "notice" && self.notice_printed(seq)),
                )
            }))
            .collect();
        known.sort_unstable_by_key(|&(start, _, _)| start);
        for (start, end, blocked) in known {
            if end <= through {
                continue;
            }
            if start > through.saturating_add(1) {
                break;
            }
            if blocked {
                return (through, true);
            }
            through = through.max(end);
        }
        (through, false)
    }

    fn receipt_ready(&self, seq: i64) -> bool {
        if seq <= self.published.unwrap_or(0) {
            return true;
        }
        let (through, blocked) = self.receipt_position(seq);
        !self.native_sync_required && !blocked && through >= seq - 1
    }

    fn native_notice_rows(entry: &Saved, width: usize, last: bool) -> Vec<Line<'static>> {
        let style = match entry.status.as_str() {
            "error" => super::theme::warning(),
            "status" => super::theme::muted(),
            _ => super::theme::notice(),
        };
        super::transcript::wrapped(
            &clean(&entry.text, MAX_NOTICE_BYTES),
            width.saturating_sub(4).max(1),
        )
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::styled(
                format!(
                    "  {}{}",
                    if index != 0 {
                        if last { "  " } else { "\u{2502} " }
                    } else if last {
                        "\u{2514} "
                    } else {
                        "\u{251c} "
                    },
                    row
                ),
                style,
            )
        })
        .collect()
    }

    pub(super) fn native_tail_rows(&self, width: usize) -> Vec<Line<'static>> {
        self.native_notice.as_ref().map_or_else(Vec::new, |entry| {
            Self::native_notice_rows(entry, width, true)
        })
    }

    fn plan_notice(
        &self,
        entry: Saved,
        width: usize,
        live: bool,
        ordered: bool,
    ) -> PublicationPlan {
        let category = self
            .action_categories
            .get(&entry.seq)
            .copied()
            .unwrap_or("events");
        let continuing = self.native_group == Some((entry.seq - 1, category));
        let mut rows = self.native_notice.as_ref().map_or_else(Vec::new, |held| {
            Self::native_notice_rows(held, width, !continuing)
        });
        if !continuing {
            let label = match category {
                "settings" => "Settings",
                "queue" => "Queue",
                "drafts" => "Drafts",
                "bindings" => "Bindings",
                "delivery" => "Delivery",
                "error" => "Errors",
                _ => "Events",
            };
            rows.push(Line::from(""));
            rows.push(Line::styled(
                format!("\u{25cf} {label}"),
                super::theme::notice(),
            ));
        }
        PublicationPlan {
            rows: Some(rows),
            change: PublicationChange::Notice {
                entry,
                category,
                live,
                ordered,
            },
        }
    }

    #[cfg(test)]
    pub(super) fn publish_with(
        &mut self,
        snapshot: &Snapshot,
        width: usize,
        mut publish: impl FnMut(&[Line<'static>]) -> Result<()>,
    ) -> Result<()> {
        self.observe_live(snapshot);
        let plan = self.plan_publication(snapshot, width);
        if let Some(rows) = &plan.rows {
            publish(rows)?;
        }
        self.commit_publication(plan);
        Ok(())
    }

    pub(super) fn plan_publication(&self, snapshot: &Snapshot, width: usize) -> PublicationPlan {
        let empty = || PublicationPlan {
            rows: None,
            change: PublicationChange::None,
        };
        if let Some(entry) = self.live_notices.front()
            && self.receipt_ready(entry.seq)
        {
            return self.plan_notice(entry.clone(), width, true, false);
        }
        if self.native_sync_required {
            return empty();
        }
        if self.exiting && self.native_notice.is_some() {
            return PublicationPlan {
                rows: Some(self.native_tail_rows(width)),
                change: PublicationChange::FlushNotice,
            };
        }
        let Some((&seq, entry)) = self.pending.first_key_value() else {
            return empty();
        };
        if entry.kind == "notice"
            && let Some(&(_, through)) = self
                .printed_notices
                .iter()
                .find(|(start, end)| *start <= seq && seq <= *end)
        {
            return PublicationPlan {
                rows: None,
                change: PublicationChange::SkipNotices { through },
            };
        }
        if entry.kind == "notice" {
            return self.plan_notice(entry.clone(), width, false, true);
        }
        if entry.status == "running"
            && self.printed_notices.len() < NOTICE_LIMIT
            && let Some((_, notice)) = self
                .pending
                .iter()
                .find(|(seq, entry)| entry.kind == "notice" && !self.notice_printed(**seq))
        {
            return self.plan_notice(notice.clone(), width, false, false);
        }
        let running = entry.status == "running";
        let text = if running {
            snapshot
                .active
                .as_ref()
                .filter(|active| active.attempt_id == entry.attempt_id)
                .map_or(entry.text.as_str(), |active| active.text.as_str())
        } else {
            entry.text.as_str()
        };
        let Some((end, last)) = native_end(text, self.native_offset, running) else {
            return empty();
        };
        let mut rows = transcript_rows(entry, &text[self.native_offset..end], last, width);
        if last {
            rows.pop();
        }
        if self.native_separator {
            rows.insert(0, Line::from(""));
        }
        rows.splice(0..0, self.native_tail_rows(width));
        PublicationPlan {
            rows: Some(rows),
            change: PublicationChange::Text {
                seq,
                end,
                last,
                attempt: (entry.kind == "assistant").then(|| entry.attempt_id.clone()),
                user_attempt: (entry.kind == "user").then(|| entry.attempt_id.clone()),
            },
        }
    }

    pub(super) fn commit_publication(&mut self, plan: PublicationPlan) {
        match plan.change {
            PublicationChange::None => {}
            PublicationChange::Notice {
                entry,
                category,
                live,
                ordered,
            } => {
                self.native_group = Some((entry.seq, category));
                self.action_categories.remove(&entry.seq);
                self.mark_notice(entry.seq);
                if live {
                    self.live_notices.pop_front();
                }
                if ordered {
                    self.pending.remove(&entry.seq);
                    self.published = Some(self.published.unwrap_or(0).max(entry.seq));
                    if !self.notice_busy {
                        self.printed_notices.retain(|(_, end)| *end > entry.seq);
                    }
                }
                self.native_notice = Some(entry);
                self.native_separator = true;
            }
            PublicationChange::FlushNotice => {
                self.native_notice = None;
                self.native_group = None;
            }
            PublicationChange::SkipNotices { through } => {
                self.pending.retain(|seq, _| *seq > through);
                self.action_categories.retain(|seq, _| *seq > through);
                self.published = Some(self.published.unwrap_or(0).max(through));
                if !self.notice_busy {
                    self.printed_notices.retain(|(_, end)| *end > through);
                }
            }
            PublicationChange::Text {
                seq,
                end,
                last,
                attempt,
                user_attempt,
            } => {
                self.native_notice = None;
                self.native_group = None;
                self.native_separator = last;
                self.native_offset = end;
                if let Some(id) = user_attempt {
                    self.accepted_previews.retain(|(attempt, _)| *attempt != id);
                }
                if let Some(id) = attempt
                    && let Some(held) = self.live_attempts.iter_mut().find(|held| held.id == id)
                {
                    held.offset = end;
                    held.complete = last;
                }
                if last {
                    self.pending.remove(&seq);
                    self.published = Some(seq);
                    if !self.notice_busy {
                        self.printed_notices.retain(|(_, end)| *end > seq);
                    }
                    self.native_offset = 0;
                }
            }
        }
    }

    pub(super) fn observe_live(&mut self, snapshot: &Snapshot) {
        if self.native_fullscreen != self.full_history {
            self.native_fullscreen = self.full_history;
            self.native_sync_required = true;
            self.native_tail_stale |= self.tail_busy;
        }
        let active = snapshot
            .active
            .as_ref()
            .map(|active| active.attempt_id.as_str());
        if active != self.observed_active.as_deref()
            && let Some(attempt) = active
            && let Some(front) = &self.observed_front
            && snapshot
                .active
                .as_ref()
                .is_some_and(|active| active.message_id == front.id)
            && !snapshot.queue.iter().any(|item| item.id == front.id)
        {
            // Only the authoritative started message can retire its queued card.
            // A cancelled card cannot be mistaken for an unrelated coalesced start.
            self.accepted_previews
                .push_back((attempt.to_owned(), self.observed_front.take().unwrap()));
            while self.accepted_previews.len() > 2 {
                self.accepted_previews.pop_front();
            }
        }
        self.observed_active = active.map(str::to_owned);
        self.observed_front = snapshot
            .queue
            .iter()
            .find(|item| item.state == crate::runtime::QueueState::Queued)
            .map(|item| {
                let (text, truncated) = sanitized(&item.text, 4096);
                QueuePreview {
                    id: item.id.clone(),
                    text,
                    truncated,
                }
            });
        let Some(active) = &snapshot.active else {
            return;
        };
        if !self
            .live_attempts
            .iter()
            .any(|held| held.id == active.attempt_id)
        {
            self.live_attempts.push_back(LiveAttempt {
                id: active.attempt_id.clone(),
                text: String::new(),
                start: 0,
                offset: 0,
                complete: false,
            });
            // Keep the current and retiring attempt, including completion tombstones
            // until a newer snapshot makes replay of the old active view impossible.
            while self.live_attempts.len() > 2 {
                self.live_attempts.pop_front();
            }
        }
        let held = self
            .live_attempts
            .iter_mut()
            .find(|held| held.id == active.attempt_id)
            .unwrap();
        if !held.complete {
            let text = super::transcript::tail(&active.text, super::ACTIVE_BYTES);
            held.start = active.text.len() - text.len();
            held.text = text.to_owned();
        }
    }

    pub(super) fn live_projection<'a>(
        &'a self,
        plan: Option<&'a PublicationPlan>,
    ) -> LiveProjection<'a> {
        let mut projection = LiveProjection {
            notice: self.native_notice.as_ref(),
            advance: None,
            through: None,
            retire_user: None,
        };
        if let Some(plan) = plan {
            match &plan.change {
                PublicationChange::Notice { entry, ordered, .. } => {
                    projection.notice = Some(entry);
                    if *ordered {
                        projection.through = Some(entry.seq);
                    }
                }
                PublicationChange::FlushNotice => projection.notice = None,
                PublicationChange::Text {
                    seq,
                    end,
                    last,
                    attempt,
                    user_attempt,
                } => {
                    projection.notice = None;
                    projection.advance = attempt.as_deref().map(|id| (id, *end, *last));
                    projection.retire_user = user_attempt.as_deref();
                    if *last {
                        projection.through = Some(*seq);
                    }
                }
                PublicationChange::SkipNotices { through } => projection.through = Some(*through),
                _ => {}
            }
        }
        projection
    }

    pub(super) fn projected_notice_rows(
        projection: &LiveProjection<'_>,
        width: usize,
    ) -> Vec<Line<'static>> {
        projection.notice.map_or_else(Vec::new, |entry| {
            Self::native_notice_rows(entry, width, true)
        })
    }

    pub(super) fn queue_texts<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        projection: &LiveProjection<'_>,
    ) -> Vec<(&'a str, bool)> {
        self.accepted_previews
            .iter()
            .filter(|(attempt, _)| projection.retire_user != Some(attempt.as_str()))
            .map(|(_, preview)| (preview.text.as_str(), preview.truncated))
            .chain(
                snapshot
                    .queue
                    .iter()
                    .filter(|item| item.state == crate::runtime::QueueState::Queued)
                    .map(|item| (item.text.as_str(), false)),
            )
            .collect()
    }

    pub(super) fn live_texts<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        projection: &LiveProjection<'_>,
    ) -> Vec<&'a str> {
        let mut sources = Vec::new();
        let head = self
            .pending
            .iter()
            .find(|(seq, _)| projection.through.is_none_or(|through| **seq > through));
        for held in &self.live_attempts {
            let saved = self
                .pending
                .values()
                .find(|entry| entry.kind == "assistant" && entry.attempt_id == held.id);
            let active = snapshot
                .active
                .as_ref()
                .filter(|active| active.attempt_id == held.id);
            // Final durable text wins over a lagging active snapshot. While the
            // final poll is pending, retain the observed (possibly newer) suffix.
            let (text, start) = if let Some(saved) = saved.filter(|entry| entry.status != "running")
            {
                (saved.text.as_str(), 0)
            } else if let Some(active) = active {
                (active.text.as_str(), 0)
            } else if let Some(saved) =
                saved.filter(|entry| entry.text.len() > held.start + held.text.len())
            {
                (saved.text.as_str(), 0)
            } else {
                (held.text.as_str(), held.start)
            };
            sources.push((held.id.as_str(), text, start, held.offset, held.complete));
        }
        if let Some((_, entry)) = head
            && entry.kind == "assistant"
            && !sources.iter().any(|(id, ..)| *id == entry.attempt_id)
        {
            // The native backlog precedes the observed retiring/current tails.
            sources.insert(
                0,
                (entry.attempt_id.as_str(), entry.text.as_str(), 0, 0, false),
            );
        }
        if let Some(active) = &snapshot.active
            && !sources.iter().any(|(id, ..)| *id == active.attempt_id)
        {
            sources.push((
                active.attempt_id.as_str(),
                active.text.as_str(),
                0,
                0,
                false,
            ));
        }
        let mut texts: Vec<_> = sources
            .into_iter()
            .filter_map(|(id, text, start, mut offset, mut complete)| {
                if projection.through.is_none()
                    && head.is_some_and(|(_, entry)| {
                        entry.kind == "assistant" && entry.attempt_id == id
                    })
                {
                    offset = offset.max(self.native_offset);
                }
                if let Some((attempt, end, last)) = projection.advance
                    && attempt == id
                {
                    offset = end;
                    complete = last;
                }
                if complete {
                    return None;
                }
                text.get(offset.saturating_sub(start).min(text.len())..)
                    .filter(|text| !text.is_empty())
            })
            .collect();
        // Tombstones suppress replay but must not consume an eligible live slot.
        texts.drain(..texts.len().saturating_sub(2));
        texts
    }

    fn notice_printed(&self, seq: i64) -> bool {
        seq <= self.published.unwrap_or(0)
            || self
                .printed_notices
                .iter()
                .any(|(start, end)| *start <= seq && seq <= *end)
    }

    fn mark_notice(&mut self, seq: i64) {
        if let Some((_, end)) = self.printed_notices.back_mut()
            && *end + 1 == seq
        {
            *end = seq;
        } else {
            self.printed_notices.push_back((seq, seq));
        }
    }

    pub(super) fn editor(&self) -> &Editor {
        if self.view == View::Bindings && self.binding_key.is_some() {
            &self.binding_commands
        } else if self.palette && self.commands_focus {
            &self.palette_query
        } else {
            &self.draft
        }
    }

    fn palette_has_exact_arguments(&self) -> bool {
        let mut tokens = self.palette_query.text().split_whitespace();
        let Some(token) = tokens.next() else {
            return false;
        };
        tokens.next().is_some()
            && commands::COMMANDS.iter().any(|spec| {
                spec.name[1..].eq_ignore_ascii_case(token.strip_prefix('/').unwrap_or(token))
            })
    }

    fn editor_mut(&mut self) -> &mut Editor {
        if self.view == View::Bindings && self.binding_key.is_some() {
            &mut self.binding_commands
        } else if self.palette && self.commands_focus {
            &mut self.palette_query
        } else {
            &mut self.draft
        }
    }

    pub(super) fn completion_visible(&self) -> bool {
        !self.exiting
            && if self.view == View::Bindings {
                self.binding_key.is_some()
                    && !self.binding_capture
                    && self.bindings_task.is_none()
                    && self.completion_active
            } else {
                !self.draft_busy
                    && self.view == View::Root
                    && ((self.palette && self.commands_focus) || self.completion_active)
            }
    }

    pub(super) fn binding_completion(&self, snapshot: &Snapshot) -> Option<completion::Completion> {
        if self.exiting
            || self.view != View::Bindings
            || self.binding_capture
            || self.bindings_task.is_some()
        {
            return None;
        }
        let key = self.binding_key.as_ref()?;
        if self.completion_active {
            return self.cached_completion();
        }
        let prefix = format!("/bind {key} ");
        let source = format!("{prefix}{}", self.binding_commands.text());
        let mut menu = completion::build_with_keys(
            &source,
            prefix.len() + self.binding_commands.cursor(),
            &self.interface,
            snapshot,
            &self.drafts,
            &self.keys,
        )?;
        if menu.range.start < prefix.len() || menu.range.end < prefix.len() {
            return None;
        }
        menu.range.start -= prefix.len();
        menu.range.end -= prefix.len();
        Some(self.attach_history(menu, snapshot))
    }

    pub(super) fn completion(&self, snapshot: &Snapshot) -> Option<completion::Completion> {
        if self.view == View::Bindings {
            return self.binding_completion(snapshot);
        }
        if self.exiting
            || self.draft_busy
            || self.view != View::Root
            || (!(self.palette && self.commands_focus)
                && (self.edit.is_some() || self.completion_hidden))
        {
            return None;
        }
        if self.completion_active {
            return self.cached_completion();
        }
        let editor = self.editor();
        if self.palette && self.commands_focus && !editor.text().starts_with('/') {
            let source = format!("/{}", editor.text());
            let mut result = completion::build_with_keys(
                &source,
                editor.cursor() + 1,
                &self.interface,
                snapshot,
                &self.drafts,
                &self.keys,
            )?;
            result.range.start = result.range.start.saturating_sub(1);
            result.range.end = result.range.end.saturating_sub(1);
            Some(self.attach_history(result, snapshot))
        } else {
            completion::build_with_keys(
                editor.text(),
                editor.cursor(),
                &self.interface,
                snapshot,
                &self.drafts,
                &self.keys,
            )
            .map(|menu| self.attach_history(menu, snapshot))
        }
    }

    fn attach_history(
        &self,
        menu: completion::Completion,
        snapshot: &Snapshot,
    ) -> completion::Completion {
        match &self.history_search_cache {
            Some((query, entries, cursor)) if *cursor == snapshot.cursor => {
                completion::attach_history(
                    menu,
                    self.editor().text(),
                    self.editor().cursor(),
                    query,
                    entries,
                )
            }
            _ => menu,
        }
    }

    fn completion_context(&self) -> (View, bool, bool, Option<String>) {
        (
            self.view,
            self.palette,
            self.commands_focus,
            self.binding_key.clone(),
        )
    }

    fn start_history_completion(
        &mut self,
        snapshot: &Snapshot,
        search: bool,
        reverse: bool,
        tx: &mpsc::Sender<Work>,
    ) -> Result<()> {
        if self.history_search_busy.is_some() {
            return Ok(());
        }
        if let Some(menu) = self.completion(snapshot)
            && let Some(query) =
                completion::history_query(&menu, self.editor().text(), self.editor().cursor())
            && self
                .history_search_cache
                .as_ref()
                .is_none_or(|(cached, _, cursor)| cached != &query || *cursor != snapshot.cursor)
        {
            self.reset_completion();
            let generation = self.history_search_generation;
            let source = self.editor().text().to_owned();
            let cursor = self.editor().cursor();
            let request = HistorySearch {
                generation,
                query: query.clone(),
                source: source.clone(),
                cursor,
                context: self.completion_context(),
                menu,
                search,
                reverse,
                journal_cursor: snapshot.cursor,
            };
            if self.enqueue(
                tx,
                Op::HistorySearch(request),
                Command::SearchHistory {
                    query,
                    limit: crate::runtime::MAX_HISTORY_MATCHES,
                },
            ) {
                self.history_search_busy = Some(generation);
                self.completion_original = Some((source, cursor));
            }
            return Ok(());
        }
        self.start_completion(snapshot, search)?;
        if reverse && self.view == View::Bindings && self.completion_active {
            self.navigate_completion(true, 1)?;
        }
        Ok(())
    }

    fn cached_completion(&self) -> Option<completion::Completion> {
        let mut menu = self.completion_cache.clone()?;
        if let Some(search) = &self.pager_search {
            let query = search.text().to_lowercase();
            menu.items.retain(|item| {
                let text =
                    format!("{} {} {}", item.label, item.value, item.description).to_lowercase();
                query.split_whitespace().all(|word| text.contains(word))
            });
        }
        Some(menu)
    }

    fn accept_completion(&mut self, snapshot: &Snapshot) -> Result<()> {
        let completion = self
            .completion(snapshot)
            .context("No matching completion")?;
        let item = completion
            .items
            .get(
                self.completion_selected
                    .min(completion.items.len().saturating_sub(1)),
            )
            .context("No matching completion")?;
        self.replace_completion(completion.range, &item.value, item.trailing_space)?;
        self.reset_completion();
        Ok(())
    }

    fn replace_completion(
        &mut self,
        range: std::ops::Range<usize>,
        value: &str,
        space: bool,
    ) -> Result<()> {
        let (original, cursor) = self
            .completion_original
            .clone()
            .unwrap_or_else(|| (self.editor().text().to_owned(), self.editor().cursor()));
        let mut text = value.to_owned();
        if space && !original[range.end..].starts_with(char::is_whitespace) {
            text.push(' ');
        }
        let resulting_bytes = original.len() - range.len() + text.len();
        anyhow::ensure!(
            resulting_bytes <= self.editor().max_bytes(),
            "Completed argument exceeds the input byte limit"
        );
        // Validate against the original range, never a previously previewed token.
        let mut editor = Editor::new(self.editor().max_bytes());
        editor.set_text(&original);
        editor.set_cursor(cursor);
        editor
            .replace(range, &text)
            .context("Completion is not at a grapheme boundary")?;
        *self.editor_mut() = editor;
        Ok(())
    }

    fn restore_completion(&mut self) {
        if let Some((text, cursor)) = self.completion_original.clone() {
            self.editor_mut().set_text(&text);
            self.editor_mut().set_cursor(cursor);
        }
    }

    fn start_completion(&mut self, snapshot: &Snapshot, search: bool) -> Result<()> {
        self.reset_completion();
        let Some(menu) = self.completion(snapshot) else {
            return Ok(());
        };
        self.open_completion(menu, search)
    }

    fn open_completion(&mut self, menu: completion::Completion, search: bool) -> Result<()> {
        if menu.items.is_empty() {
            return Ok(());
        }
        self.completion_original = Some((self.editor().text().to_owned(), self.editor().cursor()));
        if search {
            self.completion_active = true;
            self.pager_search = Some(Editor::new(256));
        } else if menu.items.len() == 1 {
            let item = &menu.items[0];
            self.replace_completion(menu.range.clone(), &item.value, item.trailing_space)?;
            if self.view == View::Bindings || menu.stage == completion::Stage::History {
                self.completion_active = true;
                self.completion_cache = Some(menu);
            }
            return Ok(());
        } else {
            let first = &menu.items[0].value;
            let prefix = first
                .char_indices()
                .map(|(byte, ch)| byte + ch.len_utf8())
                .take_while(|&end| {
                    menu.items
                        .iter()
                        .all(|item| item.value.starts_with(&first[..end]))
                })
                .last()
                .unwrap_or(0);
            let token = &self.editor().text()[menu.range.start..self.editor().cursor()];
            if prefix > token.len() && first[..prefix].starts_with(token) {
                self.replace_completion(menu.range.clone(), &first[..prefix], false)?;
            }
            self.completion_active = true;
        }
        self.completion_cache = Some(menu);
        Ok(())
    }

    fn navigate_completion(&mut self, reverse: bool, step: usize) -> Result<()> {
        let Some(menu) = self.cached_completion() else {
            return Ok(());
        };
        let count = menu.items.len();
        if count == 0 {
            return Ok(());
        }
        let selected = if !self.pager_selected {
            if reverse { count - 1 } else { 0 }
        } else if reverse {
            (self.completion_selected + count - step % count) % count
        } else {
            (self.completion_selected + step) % count
        };
        let item = &menu.items[selected];
        self.replace_completion(menu.range, &item.value, item.trailing_space)?;
        self.completion_selected = selected;
        self.pager_selected = true;
        Ok(())
    }

    fn filter_completion(&mut self) {
        self.pager_selected = false;
        self.completion_selected = 0;
        self.restore_completion();
        if let Err(error) = self.navigate_completion(false, 1) {
            self.notify(format!("{error:#}"), true);
        }
    }

    fn accept_suggestion(&mut self, snapshot: &Snapshot, word: bool) -> Result<bool> {
        if self.completion_active || self.editor().cursor() != self.editor().text().len() {
            return Ok(false);
        }
        let Some(menu) = self.completion(snapshot) else {
            return Ok(false);
        };
        let Some(suffix) = menu.ghost(self.editor().text(), self.editor().cursor(), 0) else {
            return Ok(false);
        };
        let suffix = if word {
            use unicode_segmentation::UnicodeSegmentation;
            let end = suffix
                .unicode_word_indices()
                .next()
                .map_or(suffix.len(), |(byte, value)| byte + value.len());
            &suffix[..end]
        } else {
            suffix
        };
        anyhow::ensure!(
            self.editor().text().len() + suffix.len() <= self.editor().max_bytes(),
            "Suggested argument exceeds the input byte limit"
        );
        self.editor_mut().insert(suffix);
        self.reset_completion();
        Ok(true)
    }

    fn apply_setting(&mut self, args: &str) -> Result<()> {
        let result = (|| {
            anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
            anyhow::ensure!(
                !self.saving_defaults
                    && self.defaults_task.is_none()
                    && self.bindings_task.is_none(),
                "Wait for the pending preferences save"
            );
            let tokens = args.split_whitespace().collect::<Vec<_>>();
            let (args, save) = match tokens.as_slice() {
                [key, value] => ([*key, *value], false),
                [key, value, "--default"] => ([*key, *value], true),
                _ => anyhow::bail!("Usage: /settings <key> <value> [--default]"),
            };
            let change = crate::settings::Change::parse(&args)?;
            if save {
                anyhow::ensure!(
                    !self.preferences_path.as_os_str().is_empty(),
                    "No user preferences path available"
                );
                let path = self.preferences_path.clone();
                self.defaults_task = Some(tokio::task::spawn_blocking(move || {
                    crate::settings::save_change(&path, &change)?;
                    Ok(change)
                }));
                self.saving_defaults = true;
                self.defaults_palette = self.palette && self.commands_focus;
                self.draft_busy = true;
            } else {
                self.finish_setting(change, false, self.palette && self.commands_focus);
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.settings_feedback = Some((format!("Setting not applied: {error:#}"), true));
        }
        result
    }

    pub(super) fn finish_setting(
        &mut self,
        change: crate::settings::Change,
        saved: bool,
        palette: bool,
    ) {
        let prior = crate::settings::catalog(&self.interface)
            .into_iter()
            .find(|setting| setting.key == change.key())
            .expect("validated setting has a catalog entry");
        change.apply(&mut self.interface);
        let display = |value: &str| {
            match value {
                "true" => "on",
                "false" => "off",
                value => value,
            }
            .to_owned()
        };
        self.settings_feedback = Some((
            format!(
                "{}: {}{}{}",
                prior.label,
                display(&prior.value),
                if prior.value == change.value() {
                    " (unchanged)".to_owned()
                } else {
                    format!(" -> {}", display(change.value()))
                },
                if saved {
                    " (saved for future clients)"
                } else {
                    ""
                },
            ),
            false,
        ));
        self.setting_sources
            .insert(change.key(), "This client".into());
        let canonical = format!(
            "/settings {} {}{}",
            change.key(),
            change.value(),
            if saved { " --default" } else { "" }
        );
        self.remember(canonical);
        if !palette && self.defaults_clear {
            self.draft.clear();
        }
        if !self.defaults_chain {
            self.palette = self.palette_pinned;
            self.palette_query.clear();
        }
        self.reset_completion();
        let outcome = format!(
            "{} {}{}",
            change.key(),
            match change.value() {
                "true" => "enabled".to_owned(),
                "false" => "disabled".to_owned(),
                value => format!("set to {value}"),
            },
            if saved { " (default)" } else { "" }
        );
        self.record_action(outcome.clone(), NoticeLevel::Info, "settings");
        self.notify(outcome, false);
    }

    pub(super) fn complete_defaults(
        &mut self,
        result: Result<crate::settings::Change>,
    ) -> Result<()> {
        self.saving_defaults = false;
        // Panel saves persist a snapshot, never reapply values or consume newer input.
        if std::mem::take(&mut self.defaults_panel) {
            return match result {
                Ok(_) => {
                    self.settings_feedback =
                        Some(("UI defaults saved for future clients".into(), false));
                    self.record_action("UI defaults saved", NoticeLevel::Info, "settings");
                    self.notify("UI defaults saved for future clients", false);
                    Ok(())
                }
                Err(error) => {
                    let text = format!("Defaults not confirmed; client unchanged: {error:#}");
                    self.settings_feedback = Some((text.clone(), true));
                    self.record_action(format!("error: {text}"), NoticeLevel::Error, "settings");
                    self.notify(text, true);
                    if self.exiting {
                        self.exit_error = Some(format!("UI defaults not saved: {error:#}"));
                    }
                    Err(error.context("user defaults save not confirmed"))
                }
            };
        }
        self.chain_wait = false;
        self.draft_busy = false;
        match result {
            Ok(change) => {
                self.finish_setting(change, true, self.defaults_palette);
                Ok(())
            }
            Err(error) => {
                self.command_chain.clear();
                if self.defaults_palette && !self.exiting {
                    self.palette = true;
                    self.view = View::Root;
                    self.full_history = false;
                }
                let text = format!(
                    "Default not confirmed; client unchanged: {}",
                    clean(&format!("{error:#}"), 1024)
                );
                self.settings_feedback = Some((text.clone(), true));
                self.record_action(format!("error: {text}"), NoticeLevel::Error, "settings");
                self.notify(text, true);
                let error = error.context("user default save not confirmed");
                if self.exiting {
                    self.exit_error = Some(format!("{error:#}"));
                }
                Err(error)
            }
        }
    }

    pub(super) async fn finish_pending_defaults(&mut self) -> Result<()> {
        if let Some(task) = self.defaults_task.take() {
            let result = task
                .await
                .context("preferences worker failed")
                .and_then(|result| result);
            self.complete_defaults(result)?;
        }
        Ok(())
    }

    fn push_panel_parent(&mut self, target: View) {
        if target == self.view
            || !(matches!(self.view, View::Help | View::Settings)
                || matches!(target, View::Help | View::Settings))
        {
            return;
        }
        self.panel_returns.push(PanelReturn {
            view: self.view,
            selected: self.selected,
            info_scroll: self.info_scroll,
            palette: self.palette,
            pinned: self.palette_pinned,
            commands_focus: self.commands_focus,
            query: self.palette_query.text().to_owned(),
            cursor: self.palette_query.cursor(),
            help: (self.view == View::Help).then(|| HelpReturn {
                query: self.help_query.text().to_owned(),
                cursor: self.help_query.cursor(),
                category: self.help_category,
                selected: self.help_selected,
                detail: self.help_detail,
                detail_selected: self.help_detail_selected,
            }),
            settings: (self.view == View::Settings)
                .then_some((self.settings_selected, self.settings_editor)),
        });
    }

    fn return_panel(&mut self) {
        self.reset_completion();
        if let Some(parent) = self.panel_returns.pop() {
            self.view = parent.view;
            self.selected = parent.selected;
            self.info_scroll = parent.info_scroll;
            self.palette = parent.palette;
            self.palette_pinned = parent.pinned;
            self.commands_focus = parent.commands_focus;
            self.palette_query.set_text(&parent.query);
            self.palette_query.set_cursor(parent.cursor);
            if let Some(help) = parent.help {
                self.help_query.set_text(&help.query);
                self.help_query.set_cursor(help.cursor);
                self.help_category = help.category;
                self.help_selected = help.selected;
                self.help_detail = help.detail;
                self.help_detail_selected = help.detail_selected;
            }
            if let Some((selected, editor)) = parent.settings {
                self.settings_selected = selected;
                self.settings_editor = editor;
            }
        } else {
            self.view = View::Root;
        }
        self.full_history = self.view == View::History;
        self.last_poll = Instant::now() - Duration::from_secs(1);
    }

    pub(super) fn help_commands(&self) -> Vec<&'static commands::Spec> {
        let query = self
            .help_query
            .text()
            .trim()
            .trim_start_matches('/')
            .to_ascii_lowercase();
        commands::COMMANDS
            .iter()
            .filter(|spec| {
                (self.help_category == 0 || spec.category() == HELP_CATEGORIES[self.help_category])
                    && (spec.name.to_ascii_lowercase().contains(&query)
                        || spec.description.to_ascii_lowercase().contains(&query)
                        || spec.category().to_ascii_lowercase().contains(&query))
            })
            .collect()
    }

    fn settings_key_event(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
    ) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        let catalog = crate::settings::catalog(&self.interface);
        if key.code == KeyCode::Esc {
            if self.settings_editor.take().is_none() {
                self.return_panel();
            }
            return Ok(());
        }
        if let Some(choice) = self.settings_editor {
            let setting = &catalog[self.settings_selected];
            match key.code {
                KeyCode::Up | KeyCode::Left => {
                    self.settings_editor = Some(choice.saturating_sub(1))
                }
                KeyCode::Down | KeyCode::Right => {
                    self.settings_editor = Some((choice + 1).min(setting.choices.len() - 1))
                }
                KeyCode::Enter => {
                    // Reuse command validation and receipts without consuming either field.
                    self.defaults_clear = false;
                    self.defaults_chain = true;
                    self.apply_setting(&format!("{} {}", setting.key, setting.choices[choice]))?;
                    self.settings_editor = None;
                }
                _ => {}
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Up => self.settings_selected = self.settings_selected.saturating_sub(1),
            KeyCode::Down => {
                self.settings_selected = (self.settings_selected + 1).min(catalog.len() + 1)
            }
            KeyCode::Char(' ') if self.settings_selected < catalog.len() => {
                let setting = &catalog[self.settings_selected];
                if setting.choices == ["true", "false"] {
                    self.defaults_clear = false;
                    self.defaults_chain = true;
                    self.apply_setting(&format!(
                        "{} {}",
                        setting.key,
                        if setting.value == "true" {
                            "false"
                        } else {
                            "true"
                        }
                    ))?;
                }
            }
            KeyCode::Enter if self.settings_selected < catalog.len() => {
                let setting = &catalog[self.settings_selected];
                self.settings_editor = Some(
                    setting
                        .choices
                        .iter()
                        .position(|value| *value == setting.value)
                        .unwrap_or(0),
                );
            }
            KeyCode::Enter if self.settings_selected == catalog.len() => {
                let result: Result<()> = (|| {
                    anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
                    anyhow::ensure!(
                        !self.saving_defaults
                            && self.defaults_task.is_none()
                            && self.bindings_task.is_none(),
                        "Wait for the pending preferences save"
                    );
                    anyhow::ensure!(
                        !self.preferences_path.as_os_str().is_empty(),
                        "No user preferences path available"
                    );
                    let interface = self.interface.clone();
                    let path = self.preferences_path.clone();
                    self.defaults_panel = true;
                    self.defaults_task = Some(tokio::task::spawn_blocking(move || {
                        crate::settings::save_interface(&path, &interface)?;
                        Ok(crate::settings::Change::StatusLine(interface.status_line))
                    }));
                    self.saving_defaults = true;
                    self.defaults_palette = false;
                    Ok(())
                })();
                if let Err(error) = &result {
                    self.settings_feedback = Some((format!("Defaults not saved: {error:#}"), true));
                }
                result?;
            }
            KeyCode::Enter => {
                self.execute(Action::Bindings, "", false, snapshot, tx)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn help_key_event(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
    ) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if let Some(spec) = self.help_detail {
            match key.code {
                KeyCode::Esc => self.help_detail = None,
                KeyCode::Up | KeyCode::Left => self.help_detail_selected = 0,
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => self.help_detail_selected = 1,
                KeyCode::Enter if self.help_detail_selected == 1 => {
                    self.execute(Action::Commands, "", false, snapshot, tx)?;
                    self.palette_query.set_text(&format!(
                        "{}{}",
                        spec.name,
                        if spec.takes_arguments() { " " } else { "" }
                    ));
                }
                KeyCode::Enter => {
                    commands::parse(spec.name)
                        .context("This command needs arguments; choose Insert")?;
                    if self.execute(spec.action, "", false, snapshot, tx)? {
                        self.help_detail = None;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        let count = self.help_commands().len();
        match key.code {
            KeyCode::Esc => self.return_panel(),
            KeyCode::Up => self.help_selected = self.help_selected.saturating_sub(1),
            KeyCode::Down => {
                self.help_selected = (self.help_selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::PageUp => self.help_selected = self.help_selected.saturating_sub(10),
            KeyCode::PageDown => {
                self.help_selected = (self.help_selected + 10).min(count.saturating_sub(1))
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let reverse =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.help_category = (self.help_category
                    + if reverse {
                        HELP_CATEGORIES.len() - 1
                    } else {
                        1
                    })
                    % HELP_CATEGORIES.len();
                self.help_selected = 0;
            }
            KeyCode::Enter => {
                if let Some(spec) = self.help_commands().get(self.help_selected).copied() {
                    if key.modifiers.contains(KeyModifiers::ALT) {
                        self.execute(Action::Commands, "", false, snapshot, tx)?;
                        self.palette_query.set_text(&format!(
                            "{}{}",
                            spec.name,
                            if spec.takes_arguments() { " " } else { "" }
                        ));
                    } else if spec.opens_panel() {
                        self.execute(
                            spec.action,
                            if spec.action == Action::History {
                                "fullscreen"
                            } else {
                                ""
                            },
                            false,
                            snapshot,
                            tx,
                        )?;
                    } else {
                        self.help_detail = Some(spec);
                        self.help_detail_selected = 1;
                    }
                }
            }
            KeyCode::Backspace => {
                self.help_query.backspace();
                self.help_selected = 0;
            }
            KeyCode::Delete => {
                self.help_query.delete();
                self.help_selected = 0;
            }
            KeyCode::Left => {
                self.help_query.left();
            }
            KeyCode::Right => {
                self.help_query.right();
            }
            KeyCode::Home => {
                self.help_query.home();
            }
            KeyCode::End => {
                self.help_query.end();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control() =>
            {
                self.help_query.insert(&ch.to_string());
                self.help_selected = 0;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
        width: u16,
        height: u16,
    ) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let plain_escape = key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.view == View::Root
            && !self.full_history
            && !self.palette
            && self.panel_returns.is_empty()
            && self.edit.is_none()
            && !self.draft_busy
            && self.pending_send.is_none()
            && self.completion_original.is_none()
            && !self.completion_active
            && self.history_search_busy.is_none()
            && self.completion(snapshot).is_none();
        if !plain_escape {
            self.stop_armed = None;
        }
        if self.exiting || (self.view == View::Root && self.draft_busy) {
            return Ok(());
        }
        if plain_escape {
            let now = Instant::now();
            if self
                .stop_armed
                .take()
                .is_some_and(|at| now.duration_since(at) <= Duration::from_millis(700))
            {
                self.execute(Action::Stop, "", false, snapshot, tx)?;
            } else if !self.stop_pending {
                self.stop_armed = Some(now);
            }
            return Ok(());
        }
        if let Some(pending) = &mut self.pending_send {
            match key.code {
                KeyCode::Left | KeyCode::Up => {
                    pending.selected = pending.selected.saturating_sub(1)
                }
                KeyCode::Right | KeyCode::Down => pending.selected = (pending.selected + 1).min(2),
                KeyCode::Esc => {
                    self.pending_send = None;
                    self.command_chain.clear();
                    self.chain_wait = false;
                }
                KeyCode::Enter => {
                    self.ensure_send_ready()?;
                    let mut pending = self.pending_send.take().unwrap();
                    if pending.selected == 2 {
                        self.command_chain.clear();
                        self.chain_wait = false;
                        return Ok(());
                    }
                    if pending.session != snapshot.session_id
                        || !snapshot.paused
                        || pending.text != self.draft.text()
                        || pending.cursor != self.draft.cursor()
                    {
                        self.command_chain.clear();
                        self.chain_wait = false;
                        anyhow::bail!("Input or queue state changed; send cancelled");
                    }
                    match &mut pending.command {
                        Command::SubmitInput { resume, .. }
                        | Command::SubmitInputAccumulated { resume, .. }
                        | Command::DispatchDraftChecked { resume, .. } => {
                            *resume = pending.selected == 1
                        }
                        _ => unreachable!(),
                    }
                    self.chain_execution = self.chain_wait;
                    self.draft_busy = self.enqueue(
                        tx,
                        Op::Dispatch {
                            clear: pending.clear,
                            text: pending.text,
                            action: pending.action,
                            composer_revision: pending.composer_revision,
                            draft_id: pending.draft_id,
                        },
                        pending.command,
                    );
                    self.chain_execution = false;
                    if !self.draft_busy {
                        self.command_chain.clear();
                        self.chain_wait = false;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if self.view == View::Bindings {
            return self.binding_key_event(key, snapshot, tx, width, height);
        }
        if self.view == View::Settings {
            return self.settings_key_event(key, snapshot, tx);
        }
        if self.view == View::Help {
            return self.help_key_event(key, snapshot, tx);
        }
        if key.code == KeyCode::Esc && self.view != View::Root && self.view != View::History {
            self.return_panel();
            return Ok(());
        }
        if self.view == View::History {
            self.prepare_history(width, height);
            let body = render::history_height(height, self.full_history);
            let code = match key.code {
                KeyCode::Right if self.history_browser.focus == HistoryFocus::List => KeyCode::Tab,
                KeyCode::Left if self.history_browser.focus == HistoryFocus::Preview => {
                    KeyCode::BackTab
                }
                code => code,
            };
            if matches!(
                code,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            ) && self.history_browser.focus == HistoryFocus::List
                && self.history_busy
                && (self.history_browser.chronological
                    || matches!(
                        self.history_browser.selection(),
                        Some(Selection::Message(_))
                    ))
            {
                return Ok(());
            }
            if matches!(code, KeyCode::Up | KeyCode::PageUp)
                && self.history_browser.focus == HistoryFocus::List
                && matches!(self.history_browser.selection(), Some(Selection::Action(_)))
            {
                self.actions_follow = false;
            }
            let effect =
                self.history_browser
                    .key(code, width, body, &mut self.history, &mut self.journal);
            self.sync_history_browser();
            match effect {
                Effect::None => {}
                Effect::Close => self.return_panel(),
                Effect::LoadMessages(load) => self.history_request(tx, load),
                Effect::LoadActions { older } => self.load_actions(
                    tx,
                    if older {
                        ActionLoad::Older
                    } else {
                        ActionLoad::Newer
                    },
                ),
                Effect::ToggleArtifact => self.journal.collapsed = !self.journal.collapsed,
                Effect::SelectionChanged => {
                    if let Some(Selection::Message(seq)) = self.history_browser.selection() {
                        self.history.anchor(*seq);
                    }
                }
                Effect::ArtifactScroll(delta) if !self.journal.collapsed => {
                    let height = self.history_preview_size.1.saturating_sub(2).max(1) as usize;
                    let last = self.journal.preview.len().saturating_sub(height);
                    let before = self.journal.preview_offset.min(last);
                    let next = before.saturating_add_signed(delta).min(last);
                    self.journal.preview_offset = next;
                    if delta < 0
                        && next == 0
                        && before < delta.unsigned_abs()
                        && self.preview_start > 0
                    {
                        self.load_artifact(tx, self.preview_start, true);
                    } else if delta > 0
                        && next == last
                        && delta as usize > last.saturating_sub(before)
                        && !self.journal.eof
                    {
                        self.load_artifact(tx, self.journal.next_offset, false);
                    }
                }
                Effect::ArtifactScroll(_) => {}
            }
            if !self.history_browser.chronological
                && self.history_browser.focus == HistoryFocus::Preview
                && let Some(Selection::Message(seq)) = self.history_browser.selection()
            {
                self.history.anchor(*seq);
            }
            if self.view == View::History
                && !self.history_browser.chronological
                && matches!(self.history_browser.selection(), Some(Selection::Action(_)))
                && self.journal.preview_text.is_empty()
                && !self.journal.eof
                && !self.journal.collapsed
            {
                self.load_artifact(tx, 0, false);
            }
            return Ok(());
        }
        if self.view == View::Drafts {
            match key.code {
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down => {
                    self.selected = (self.selected + 1).min(self.drafts.len().saturating_sub(1))
                }
                KeyCode::Delete => {
                    anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
                    anyhow::ensure!(
                        !self.draft_busy && !self.drafts_busy,
                        "Wait for the pending draft operation"
                    );
                    let draft = self
                        .drafts
                        .get(self.selected)
                        .context("No draft selected")?
                        .clone();
                    self.drafts_busy = self.enqueue(
                        tx,
                        Op::DeleteDraft {
                            id: draft.id.clone(),
                            clear: false,
                        },
                        Command::DeleteDraft {
                            id: draft.id,
                            revision: draft.revision,
                        },
                    );
                    self.draft_busy = self.drafts_busy;
                }
                KeyCode::Enter => {
                    anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
                    anyhow::ensure!(
                        !self.draft_busy && self.edit.is_none(),
                        "Finish your queued edit first"
                    );
                    let draft = self
                        .drafts
                        .get(self.selected)
                        .context("No draft selected")?
                        .clone();
                    anyhow::ensure!(
                        draft.text.len() <= self.message_limit.min(self.draft.max_bytes()),
                        "Saved draft exceeds the configured byte limit; composer unchanged"
                    );
                    if self.draft.text().trim().is_empty() {
                        self.draft
                            .set_text(&sanitized(&draft.text, self.message_limit).0);
                        self.view = View::Root;
                        self.full_history = false;
                        self.reset_completion();
                    } else {
                        self.draft_busy = self.enqueue(
                            tx,
                            Op::LoadDraft(draft),
                            Command::SaveDraft {
                                text: self.draft.text().to_owned(),
                            },
                        );
                    }
                }
                KeyCode::Esc => {
                    self.view = View::Root;
                    self.full_history = false;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.view == View::Root {
            if key.code == KeyCode::Esc
                && (self.completion_original.is_some()
                    || self.completion_active
                    || self.history_search_busy.is_some())
            {
                self.restore_completion();
                self.reset_completion();
                return Ok(());
            }
            if key.code == KeyCode::Esc && !self.panel_returns.is_empty() {
                self.return_panel();
                return Ok(());
            }
            if key.code == KeyCode::Esc && self.palette {
                self.palette = false;
                self.palette_pinned = false;
                self.commands_focus = true;
                self.palette_query.clear();
                self.reset_completion();
                return Ok(());
            }
            let reverse = key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
            let search = ctrl && matches!(key.code, KeyCode::Char('s' | 'S'));
            if key.code == KeyCode::Tab || reverse || search {
                if !self.completion_active {
                    self.start_history_completion(snapshot, reverse || search, reverse, tx)?;
                } else if search {
                    if self.pager_search.is_some() {
                        self.pager_search = None;
                        self.pager_selected = false;
                        self.completion_selected = 0;
                        self.restore_completion();
                    } else {
                        self.pager_search = Some(Editor::new(256));
                    }
                } else {
                    self.navigate_completion(reverse, 1)?;
                }
                return Ok(());
            }
        }
        if self.completion_visible() && self.completion_active {
            match key.code {
                KeyCode::Enter if !alt && !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    if self.pager_selected {
                        anyhow::ensure!(
                            render::selection_visible(self, snapshot, width, height),
                            "Enlarge the terminal to read the selected completion"
                        );
                    }
                    self.reset_completion();
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    let reverse = matches!(key.code, KeyCode::Up | KeyCode::PageUp);
                    let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        6
                    } else {
                        1
                    };
                    self.navigate_completion(reverse, step)?;
                    return Ok(());
                }
                KeyCode::Left | KeyCode::Right
                    if !shift && self.pager_selected && self.pager_search.is_none() =>
                {
                    self.navigate_completion(key.code == KeyCode::Left, 1)?;
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.pager_search.is_some() {
            let search = self.pager_search.as_mut().unwrap();
            match key.code {
                KeyCode::Char(ch) if !ctrl && !alt && !ch.is_control() => {
                    self.append(&ch.to_string());
                    return Ok(());
                }
                KeyCode::Backspace => {
                    search.backspace();
                }
                KeyCode::Delete => {
                    search.delete();
                }
                KeyCode::Left => {
                    search.left();
                    return Ok(());
                }
                KeyCode::Right => {
                    search.right();
                    return Ok(());
                }
                KeyCode::Home => {
                    search.home();
                    return Ok(());
                }
                KeyCode::End => {
                    search.end();
                    return Ok(());
                }
                _ => return Ok(()),
            }
            self.filter_completion();
            return Ok(());
        }
        match key.code {
            KeyCode::Esc if self.view != View::Root => {
                self.view = View::Root;
                self.full_history = false;
                self.last_poll = Instant::now() - Duration::from_secs(1);
            }
            KeyCode::Esc if self.edit.is_some() => {
                self.execute(Action::CancelEdit, "", false, snapshot, tx)?;
            }
            KeyCode::Up if self.view == View::Queue => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Down if self.view == View::Queue => {
                self.selected = (self.selected + 1).min(snapshot.queue.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::PageUp if matches!(self.view, View::Help | View::Details) => {
                self.info_scroll =
                    self.info_scroll
                        .saturating_sub(if key.code == KeyCode::Up { 1 } else { 10 });
            }
            KeyCode::Down | KeyCode::PageDown
                if matches!(self.view, View::Help | View::Details) =>
            {
                self.info_scroll = self
                    .info_scroll
                    .saturating_add(if key.code == KeyCode::Down { 1 } else { 10 });
            }
            KeyCode::Char('e') | KeyCode::Enter
                if self.view == View::Queue && !ctrl && !self.draft_busy =>
            {
                self.execute(Action::Edit, "", false, snapshot, tx)?;
            }
            KeyCode::Delete if self.view == View::Queue && !self.draft_busy => {
                self.execute(Action::Cancel, "", false, snapshot, tx)?;
            }
            _ if self.view != View::Root || self.draft_busy => {}
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.append("\n")
            }
            KeyCode::Enter => {
                if self.palette
                    && self.commands_focus
                    && !self.palette_has_exact_arguments()
                    && self
                        .completion(snapshot)
                        .is_some_and(|menu| menu.stage == completion::Stage::Command)
                {
                    anyhow::ensure!(
                        render::selection_visible(self, snapshot, width, height),
                        "Enlarge the terminal to read the selected completion"
                    );
                }
                self.submit(snapshot, tx)?;
            }
            KeyCode::Up | KeyCode::Down => {
                let moved = if key.code == KeyCode::Up {
                    self.editor_mut().up(width.saturating_sub(2))
                } else {
                    self.editor_mut().down(width.saturating_sub(2))
                };
                if !moved && !(self.palette && self.commands_focus) {
                    if key.code == KeyCode::Down && self.edit.is_some() {
                        self.execute(Action::CancelEdit, "", false, snapshot, tx)?;
                    } else if key.code == KeyCode::Up
                        && self.edit.is_none()
                        && let Some(item) = snapshot
                            .queue
                            .iter()
                            .rev()
                            .find(|item| item.state == crate::runtime::QueueState::Queued)
                    {
                        self.begin_edit(item.clone(), tx)?;
                    } else {
                        self.recall(key.code == KeyCode::Up);
                    }
                } else {
                    self.reset_completion();
                }
            }
            KeyCode::Left => {
                if shift || ctrl || alt {
                    self.editor_mut().word_left();
                } else {
                    self.editor_mut().left();
                }
                self.reset_completion();
            }
            KeyCode::Right => {
                if !shift && self.accept_suggestion(snapshot, ctrl || alt)? {
                    return Ok(());
                }
                if shift || ctrl || alt {
                    self.editor_mut().word_right();
                } else {
                    self.editor_mut().right();
                }
                self.reset_completion();
            }
            KeyCode::Home => {
                if ctrl {
                    self.editor_mut().set_cursor(0);
                } else {
                    self.editor_mut().home();
                }
                self.reset_completion();
            }
            KeyCode::End => {
                if !ctrl && self.accept_suggestion(snapshot, false)? {
                    return Ok(());
                }
                if ctrl {
                    let end = self.editor().text().len();
                    self.editor_mut().set_cursor(end);
                } else {
                    self.editor_mut().end();
                }
                self.reset_completion();
            }
            KeyCode::Backspace => {
                if ctrl || alt {
                    self.editor_mut().delete_word_left();
                } else {
                    self.editor_mut().backspace();
                }
                self.edited();
            }
            KeyCode::Delete => {
                if ctrl || alt {
                    self.editor_mut().delete_word_right();
                } else {
                    self.editor_mut().delete();
                }
                self.edited();
            }
            KeyCode::Char(ch) if ctrl => match ch.to_ascii_lowercase() {
                'a' => {
                    self.editor_mut().home();
                    self.reset_completion();
                }
                'e' => {
                    if self.accept_suggestion(snapshot, false)? {
                        return Ok(());
                    }
                    self.editor_mut().end();
                    self.reset_completion();
                }
                'f' => {
                    if !self.accept_suggestion(snapshot, false)? {
                        self.editor_mut().right();
                        self.reset_completion();
                    }
                }
                'w' => {
                    self.editor_mut().delete_word_left();
                    self.edited();
                }
                'k' => {
                    self.editor_mut().delete_to_line_end();
                    self.edited();
                }
                _ => {}
            },
            KeyCode::Char('b' | 'f') if alt => {
                if key.code == KeyCode::Char('b') {
                    self.editor_mut().word_left();
                } else {
                    if self.accept_suggestion(snapshot, true)? {
                        return Ok(());
                    }
                    self.editor_mut().word_right();
                }
                self.reset_completion();
            }
            KeyCode::Char('u') if alt => {
                self.editor_mut().delete_to_line_start();
                self.edited();
            }
            KeyCode::Char(ch) if !alt && !ch.is_control() => self.append(&ch.to_string()),
            _ => {}
        }
        Ok(())
    }

    fn edited(&mut self) {
        self.recall_cursor = None;
        self.reset_completion();
    }

    pub(super) fn target(
        &self,
        prefix: &str,
        snapshot: &Snapshot,
    ) -> anyhow::Result<crate::runtime::QueueItem> {
        let prefix = prefix.to_ascii_lowercase();
        anyhow::ensure!(
            prefix.len() >= 4
                && prefix.len() <= 36
                && prefix.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-'),
            "Use at least four characters of a message ID from /queue"
        );
        let mut targets = snapshot
            .queue
            .iter()
            .filter(|item| item.id.starts_with(&prefix));
        let target = targets
            .next()
            .context("No queued message has that ID prefix")?;
        anyhow::ensure!(
            targets.next().is_none(),
            "Ambiguous message ID prefix; use more characters"
        );
        Ok(target.clone())
    }

    pub(super) fn shortcut(
        &mut self,
        action: Action,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
    ) -> Result<()> {
        self.stop_armed = None;
        if action == Action::ClearDraft && self.view == View::Help && self.help_detail.is_none() {
            self.help_query.clear();
            self.help_selected = 0;
            self.reset_completion();
        } else if action == Action::ClearDraft
            && self.view == View::Root
            && self.palette
            && self.commands_focus
            && !self.draft_busy
        {
            self.palette_query.clear();
            self.reset_completion();
        } else {
            self.execute(action, "", false, snapshot, tx)?;
        }
        Ok(())
    }

    pub(super) fn execute(
        &mut self,
        action: Action,
        args: &str,
        from_text: bool,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
    ) -> anyhow::Result<bool> {
        self.stop_armed = None;
        if self.exiting && action != Action::Quit {
            return Ok(false);
        }
        anyhow::ensure!(
            self.composer_ready || !self.draft_busy || action == Action::Quit,
            "Wait for composer restoration or input acknowledgement"
        );
        if matches!(
            action,
            Action::Mode
                | Action::Draft
                | Action::Send
                | Action::ForceSend
                | Action::Bind
                | Action::Unbind
        ) {
            anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
        }
        match action {
            Action::FocusInput => {
                if self.palette_pinned {
                    self.commands_focus = !self.commands_focus;
                    self.reset_completion();
                }
                return Ok(true);
            }
            Action::Exit => {
                self.palette = false;
                self.palette_pinned = false;
                self.commands_focus = true;
                if from_text {
                    self.draft.clear();
                }
                self.palette_query.clear();
                self.reset_completion();
                return Ok(true);
            }
            Action::Mode => {
                self.mode = match args {
                    "steer" => InputMode::Steer,
                    "after" => InputMode::After,
                    "draft" => InputMode::Draft,
                    _ => anyhow::bail!("Usage: /mode steer|after|draft"),
                };
                if from_text {
                    self.draft.clear();
                }
                self.palette_query.clear();
                self.palette = self.palette_pinned;
                self.reset_completion();
                self.record_action(
                    format!("delivery mode set to {args}"),
                    NoticeLevel::Info,
                    "settings",
                );
                return Ok(true);
            }
            Action::Drafts => {
                self.push_panel_parent(View::Drafts);
                self.view = View::Drafts;
                self.full_history = false;
                self.selected = 0;
                self.drafts_busy = self.enqueue(tx, Op::Drafts, Command::Drafts);
                if from_text {
                    self.draft.clear();
                }
                self.palette_query.clear();
                self.palette = self.palette_pinned;
                return Ok(true);
            }
            Action::Draft => {
                anyhow::ensure!(
                    !self.draft_busy && self.edit.is_none(),
                    "Wait for input or finish your queued edit"
                );
                let tokens = args.split_whitespace().collect::<Vec<_>>();
                let accepted = match tokens.as_slice() {
                    ["save"] => {
                        anyhow::ensure!(
                            !from_text,
                            "Use the secondary command field to save the primary composer"
                        );
                        let text = self.draft.text().to_owned();
                        anyhow::ensure!(!text.trim().is_empty(), "Composer is empty");
                        self.enqueue(
                            tx,
                            Op::SaveDraft {
                                text: text.clone(),
                                clear: true,
                            },
                            Command::SaveDraft { text },
                        )
                    }
                    ["delete", prefix] => {
                        let draft = self.draft_target(prefix)?;
                        self.enqueue(
                            tx,
                            Op::DeleteDraft {
                                id: draft.id.clone(),
                                clear: from_text,
                            },
                            Command::DeleteDraft {
                                id: draft.id,
                                revision: draft.revision,
                            },
                        )
                    }
                    _ => anyhow::bail!("Usage: /draft save|delete <id>"),
                };
                self.draft_busy = accepted;
                return Ok(accepted);
            }
            Action::Send | Action::ForceSend => {
                anyhow::ensure!(!self.draft_busy, "Wait for input acknowledgement");
                anyhow::ensure!(self.edit.is_none(), "Finish your queued edit first");
                self.ensure_send_ready()?;
                let mode = if action == Action::ForceSend {
                    DeliveryMode::Force
                } else if args.contains("--after") {
                    DeliveryMode::After
                } else if args.contains("--force") {
                    DeliveryMode::Force
                } else {
                    DeliveryMode::Steer
                };
                let explicit =
                    action == Action::Send || args.contains("--run") || args.contains("--hold");
                let resume = args.contains("--run");
                let command = if action == Action::ForceSend {
                    anyhow::ensure!(
                        !from_text && self.edit.is_none(),
                        "Force send uses the primary composer, not a queued edit"
                    );
                    anyhow::ensure!(!self.draft.text().trim().is_empty(), "Composer is empty");
                    self.input_command(mode, resume, snapshot)
                } else {
                    let draft = self.draft_target(
                        args.split_whitespace()
                            .next()
                            .context("Usage: /send <draft-id>")?,
                    )?;
                    anyhow::ensure!(
                        args.split_whitespace().count() == 1,
                        "Usage: /send <draft-id>"
                    );
                    Command::DispatchDraftAccumulated {
                        id: draft.id,
                        revision: draft.revision,
                        expected_paused: snapshot.paused,
                    }
                };
                return Ok(self.send_command(
                    command,
                    from_text || action == Action::ForceSend,
                    snapshot,
                    tx,
                    explicit,
                ));
            }
            Action::Bindings => {
                self.push_panel_parent(View::Bindings);
                self.view = View::Bindings;
                self.full_history = false;
                self.selected = 0;
                self.binding_capture = false;
                self.binding_key = None;
                self.reset_completion();
                if from_text {
                    self.draft.clear();
                }
                self.palette_query.clear();
                return Ok(true);
            }
            Action::Bind | Action::Unbind => {
                self.change_binding(action, args)?;
                if from_text {
                    if self.bindings_task.is_some() {
                        self.binding_clear = Some(self.draft.text().to_owned());
                        self.draft_busy = true;
                    } else {
                        self.draft.clear();
                    }
                }
                return Ok(true);
            }
            Action::Queue if !args.is_empty() => {
                return self.execute(
                    if args == "run" {
                        Action::Resume
                    } else if args == "pause" {
                        Action::Pause
                    } else {
                        anyhow::bail!("Usage: /queue [run|pause]")
                    },
                    "",
                    from_text,
                    snapshot,
                    tx,
                );
            }
            _ => {}
        }
        let was_palette = self.palette && self.commands_focus;
        if action == Action::Settings {
            anyhow::ensure!(!self.draft_busy, "Wait until input is acknowledged");
            commands::parse(&format!("/settings {args}"))?;
            if args.trim().is_empty() {
                self.push_panel_parent(View::Settings);
                self.view = View::Settings;
                self.full_history = false;
                self.settings_editor = None;
                if from_text {
                    self.draft.clear();
                }
                self.reset_completion();
                self.palette = self.palette_pinned;
                return Ok(true);
            }
            if args.split_whitespace().count() < 2 {
                let text = format!("/settings {args} ");
                if !from_text && !self.palette {
                    self.palette = true;
                }
                if !from_text {
                    self.commands_focus = true;
                }
                self.editor_mut().set_text(&text);
                self.view = View::Root;
                self.full_history = false;
                self.reset_completion();
                return Ok(true);
            }
            self.defaults_clear = from_text;
            self.defaults_chain = self.chain_execution;
            self.apply_setting(args)?;
            return Ok(true);
        }
        let history_load = if action == Action::History {
            commands::parse(&format!("/history {args}"))?;
            if args.is_empty() || args == "fullscreen" {
                Load::Latest
            } else {
                anyhow::ensure!(
                    !self.history_busy,
                    "Wait for the pending history page before opening an anchor"
                );
                Load::Anchor(args.parse()?)
            }
        } else {
            Load::Latest
        };
        let navigation = match action {
            Action::Help => Some(View::Help),
            Action::History => Some(View::History),
            Action::Queue => Some(View::Queue),
            Action::Details => Some(View::Details),
            _ => None,
        };
        if let Some(view) = navigation {
            if view == View::History
                && self.view == View::History
                && !from_text
                && matches!(history_load, Load::Latest)
            {
                self.return_panel();
                return Ok(true);
            }
            if view == View::History {
                anyhow::ensure!(!self.history_busy, "Wait for the pending history page");
            }
            self.push_panel_parent(view);
            self.view = view;
            if view == View::Queue {
                self.selected = self.selected.min(snapshot.queue.len().saturating_sub(1));
            }
            self.full_history = view == View::History;
            self.info_scroll = 0;
            if view == View::History {
                self.history_browser.focus = HistoryFocus::List;
                self.history_browser.chronological = false;
                self.history_request(tx, history_load);
                self.sync_history_browser();
                if self.actions_follow {
                    self.load_actions(tx, ActionLoad::Latest);
                }
            }
            if from_text {
                self.draft.clear();
            }
            self.reset_completion();
            self.palette = self.palette_pinned;
            if !self.saving_defaults || !self.defaults_palette {
                self.palette_query.clear();
            }
            if was_palette {
                self.completion_hidden = true;
                self.completion_selected = 0;
            }
            self.notice = None;
            return Ok(true);
        }
        let mut clear_now = from_text;
        if matches!(
            action,
            Action::CancelEdit
                | Action::Edit
                | Action::Cancel
                | Action::Pause
                | Action::Resume
                | Action::TogglePause
                | Action::Interrupt
                | Action::Stop
        ) {
            anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
        }
        let accepted = match action {
            Action::Commands => {
                anyhow::ensure!(
                    !self.saving_defaults,
                    "Wait for the pending preferences save"
                );
                self.push_panel_parent(View::Root);
                self.view = View::Root;
                self.full_history = false;
                if from_text {
                    self.draft.clear();
                }
                self.palette = true;
                self.palette_pinned = args == "keep" || self.palette_pinned;
                self.commands_focus = true;
                self.palette_query.clear();
                self.reset_completion();
                self.completion_hidden = !self.palette;
                self.notice = None;
                return Ok(true);
            }
            Action::Quit => self.request_quit(tx),
            Action::ClearDraft => {
                anyhow::ensure!(!self.draft_busy, "Wait until input is acknowledged");
                self.draft.clear();
                true
            }
            Action::CancelEdit => {
                anyhow::ensure!(!self.draft_busy, "Wait until input is acknowledged");
                let (id, revision) = self.edit.clone().context("No active edit lease")?;
                let accepted =
                    self.enqueue(tx, Op::CancelEdit, Command::CancelEdit { id, revision });
                self.draft_busy = accepted;
                if accepted {
                    self.reset_completion();
                }
                return Ok(accepted);
            }
            Action::Edit | Action::Cancel => {
                anyhow::ensure!(!self.draft_busy, "Wait until input is acknowledged");
                let item = if args.is_empty() {
                    anyhow::ensure!(
                        self.view == View::Queue,
                        "Usage: /edit or /cancel <message-id-prefix>; see /queue"
                    );
                    snapshot
                        .queue
                        .get(self.selected)
                        .context("Nothing selected in queue")?
                        .clone()
                } else {
                    self.target(args, snapshot)?
                };
                let editing_target = self.edit.as_ref().is_some_and(|(id, _)| id == &item.id);
                let accepted = if action == Action::Edit {
                    self.begin_edit(item, tx)?
                } else {
                    self.enqueue(
                        tx,
                        Op::Cancel {
                            id: item.id.clone(),
                            clear_draft: from_text,
                        },
                        Command::Cancel {
                            id: item.id,
                            revision: item.revision,
                        },
                    )
                };
                if accepted && (from_text || action == Action::Edit || editing_target) {
                    self.draft_busy = true;
                }
                clear_now = false;
                accepted
            }
            Action::Stop => {
                anyhow::ensure!(!self.stop_pending, "Wait for the stop acknowledgement");
                let accepted = self.enqueue(
                    tx,
                    Op::Stop {
                        clear_draft: from_text,
                    },
                    Command::StopPending {
                        expected_attempt_id: snapshot
                            .active
                            .as_ref()
                            .map(|active| active.attempt_id.clone()),
                        expected_paused: snapshot.paused,
                    },
                );
                if accepted {
                    self.stop_pending = true;
                    self.draft_busy |= from_text;
                }
                clear_now = false;
                accepted
            }
            Action::Pause | Action::Resume | Action::TogglePause | Action::Interrupt => {
                let pause = matches!(action, Action::Pause | Action::Resume | Action::TogglePause);
                anyhow::ensure!(!self.stop_pending, "Wait for the stop acknowledgement");
                anyhow::ensure!(
                    !pause || !self.pause_pending,
                    "Wait for the pending pause/resume acknowledgement"
                );
                let command = match action {
                    Action::Pause => Command::Pause,
                    Action::Resume => Command::Resume,
                    Action::TogglePause => {
                        if snapshot.paused {
                            Command::Resume
                        } else {
                            Command::Pause
                        }
                    }
                    Action::Interrupt => Command::Interrupt {
                        attempt_id: snapshot
                            .active
                            .as_ref()
                            .context("No active response")?
                            .attempt_id
                            .clone(),
                    },
                    _ => unreachable!(),
                };
                let outcome = match &command {
                    Command::Pause => "paused",
                    Command::Resume => "resumed",
                    Command::Interrupt { .. } => "interrupted",
                    _ => unreachable!(),
                };
                let accepted = self.enqueue(
                    tx,
                    Op::Control {
                        clear_draft: from_text,
                        pause,
                        outcome,
                    },
                    command,
                );
                if accepted {
                    self.pause_pending |= pause;
                    self.draft_busy |= from_text;
                }
                clear_now = false;
                accepted
            }
            _ => unreachable!(),
        };
        if accepted {
            if clear_now {
                self.draft.clear();
            }
            self.reset_completion();
            self.palette = self.palette_pinned;
            if !self.saving_defaults || !self.defaults_palette {
                self.palette_query.clear();
            }
            if was_palette {
                self.completion_hidden = true;
                self.completion_selected = 0;
            }
            self.notice = None;
        }
        Ok(accepted)
    }

    pub(super) fn submit(
        &mut self,
        snapshot: &Snapshot,
        tx: &mpsc::Sender<Work>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.draft_busy, "Wait until pending input is acknowledged");
        if self.completion_active || self.history_search_busy.is_some() {
            self.reset_completion();
            return Ok(());
        }
        if self.palette
            && self.commands_focus
            && !self.palette_has_exact_arguments()
            && self
                .completion(snapshot)
                .is_some_and(|menu| menu.stage == completion::Stage::Command)
        {
            let menu = self.completion(snapshot).context("No matching command")?;
            let item = menu
                .items
                .get(
                    self.completion_selected
                        .min(menu.items.len().saturating_sub(1)),
                )
                .context("No matching command")?;
            let spec = commands::COMMANDS
                .iter()
                .find(|spec| spec.name == item.value)
                .context("Unknown selection")?;
            if self.palette_query.text().split_whitespace().count() > 1
                || !matches!(
                    spec.arguments,
                    Arguments::None
                        | Arguments::History
                        | Arguments::Commands
                        | Arguments::Queue
                        | Arguments::Settings
                )
            {
                self.accept_completion(snapshot)?;
            } else {
                let name = spec.name.to_owned();
                if self.execute(spec.action, "", false, snapshot, tx)? {
                    self.remember(name);
                }
            }
            return Ok(());
        }
        self.completion_original = None;
        let source = self.editor().text().to_owned();
        let source = if self.palette && self.commands_focus && !source.starts_with('/') {
            format!("/{source}")
        } else {
            source
        };
        if (self.edit.is_none() || (self.palette && self.commands_focus))
            && let Some((spec, args)) = commands::parse(&source)?
        {
            let action = spec.action;
            let args = args.to_owned();
            let canonical = source.clone();
            let from_text = !(self.palette && self.commands_focus);
            if self.execute(action, &args, from_text, snapshot, tx)? && action != Action::Settings {
                self.remember(canonical);
            }
            return Ok(());
        }
        anyhow::ensure!(
            !(self.palette && self.commands_focus),
            "Palette input is not a command; /help lists commands"
        );
        if self.draft.text().trim().is_empty() && self.edit.is_none() {
            return Ok(());
        }
        anyhow::ensure!(self.notice_load() < NOTICE_LIMIT, "wait for history");
        if self.edit.is_none() {
            if self.mode == InputMode::Draft {
                self.draft_busy = self.enqueue(
                    tx,
                    Op::SaveDraft {
                        text: self.draft.text().to_owned(),
                        clear: true,
                    },
                    Command::SaveDraft {
                        text: self.draft.text().to_owned(),
                    },
                );
            } else {
                self.ensure_send_ready()?;
                self.send_command(
                    self.input_command(DeliveryMode::After, snapshot.paused, snapshot),
                    true,
                    snapshot,
                    tx,
                    true,
                );
            }
            return Ok(());
        }
        let (id, revision) = self.edit.clone().context("No active edit lease")?;
        self.ensure_send_ready()?;
        if self.draft.text().trim().is_empty() {
            self.draft_busy = self.enqueue(
                tx,
                Op::Cancel {
                    id: id.clone(),
                    clear_draft: false,
                },
                Command::Cancel { id, revision },
            );
            return Ok(());
        }
        if self.enqueue(
            tx,
            Op::FinishEdit,
            Command::FinishPendingEdit {
                id,
                revision,
                text: self.draft.text().to_owned(),
                expected_paused: snapshot.paused,
            },
        ) {
            self.draft_busy = true;
        }
        Ok(())
    }

    pub(super) fn completed(&mut self, done: Done, screen: &mut Screen) -> Result<bool> {
        self.completed_with(done, |full| screen.fullscreen(full))
    }

    pub(super) fn completed_with(
        &mut self,
        mut done: Done,
        mut fullscreen: impl FnMut(bool) -> Result<()>,
    ) -> Result<bool> {
        self.outstanding = self.outstanding.saturating_sub(1);
        let chain_op = matches!(done.op, Op::Chain(_));
        if let Op::Chain(op) = done.op {
            done.op = *op;
        }
        if chain_op {
            self.chain_pending = self.chain_pending.saturating_sub(1);
            self.chain_wait = self.chain_pending != 0;
        }
        if let Op::HistorySearch(request) = done.op {
            if self.history_search_busy == Some(request.generation) {
                self.history_search_busy = None;
            }
            if request.generation != self.history_search_generation
                || self.exiting
                || self.draft_busy
                || request.context != self.completion_context()
                || request.source != self.editor().text()
                || request.cursor != self.editor().cursor()
            {
                if request.generation == self.history_search_generation {
                    self.reset_completion();
                }
                return Ok(false);
            }
            match done.result {
                Ok(Reply::HistoryMatches(mut entries)) => {
                    entries.truncate(crate::runtime::MAX_HISTORY_MATCHES);
                    entries.retain(|entry| {
                        entry.seq > 0 && matches!(entry.kind.as_str(), "user" | "assistant")
                    });
                    for entry in &mut entries {
                        entry.preview = clean(
                            &entry.preview,
                            crate::runtime::MAX_HISTORY_PREVIEW_CHARS * 4,
                        )
                        .replace(['\n', '\t'], " ")
                        .chars()
                        .take(crate::runtime::MAX_HISTORY_PREVIEW_CHARS)
                        .collect();
                    }
                    let menu = completion::attach_history(
                        request.menu,
                        &request.source,
                        request.cursor,
                        &request.query,
                        &entries,
                    );
                    self.history_search_cache =
                        Some((request.query, entries, request.journal_cursor));
                    if menu.items.is_empty() {
                        self.completion_original = None;
                        self.notify("No matching saved dialogue", false);
                    } else {
                        self.open_completion(menu, request.search)?;
                        if request.reverse && self.view == View::Bindings && self.completion_active
                        {
                            self.navigate_completion(true, 1)?;
                        }
                    }
                }
                result => {
                    self.reset_completion();
                    let error = match result {
                        Err(error) => format!("{error:#}"),
                        Ok(_) => "unexpected runtime reply".into(),
                    };
                    self.notify(
                        format!("History search failed: {}", clean(&error, 1024)),
                        true,
                    );
                }
            }
            return Ok(false);
        }
        if matches!(done.op, Op::Notice) {
            self.notice_busy = false;
            self.notice_in_flight = None;
            let category = self.notice_category.take();
            match done.result {
                Ok(Reply::Notice(entry)) => {
                    // A late receipt may be older than the initial tail cursor.
                    // Only actual presentation, not that cursor, deduplicates it.
                    if !self
                        .native_notice
                        .as_ref()
                        .is_some_and(|held| held.seq == entry.seq)
                        && !self
                            .printed_notices
                            .iter()
                            .any(|(start, end)| *start <= entry.seq && entry.seq <= *end)
                        && !self
                            .live_notices
                            .iter()
                            .any(|notice| notice.seq == entry.seq)
                    {
                        if let Some(category) = category {
                            self.action_categories
                                .insert(entry.seq, native_category(Some(category)));
                        }
                        self.live_notices.push_back(entry);
                    }
                    self.last_poll = Instant::now() - Duration::from_secs(1);
                }
                result => {
                    let error = match result {
                        Err(error) => format!("{error:#}"),
                        Ok(_) => "unexpected runtime reply".into(),
                    };
                    if self.exiting {
                        self.exit_error = Some(format!("history not saved: {error}"));
                    }
                    self.notice = Some((
                        notice_text(&format!("history not saved: {error}")),
                        Instant::now(),
                        true,
                    ));
                }
            }
            let published = self.published.unwrap_or(0);
            self.printed_notices.retain(|(_, end)| *end > published);
            return Ok(false);
        }
        let native_tail_current = !self.native_tail_stale && !self.full_history;
        match &done.op {
            Op::Stop { clear_draft } => {
                self.stop_pending = false;
                if *clear_draft {
                    self.draft_busy = false;
                }
            }
            Op::RestoreComposer
            | Op::SaveDraft { .. }
            | Op::Dispatch { .. }
            | Op::DeleteDraft { .. }
            | Op::LoadDraft(_) => self.draft_busy = false,
            Op::SaveComposer { .. } => self.composer_busy = false,
            Op::Drafts => self.drafts_busy = false,
            Op::Actions { .. } => self.actions_busy = false,
            Op::ActionRefresh { .. } => self.action_refresh_busy = false,
            Op::Artifact { .. } => self.artifact_busy = false,
            Op::BeginEdit | Op::FinishEdit | Op::CancelEdit | Op::ReleaseEdit => {
                self.draft_busy = false
            }
            Op::Cancel { id, clear_draft }
                if *clear_draft || self.edit.as_ref().is_some_and(|(editing, _)| editing == id) =>
            {
                self.draft_busy = false
            }
            Op::Control {
                clear_draft,
                pause,
                outcome,
            } => {
                if *clear_draft {
                    self.draft_busy = false;
                }
                if *pause {
                    self.pause_pending = false;
                }
                if *outcome == "stopped" {
                    self.stop_pending = false;
                }
            }
            Op::Tail => {
                self.tail_busy = false;
                self.native_tail_stale = false;
            }
            Op::History(_, false) => self.history_busy = false,
            Op::History(_, true) | Op::HistoryRefresh => self.history_refresh_busy = false,
            Op::Events => self.events_busy = false,
            _ => {}
        }
        let reply = match done.result {
            Ok(reply) => reply,
            Err(error) => {
                if matches!(done.op, Op::ActionRefresh { .. }) {
                    return Ok(false);
                }
                if matches!(done.op, Op::Tail) && self.exiting {
                    return Err(error).context("native transcript read failed during exit");
                }
                if matches!(done.op, Op::BeginEdit | Op::ReleaseEdit) {
                    self.edit_draft = None;
                }
                if matches!(done.op, Op::SaveComposer { .. }) && self.pending_edit.take().is_some()
                {
                    self.edit_draft = None;
                    self.draft_busy = false;
                }
                if chain_op {
                    self.command_chain.clear();
                }
                if matches!(done.op, Op::SaveComposer { .. } | Op::RestoreComposer) {
                    self.composer_error = true;
                    if self.exiting {
                        self.exit_error = Some(format!("Composer not saved: {error:#}"));
                    }
                    if matches!(done.op, Op::RestoreComposer) {
                        self.draft_busy = true;
                    }
                }
                let text = format!("Command failed: {}", clean(&format!("{error:#}"), 1024));
                let category = match done.op {
                    Op::SaveDraft { .. } | Op::DeleteDraft { .. } | Op::LoadDraft(_) => {
                        Some("drafts")
                    }
                    Op::Dispatch { action: true, .. }
                    | Op::Stop { .. }
                    | Op::Control { .. }
                    | Op::BeginEdit
                    | Op::FinishEdit
                    | Op::CancelEdit
                    | Op::Cancel { .. } => Some("queue"),
                    _ => None,
                };
                if let Some(category) = category {
                    self.record_action(format!("error: {text}"), NoticeLevel::Error, category);
                }
                self.notify(text, true);
                if matches!(done.op, Op::Shutdown) {
                    self.exiting = false;
                    self.quit_pending = false;
                }
                return Ok(false);
            }
        };
        let reply = match (reply, &mut done.op) {
            (
                Reply::ArtifactPage {
                    offset: start,
                    chunk,
                },
                Op::Artifact { offset, .. },
            ) => {
                *offset = start;
                Reply::Artifact(chunk)
            }
            (reply, _) => reply,
        };
        match (done.op, reply) {
            (Op::Stop { clear_draft }, Reply::Stopped { paused, cancelled }) => {
                self.stopped = paused;
                if clear_draft {
                    self.draft.clear();
                }
                let outcome = if paused {
                    "stopped"
                } else if cancelled {
                    "response interrupted; pending input will continue"
                } else {
                    "pending input will continue"
                };
                self.record_action(outcome, NoticeLevel::Info, "queue");
                self.notify(outcome, false);
            }
            (Op::RestoreComposer, Reply::Composer(state)) => {
                if state.text.len() > self.draft.max_bytes() {
                    self.draft_busy = true;
                    self.composer_error = true;
                    if self.exiting {
                        self.exit_error = Some("Saved composer exceeds the configured byte limit; startup input was not loaded".into());
                    }
                    self.notify("Saved composer exceeds the configured byte limit; increase the limit before editing", true);
                    return Ok(false);
                }
                self.draft.set_text(&state.text);
                self.draft.set_cursor(state.cursor);
                self.composer_revision = state.revision;
                self.composer_saved = (self.draft.text().to_owned(), self.draft.cursor());
                self.composer_observed = self.composer_saved.clone();
                self.composer_ready = true;
            }
            (Op::SaveComposer { text, cursor }, Reply::Composer(state)) => {
                self.composer_revision = state.revision;
                self.composer_saved = (text, cursor);
            }
            (Op::Drafts, Reply::Drafts(drafts)) => {
                self.drafts = drafts;
                self.drafts_loaded = true;
                if self.view == View::Drafts {
                    self.selected = self.selected.min(self.drafts.len().saturating_sub(1));
                }
            }
            (Op::SaveDraft { text, clear }, Reply::Draft(draft)) => {
                self.drafts.retain(|saved| saved.id != draft.id);
                self.drafts.push(draft);
                if clear && self.draft.text() == text {
                    self.draft.clear();
                }
                if !chain_op {
                    self.palette_query.clear();
                    self.palette = self.palette_pinned;
                }
                self.record_action("draft saved", NoticeLevel::Info, "drafts");
                self.notify("draft saved", false);
            }
            (Op::LoadDraft(copy), Reply::Draft(saved)) => {
                self.drafts.push(saved);
                if copy.text.len() > self.message_limit.min(self.draft.max_bytes()) {
                    self.notify(
                        "Saved draft exceeds the configured byte limit; composer unchanged",
                        true,
                    );
                    return Ok(false);
                }
                self.draft
                    .set_text(&sanitized(&copy.text, self.message_limit).0);
                self.view = View::Root;
                self.full_history = false;
                self.reset_completion();
                self.record_action(
                    "Previous composer saved; draft copy loaded",
                    NoticeLevel::Info,
                    "drafts",
                );
                self.notify("Previous composer saved; draft copy loaded", false);
            }
            (Op::DeleteDraft { id, clear }, Reply::Ack) => {
                self.drafts.retain(|draft| draft.id != id);
                self.drafts_busy = false;
                if clear {
                    self.draft.clear();
                }
                if !chain_op {
                    self.palette_query.clear();
                    self.palette = self.palette_pinned;
                }
                self.record_action("draft deleted", NoticeLevel::Info, "drafts");
                self.notify("draft deleted", false);
            }
            (
                Op::Dispatch {
                    clear,
                    text,
                    action,
                    composer_revision,
                    draft_id,
                },
                Reply::Ack,
            ) => {
                self.stopped = false;
                if let Some(revision) = composer_revision {
                    // Checked input dispatch atomically consumes exactly this revision.
                    self.composer_revision = revision.saturating_add(1);
                    self.composer_saved = (String::new(), 0);
                }
                self.composer_error = false;
                if clear && self.draft.text() == text {
                    self.remember(text);
                    self.draft.clear();
                }
                if !chain_op {
                    self.palette_query.clear();
                    self.palette = self.palette_pinned;
                }
                if let Some(id) = draft_id {
                    self.drafts.retain(|draft| draft.id != id);
                    self.drafts_loaded = false;
                }
                if action {
                    self.record_action("message accepted", NoticeLevel::Info, "queue");
                }
                self.notify("message accepted", false);
            }
            (Op::Actions { load }, Reply::Actions(actions)) => {
                if load == ActionLoad::Latest
                    && !self.actions_follow
                    && !self.journal.actions.is_empty()
                {
                    return Ok(false);
                }
                if load == ActionLoad::Older && actions.is_empty() {
                    self.actions_oldest = true;
                }
                if load != ActionLoad::Older {
                    self.history_browser.actions_newest = actions.is_empty();
                }
                if load == ActionLoad::Newer
                    && actions.is_empty()
                    && self.journal.selected + 1 >= self.journal.actions.len()
                {
                    self.actions_follow = true;
                }
                let first = self.journal.actions.first().map(|action| action.seq);
                self.journal.append_actions(actions);
                if load == ActionLoad::Older {
                    self.history_browser.actions_newest = false;
                }
                if first
                    .zip(self.journal.actions.first().map(|action| action.seq))
                    .is_some_and(|(old, new)| new > old)
                {
                    self.actions_oldest = false;
                }
                self.history_browser.actions_oldest = self.actions_oldest;
                self.history_browser
                    .actions_loaded(&self.history, &mut self.journal);
                self.sync_history_browser();
                if load != ActionLoad::Latest
                    && let Some(Selection::Message(seq)) = self.history_browser.selection()
                {
                    self.history.anchor(*seq);
                }
            }
            (Op::ActionRefresh { id, seq }, Reply::Actions(actions)) => {
                if let Some(action) = actions
                    .into_iter()
                    .find(|action| action.id == id && action.seq == seq)
                    && self.journal.actions.iter().any(|cached| {
                        cached.id == id && cached.seq == seq && cached.status == "running"
                    })
                {
                    // Never reinsert an evicted row or consume pending page navigation.
                    self.journal.append_actions(vec![action]);
                    self.sync_history_browser();
                }
            }
            (Op::ActionRefresh { .. }, _) => {}
            (
                Op::Artifact {
                    id,
                    offset,
                    generation,
                },
                Reply::Artifact(chunk),
            ) => {
                if generation == self.preview_generation
                    && chunk.id == id
                    && self
                        .journal
                        .actions
                        .get(self.journal.selected)
                        .and_then(|action| action.output.as_ref())
                        .is_some_and(|output| output.id == id)
                {
                    let backwards = offset < self.preview_start;
                    self.preview_start = offset;
                    self.journal.preview_text = sanitized(&chunk.text, 8192).0;
                    self.journal.rewrap(self.history_preview_size.0 as usize);
                    self.journal.preview_offset = if backwards {
                        self.journal.preview.len().saturating_sub(
                            self.history_preview_size.1.saturating_sub(2).max(1) as usize,
                        )
                    } else {
                        0
                    };
                    self.journal.next_offset = chunk.next_offset;
                    self.journal.eof = chunk.eof;
                    self.journal.path = chunk.path;
                    self.journal.incomplete = chunk.incomplete;
                }
            }
            (Op::BeginEdit, Reply::Editing(item)) => {
                let changed = self.edit_draft.as_ref().is_some_and(|(text, cursor)| {
                    self.draft.text() != text || self.draft.cursor() != *cursor
                });
                if changed || item.text.len() > self.message_limit.min(self.draft.max_bytes()) {
                    self.release_edit = Some((item.id, item.revision));
                    self.draft_busy = true;
                    self.command_chain.clear();
                    let text = if changed {
                        "Composer changed; releasing edit lease, input unchanged"
                    } else {
                        "Queued message exceeds the configured byte limit; releasing edit lease, input unchanged"
                    };
                    self.record_action(format!("error: {text}"), NoticeLevel::Error, "queue");
                    self.notify(text, true);
                    return Ok(false);
                }
                self.edit = Some((item.id, item.revision));
                self.draft
                    .set_text(&sanitized(&item.text, self.message_limit).0);
                self.view = View::Root;
                self.full_history = false;
                self.record_action("editing queued message", NoticeLevel::Info, "queue");
                self.notify("editing queued message", false);
                fullscreen(false)?;
            }
            (Op::ReleaseEdit, Reply::Ack) => {
                self.edit_draft = None;
                self.notify("Edit lease released; input unchanged", false)
            }
            (op @ (Op::FinishEdit | Op::CancelEdit), Reply::Ack) => {
                if matches!(op, Op::FinishEdit) {
                    self.remember(self.draft.text().to_owned());
                    self.stopped = false;
                }
                self.restore_edit_draft();
                let outcome = if matches!(op, Op::CancelEdit) {
                    "edit cancelled"
                } else {
                    "queued message updated"
                };
                self.record_action(outcome, NoticeLevel::Info, "queue");
                self.notify(outcome, false);
            }
            (Op::Cancel { id, clear_draft }, Reply::Ack) => {
                if self
                    .edit
                    .as_ref()
                    .is_some_and(|(editing, _)| editing == &id)
                {
                    self.restore_edit_draft();
                }
                if clear_draft {
                    self.draft.clear();
                }
                self.record_action("message cancelled", NoticeLevel::Info, "queue");
                self.notify("message cancelled", false);
            }
            (
                Op::Control {
                    clear_draft,
                    outcome,
                    ..
                },
                Reply::Ack,
            ) => {
                if outcome == "stopped" {
                    self.stopped = true;
                } else if outcome == "resumed" {
                    self.stopped = false;
                }
                if clear_draft {
                    self.draft.clear();
                }
                self.record_action(outcome, NoticeLevel::Info, "queue");
                self.notify(outcome, false);
            }
            (Op::Shutdown, Reply::Ack) => return Ok(true),
            (Op::History(load, refresh), Reply::History(entries))
                if !matches!(load, Load::Anchor(_)) =>
            {
                if !refresh || (!self.history_refresh_stale && self.history.follows_tail()) {
                    self.history.apply(entries, load);
                    if !refresh {
                        self.history_browser
                            .page_loaded(load, &self.history, &mut self.journal);
                    }
                    self.sync_history_browser();
                }
            }
            (Op::History(Load::Anchor(expected), _), Reply::HistoryAnchor { seq, entries }) => {
                if seq <= 0
                    || seq != expected
                    || entries.len() > 8
                    || entries.first().is_none_or(|entry| entry.seq != seq)
                {
                    self.notify(
                        "Invalid history anchor reply; saved selection retained",
                        true,
                    );
                    return Ok(false);
                }
                if self.view != View::History {
                    return Ok(false);
                }
                self.history.apply(entries, Load::Anchor(seq));
                self.history_browser
                    .select_message(seq, &self.history, &mut self.journal);
                self.sync_history_browser();
            }
            (Op::HistoryRefresh, Reply::History(entries)) => {
                if !self.history_refresh_stale {
                    self.history.refresh(entries);
                    self.sync_history_browser();
                }
            }
            (Op::Inputs, Reply::Inputs(entries)) => {
                let mut inputs = entries
                    .into_iter()
                    .map(|text| {
                        if text.starts_with('/') && !text.contains('\n') {
                            format!("/{text}")
                        } else {
                            text
                        }
                    })
                    .collect::<Vec<_>>();
                inputs.append(&mut self.inputs);
                if inputs.len() > 100 {
                    inputs.drain(..inputs.len() - 100);
                }
                self.inputs = inputs;
            }
            (Op::Tail, Reply::NativeTranscript(mut entries)) => {
                if native_tail_current {
                    self.native_sync_required = false;
                }
                entries.sort_by_key(|e| e.entry.seq);
                if self.published.is_none() {
                    self.published = Some(entries.first().map_or(0, |item| item.entry.seq - 1));
                }
                let published = self.published.unwrap_or(0);
                for item in entries.into_iter().filter(|e| e.entry.seq > published) {
                    let entry = item.entry;
                    if entry.kind == "notice" && !self.notice_printed(entry.seq) {
                        self.action_categories
                            .insert(entry.seq, native_category(item.category.as_deref()));
                    }
                    self.pending.insert(entry.seq, entry);
                }
                // A page-one running head needs one extra slot to discover older
                // native notices before a later receipt. Printed intervals retain
                // deduplication proof when their redundant pending entries are evicted.
                while self.pending.len() > self.page + 1 {
                    if let Some(seq) = self
                        .pending
                        .iter()
                        .skip(1)
                        .find(|(seq, entry)| entry.kind == "notice" && self.notice_printed(**seq))
                        .map(|(&seq, _)| seq)
                    {
                        self.pending.remove(&seq);
                        self.action_categories.remove(&seq);
                        continue;
                    }
                    if let Some((seq, _)) = self.pending.pop_last()
                        && !self.live_notices.iter().any(|entry| entry.seq == seq)
                    {
                        self.action_categories.remove(&seq);
                    }
                }
            }
            (Op::Events, Reply::Events(events)) => {
                for event in events {
                    self.event_cursor = self.event_cursor.max(event.seq);
                }
                // History is authoritative for conversation text; journal payloads
                // are deliberately not interpreted or printed as messages.
            }
            (op, _) => {
                if matches!(op, Op::BeginEdit | Op::ReleaseEdit) {
                    self.edit_draft = None;
                }
                if matches!(op, Op::SaveComposer { .. }) && self.pending_edit.take().is_some() {
                    self.edit_draft = None;
                    self.draft_busy = false;
                }
                self.command_chain.clear();
                self.chain_wait = false;
                if matches!(op, Op::SaveComposer { .. } | Op::RestoreComposer) {
                    self.composer_error = true;
                    if self.exiting {
                        self.exit_error =
                            Some("Composer save not confirmed: unexpected reply".into());
                    }
                    if matches!(op, Op::RestoreComposer) {
                        self.draft_busy = true;
                    }
                }
                self.notify("Unexpected runtime reply; draft retained", true);
            }
        }
        Ok(false)
    }
}

fn native_category(category: Option<&str>) -> &'static str {
    match category {
        Some("settings") => "settings",
        Some("queue") => "queue",
        Some("drafts") => "drafts",
        Some("bindings") => "bindings",
        Some("delivery") => "delivery",
        Some("error") => "error",
        _ => "events",
    }
}

fn notice_text(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if ch.is_control() {
            continue;
        }
        let bytes = ch.len_utf8() + usize::from(space);
        if out.len() + bytes > MAX_NOTICE_BYTES {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    out
}

fn sanitized_graphemes(text: &str, limit: usize) -> (String, bool) {
    use unicode_segmentation::UnicodeSegmentation;
    // One UTF-8 code point of lookahead reveals whether the byte limit cuts a
    // grapheme, including clusters joined by removing terminal controls.
    let (text, truncated) = sanitized(text, limit.saturating_add(4));
    let mut out = String::new();
    for grapheme in text.graphemes(true) {
        if grapheme.len() > limit.saturating_sub(out.len()) {
            return (out, true);
        }
        out.push_str(grapheme);
    }
    (out, truncated)
}

pub(super) fn native_end(text: &str, offset: usize, running: bool) -> Option<(usize, bool)> {
    if offset > text.len() {
        return None;
    }
    let mut end = (offset + NATIVE_CHUNK).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if running || end < text.len() {
        if let Some(newline) = text[offset..end].rfind('\n') {
            end = offset + newline + 1;
        } else if running && text.len() - offset < NATIVE_CHUNK {
            return None;
        }
    }
    if running && end == offset {
        return None;
    }
    Some((end, !running && end == text.len()))
}

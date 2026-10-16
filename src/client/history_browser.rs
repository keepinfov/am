use crossterm::event::KeyCode;
use ratatui::{Frame, layout::Rect, text::Line, widgets::Paragraph};

use super::{HistoryView, Load};
use crate::client::{
    journal::{self, JournalView},
    theme::{divider, muted, notice, strong},
    transcript::clean,
};

pub(in crate::client) const WIDE_WIDTH: u16 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) enum Selection {
    Message(i64),
    Action(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client) enum Focus {
    #[default]
    List,
    Preview,
}

pub(in crate::client) enum Effect {
    None,
    Close,
    LoadMessages(Load),
    LoadActions { older: bool },
    ArtifactScroll(isize),
    ToggleArtifact,
    SelectionChanged,
}

struct Row {
    selection: Selection,
    label: String,
}

pub(in crate::client) struct HistoryBrowser {
    pub(in crate::client) focus: Focus,
    pub(in crate::client) chronological: bool,
    pub(in crate::client) actions_oldest: bool,
    pub(in crate::client) actions_newest: bool,
    selected: Option<Selection>,
    rows: Vec<Row>,
    list_offset: usize,
    preview: HistoryView,
    visible_preview: Vec<Line<'static>>,
    visible_chronological: Vec<Line<'static>>,
    pending: Option<(Selection, isize)>,
    artifact: bool,
}

impl Default for HistoryBrowser {
    fn default() -> Self {
        Self {
            focus: Focus::List,
            chronological: false,
            actions_oldest: false,
            actions_newest: false,
            selected: None,
            rows: Vec::new(),
            list_offset: 0,
            preview: HistoryView::new(1),
            visible_preview: Vec::new(),
            visible_chronological: Vec::new(),
            pending: None,
            artifact: false,
        }
    }
}

impl HistoryBrowser {
    pub(in crate::client) fn selection(&self) -> Option<&Selection> {
        self.selected.as_ref()
    }

    /// Call after applying history/action pages and before drawing. IDs keep
    /// selection stable across refreshes and cache eviction in either stream.
    pub(in crate::client) fn sync(&mut self, history: &HistoryView, journal: &mut JournalView) {
        self.rows = history
            .entries()
            .map(|entry| Row {
                selection: Selection::Message(entry.seq),
                label: format!(
                    "#{} {} [{}] {}",
                    entry.seq,
                    entry.kind,
                    clean(&entry.status, 48),
                    clean(&entry.text, 240).replace(['\n', '\t'], " ")
                ),
            })
            .collect();
        // These are separate sequence domains. Keep explicit sections rather
        // than suggesting an invented message/action chronology or timestamp.
        self.rows.extend(journal.actions.iter().map(|action| Row {
            selection: Selection::Action(action.id.clone()),
            label: format!(
                "action #{} {} [{}] {}",
                action.seq,
                clean(&action.category, 64),
                clean(&action.status, 48),
                clean(&action.label, 240)
            ),
        }));
        if !self
            .rows
            .iter()
            .any(|row| Some(&row.selection) == self.selected.as_ref())
        {
            self.selected = self
                .rows
                .iter()
                .rev()
                .find(|row| matches!(row.selection, Selection::Message(_)))
                .or_else(|| {
                    journal.selected_action().and_then(|action| {
                        self.rows
                            .iter()
                            .find(|row| row.selection == Selection::Action(action.id.clone()))
                    })
                })
                .or_else(|| self.rows.first())
                .map(|row| row.selection.clone());
        }
        self.sync_preview(history, journal);
    }

    fn sync_preview(&mut self, history: &HistoryView, journal: &mut JournalView) {
        self.artifact = false;
        match self.selected.as_ref() {
            Some(Selection::Message(seq)) => {
                if let Some(entry) = history.entries().find(|entry| entry.seq == *seq) {
                    let unchanged = self.preview.entries().next() == Some(entry);
                    if !unchanged
                        && self
                            .preview
                            .entries()
                            .next()
                            .is_some_and(|old| old.seq == *seq)
                    {
                        self.preview.refresh(vec![entry.clone()]);
                    } else if !unchanged {
                        self.preview.apply(vec![entry.clone()], Load::Oldest);
                        // A single-message preview must not request dialogue pages.
                        self.preview.apply(Vec::new(), Load::Newer);
                    }
                }
            }
            Some(Selection::Action(id)) => {
                journal.select_id(id);
                self.artifact = journal
                    .selected_action()
                    .is_some_and(|action| action.output.is_some());
            }
            None => {}
        }
    }

    /// Returns false if a reply was stale or the requested row is not loaded.
    pub(in crate::client) fn select_message(
        &mut self,
        seq: i64,
        history: &HistoryView,
        journal: &mut JournalView,
    ) -> bool {
        if !history.entries().any(|entry| entry.seq == seq) {
            return false;
        }
        self.selected = Some(Selection::Message(seq));
        self.focus = Focus::List;
        self.chronological = false;
        self.sync(history, journal);
        true
    }

    /// Call after history.apply(...). Consumes the navigation that requested
    /// the page; no second keypress is needed to cross a loaded-page seam.
    pub(in crate::client) fn page_loaded(
        &mut self,
        load: Load,
        history: &HistoryView,
        journal: &mut JournalView,
    ) {
        self.sync(history, journal);
        let seq = match load {
            Load::Latest => history.entries().last().map(|entry| entry.seq),
            Load::Oldest => history.entries().next().map(|entry| entry.seq),
            Load::Anchor(seq) => Some(seq),
            _ => None,
        };
        if let Some(seq) = seq {
            self.selected = Some(Selection::Message(seq));
            self.pending = None;
        } else {
            self.consume_pending(false);
        }
        self.sync_preview(history, journal);
    }

    fn consume_pending(&mut self, actions: bool) {
        if self
            .pending
            .as_ref()
            .is_none_or(|(selection, _)| matches!(selection, Selection::Action(_)) != actions)
        {
            return;
        }
        if let Some((selection, delta)) = self.pending.take()
            && let Some(index) = self.rows.iter().position(|row| row.selection == selection)
        {
            let messages = self
                .rows
                .iter()
                .take_while(|row| matches!(row.selection, Selection::Message(_)))
                .count();
            let next = index.saturating_add_signed(delta);
            // Paging stays in its own sequence domain; arrows cross the seam.
            let next = if actions {
                next.max(messages).min(self.rows.len().saturating_sub(1))
            } else {
                next.min(messages.saturating_sub(1))
            };
            self.selected = self.rows.get(next).map(|row| row.selection.clone());
        }
    }

    pub(in crate::client) fn actions_loaded(
        &mut self,
        history: &HistoryView,
        journal: &mut JournalView,
    ) {
        self.sync(history, journal);
        self.consume_pending(true);
        self.sync_preview(history, journal);
    }

    pub(in crate::client) fn preview_dimensions(width: u16, height: u16) -> (u16, u16) {
        let width = if width >= WIDE_WIDTH {
            width - (width / 3).clamp(28, 44) - 1
        } else {
            width
        };
        (width, height.saturating_sub(1))
    }

    pub(in crate::client) fn hint(&self, width: u16) -> &'static str {
        if width < 3 {
            return "";
        }
        if self.chronological {
            return match width {
                70.. => "Up/Down/Pg scroll / Home/End history / c/Esc list",
                32.. => "Pg scroll / Home/End / Esc list",
                16.. => "c/Esc list",
                _ => "Esc",
            };
        }
        if self.focus == Focus::Preview {
            return match (self.selected.as_ref(), self.artifact, width) {
                (Some(Selection::Message(_)), _, 70..) => {
                    "Up/Down/Pg scroll / Home/End message / Tab list / c scroll / Esc list"
                }
                (Some(Selection::Message(_)), _, 40..) => "Up/Down/Pg scroll / Tab/Esc list",
                (Some(Selection::Message(_)), _, 24..) => "Pg scroll / Esc list",
                (_, true, 70..) => "Up/Down/Pg output / Space fold / Tab/Esc list / c scroll",
                (_, true, 40..) => "Pg output / Space fold / Esc list",
                (_, true, 24..) => "Pg output / Esc list",
                (_, _, 24..) => "Tab/Esc list / c scroll",
                (_, _, 8..) => "Esc list",
                _ => "Esc",
            };
        }
        match (self.selected.as_ref(), self.artifact, width) {
            (None, _, 24..) => "c scroll / Esc back",
            (_, true, 100..) => {
                "Arrows select / Pg actions / Home/End messages / Tab preview / Space fold / c scroll / Esc back"
            }
            (_, _, 100..) => {
                "Arrows select / Pg pages / Home/End messages / Enter/Tab preview / c scroll / Esc back"
            }
            (_, true, 70..) => {
                "Arrows/Pg select / Enter preview / Space fold / c scroll / Esc back"
            }
            (_, _, 70..) => "Arrows/Pg select / Enter/Tab preview / c scroll / Esc back",
            (_, _, 50..) => "Arrows/Pg select / Enter preview / Esc back",
            (_, _, 40..) => "Enter preview / Pg pages / Esc back",
            (_, _, 24..) => "Enter view / Esc back",
            (_, _, 8..) => "Esc back",
            _ => "Esc",
        }
    }

    /// Artifact paging remains owned by Ui, including generations and byte
    /// offsets. Message preview scrolling and list focus are local to this view.
    pub(in crate::client) fn key(
        &mut self,
        key: KeyCode,
        width: u16,
        height: u16,
        history: &mut HistoryView,
        journal: &mut JournalView,
    ) -> Effect {
        match key {
            KeyCode::Esc if self.chronological => {
                self.chronological = false;
                self.focus = Focus::List;
                return Effect::None;
            }
            KeyCode::Esc if self.focus == Focus::Preview => {
                self.focus = Focus::List;
                return Effect::None;
            }
            KeyCode::Esc => return Effect::Close,
            KeyCode::Char('c') => {
                self.chronological = !self.chronological;
                if !self.chronological {
                    self.focus = Focus::List;
                }
                return Effect::None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.chronological = false;
                self.focus = if self.focus == Focus::List {
                    Focus::Preview
                } else {
                    Focus::List
                };
                return Effect::None;
            }
            KeyCode::Enter if !self.chronological => {
                self.focus = Focus::Preview;
                return Effect::None;
            }
            KeyCode::Char(' ') if !self.chronological && self.artifact => {
                return Effect::ToggleArtifact;
            }
            _ => {}
        }
        let page = height.saturating_sub(2).max(1) as isize;
        let delta = match key {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -page,
            KeyCode::PageDown => page,
            _ => 0,
        };
        if self.chronological {
            let load = match key {
                KeyCode::Home => Some(Load::Oldest),
                KeyCode::End => Some(Load::Latest),
                _ => history.scroll(delta, width, height.saturating_sub(1)),
            };
            return load.map_or(Effect::None, Effect::LoadMessages);
        }
        if self.focus == Focus::Preview {
            return match self.selected {
                Some(Selection::Action(_)) if delta != 0 => Effect::ArtifactScroll(delta),
                Some(Selection::Message(_)) => {
                    let (width, height) = Self::preview_dimensions(width, height);
                    match key {
                        KeyCode::Home => self.preview.cached_edge(false),
                        KeyCode::End => self.preview.cached_edge(true),
                        _ => {
                            self.preview.scroll(delta, width, height);
                        }
                    }
                    Effect::None
                }
                _ => Effect::None,
            };
        }
        if matches!(key, KeyCode::Home | KeyCode::End) {
            self.pending = None;
            return Effect::LoadMessages(if key == KeyCode::Home {
                Load::Oldest
            } else {
                Load::Latest
            });
        }
        let Some(index) = self
            .rows
            .iter()
            .position(|row| Some(&row.selection) == self.selected.as_ref())
        else {
            return Effect::None;
        };
        if delta == 0 {
            return Effect::None;
        }
        let message_count = history.entries().count();
        let (oldest, newest) = history.bounds();
        if matches!(self.selected, Some(Selection::Message(_))) {
            if delta < 0 && index.saturating_add_signed(delta) == 0 && !oldest && index == 0 {
                self.pending = self.selected.clone().map(|selection| (selection, delta));
                return Effect::LoadMessages(Load::Older);
            }
            if delta > 0
                && index + delta as usize >= message_count
                && !newest
                && (key == KeyCode::PageDown || message_count == self.rows.len())
            {
                self.pending = self.selected.clone().map(|selection| (selection, delta));
                return Effect::LoadMessages(Load::Newer);
            }
        } else if delta < 0
            && !self.actions_oldest
            && ((key == KeyCode::PageUp && index.saturating_add_signed(delta) < message_count)
                || (message_count == 0 && index == 0))
        {
            self.pending = self.selected.clone().map(|selection| (selection, delta));
            return Effect::LoadActions { older: true };
        } else if index + 1 == self.rows.len() && delta > 0 && !self.actions_newest {
            self.pending = self.selected.clone().map(|selection| (selection, delta));
            return Effect::LoadActions { older: false };
        }
        let mut next = index
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        if matches!(key, KeyCode::PageUp | KeyCode::PageDown) {
            next = if matches!(self.selected, Some(Selection::Message(_))) {
                next.min(message_count.saturating_sub(1))
            } else {
                next.max(message_count)
            };
        }
        if next == index {
            return Effect::None;
        }
        self.pending = None;
        self.selected = Some(self.rows[next].selection.clone());
        self.sync_preview(history, journal);
        Effect::SelectionChanged
    }

    /// Prepare only the visible rows before the existing immutable render pass.
    /// Width/height are the history body, excluding the outer screen chrome.
    pub(in crate::client) fn prepare(
        &mut self,
        width: u16,
        height: u16,
        history: &mut HistoryView,
    ) {
        let viewport = height.saturating_sub(1) as usize;
        let selected = self
            .rows
            .iter()
            .position(|row| Some(&row.selection) == self.selected.as_ref())
            .unwrap_or(0);
        self.list_offset = self
            .list_offset
            .min(selected)
            .max((selected + 1).saturating_sub(viewport));
        if self.chronological {
            self.visible_chronological = history.lines(width, height.saturating_sub(1));
            self.visible_preview.clear();
        } else if matches!(self.selected, Some(Selection::Message(_))) {
            let (width, height) = Self::preview_dimensions(width, height);
            self.visible_preview = self.preview.lines(width, height);
            self.visible_chronological.clear();
        } else {
            self.visible_preview.clear();
            self.visible_chronological.clear();
        }
    }

    pub(in crate::client) fn render(&self, frame: &mut Frame, area: Rect, journal: &JournalView) {
        let area = area.intersection(frame.area());
        if area.is_empty() {
            return;
        }
        if self.chronological {
            frame.render_widget(
                Paragraph::new(Line::styled("Conversation scroll", strong())),
                Rect::new(area.x, area.y, area.width, 1),
            );
            let body = Rect::new(
                area.x,
                area.y + 1,
                area.width,
                area.height.saturating_sub(1),
            );
            frame.render_widget(Paragraph::new(self.visible_chronological.clone()), body);
            return;
        }
        if area.width < WIDE_WIDTH {
            if self.focus == Focus::List {
                self.render_list(frame, area);
            } else {
                self.render_preview(frame, area, journal);
            }
            return;
        }
        let list_width = (area.width / 3).clamp(28, 44);
        self.render_list(frame, Rect::new(area.x, area.y, list_width, area.height));
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("\u{2502}", divider());
                area.height as usize
            ]),
            Rect::new(area.x + list_width, area.y, 1, area.height),
        );
        self.render_preview(
            frame,
            Rect::new(
                area.x + list_width + 1,
                area.y,
                area.width - list_width - 1,
                area.height,
            ),
            journal,
        );
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Conversations / Actions",
                if self.focus == Focus::List {
                    strong()
                } else {
                    muted()
                },
            )),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let height = area.height.saturating_sub(1) as usize;
        if height == 0 {
            return;
        }
        let selected = self
            .rows
            .iter()
            .position(|row| Some(&row.selection) == self.selected.as_ref())
            .unwrap_or(0);
        let offset = self
            .list_offset
            .min(selected)
            .max((selected + 1).saturating_sub(height));
        let rows = self
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(index, row)| {
                Line::styled(
                    format!("  {}", row.label),
                    if index == selected && self.focus == Focus::List {
                        strong()
                    } else if index == selected {
                        notice()
                    } else {
                        muted()
                    },
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(if rows.is_empty() {
                vec![Line::styled("No saved history.", muted())]
            } else {
                rows
            }),
            Rect::new(area.x, area.y + 1, area.width, height as u16),
        );
    }

    fn render_preview(&self, frame: &mut Frame, area: Rect, journal: &JournalView) {
        match self.selected {
            Some(Selection::Action(_)) => {
                journal::render_artifact_preview(frame, area, journal, self.focus == Focus::Preview)
            }
            Some(Selection::Message(seq)) => {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        format!("Message #{seq}"),
                        if self.focus == Focus::Preview {
                            strong()
                        } else {
                            muted()
                        },
                    )),
                    Rect::new(area.x, area.y, area.width, 1),
                );
                let body = Rect::new(
                    area.x,
                    area.y + 1,
                    area.width,
                    area.height.saturating_sub(1),
                );
                frame.render_widget(Paragraph::new(self.visible_preview.clone()), body);
            }
            None => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled("Select a saved entry to preview.", muted()),
                        Line::styled(self.hint(area.width), muted()),
                    ]),
                    area,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ActionRecord, ArtifactRecord, HistoryEntry};
    use ratatui::{Terminal, backend::TestBackend};

    fn fixture() -> (HistoryBrowser, HistoryView, JournalView) {
        let mut history = HistoryView::new(8);
        history.apply(
            (1..=2)
                .map(|seq| HistoryEntry {
                    seq,
                    attempt_id: "attempt".into(),
                    kind: "user".into(),
                    text: format!("message sentinel {seq}"),
                    status: "accepted".into(),
                })
                .collect(),
            Load::Latest,
        );
        let mut journal = JournalView::default();
        journal.append_actions(vec![ActionRecord {
            seq: 1,
            id: "action".into(),
            group_id: "group".into(),
            category: "read".into(),
            label: "action sentinel".into(),
            status: "done".into(),
            output: Some(ArtifactRecord {
                id: "artifact".into(),
                path: "output.txt".into(),
                bytes: 19,
                incomplete: false,
            }),
        }]);
        let mut browser = HistoryBrowser::default();
        browser.sync(&history, &mut journal);
        (browser, history, journal)
    }

    fn draw(
        browser: &mut HistoryBrowser,
        history: &mut HistoryView,
        journal: &JournalView,
        width: u16,
        height: u16,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        browser.prepare(width, height, history);
        terminal
            .draw(|frame| browser.render(frame, frame.area(), journal))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_list_contains_messages_and_actions_with_selected_preview() {
        let (mut browser, mut history, mut journal) = fixture();
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
        let screen = draw(&mut browser, &mut history, &journal, 120, 12);
        assert!(screen.contains("Conversations / Actions"));
        assert!(screen.contains("Message #2"));
        assert!(screen.contains("message sentinel 2"));
        assert!(screen.contains("action sentinel"));
        assert!(matches!(
            browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal),
            Effect::SelectionChanged
        ));
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action".into()))
        );
        journal.preview_text = "artifact sentinel".into();
        journal.rewrap(79);
        let screen = draw(&mut browser, &mut history, &journal, 120, 12);
        assert!(screen.contains("artifact sentinel"));
        assert!(screen.contains("19 bytes"));
        assert!(screen.contains("message sentinel 1"));
    }

    #[test]
    fn narrow_enter_tab_and_escape_focus_and_chronological_scroll_remain_available() {
        let (mut browser, mut history, mut journal) = fixture();
        assert!(!draw(&mut browser, &mut history, &journal, 60, 12).contains("Message #2"));
        browser.key(KeyCode::Enter, 60, 12, &mut history, &mut journal);
        assert_eq!(browser.focus, Focus::Preview);
        let screen = draw(&mut browser, &mut history, &journal, 60, 12);
        assert!(screen.contains("Message #2"));
        assert!(!screen.contains("action sentinel"));
        assert!(matches!(
            browser.key(KeyCode::Esc, 60, 12, &mut history, &mut journal),
            Effect::None
        ));
        assert_eq!(browser.focus, Focus::List);
        browser.key(KeyCode::Tab, 60, 12, &mut history, &mut journal);
        assert_eq!(browser.focus, Focus::Preview);
        browser.key(KeyCode::BackTab, 60, 12, &mut history, &mut journal);
        assert_eq!(browser.focus, Focus::List);
        browser.key(KeyCode::Char('c'), 60, 12, &mut history, &mut journal);
        assert!(browser.chronological);
        assert!(draw(&mut browser, &mut history, &journal, 60, 12).contains("Conversation scroll"));
        browser.key(KeyCode::Esc, 60, 12, &mut history, &mut journal);
        assert!(!browser.chronological);
        assert!(matches!(
            browser.key(KeyCode::Esc, 60, 12, &mut history, &mut journal),
            Effect::Close
        ));
        for width in [1, 2, 40, 99, 100, 120] {
            for height in [1, 2, 3] {
                draw(&mut browser, &mut history, &journal, width, height);
                browser.focus = Focus::Preview;
                draw(&mut browser, &mut history, &journal, width, height);
                browser.focus = Focus::List;
            }
        }
    }

    #[test]
    fn arrow_consumes_older_page_request_and_anchor_selects_exact_message() {
        let (mut browser, mut history, mut journal) = fixture();
        let latest = history.entries().last().unwrap().clone();
        history.apply(vec![latest], Load::Latest);
        browser.select_message(2, &history, &mut journal);
        assert!(matches!(
            browser.key(KeyCode::Up, 120, 12, &mut history, &mut journal),
            Effect::LoadMessages(Load::Older)
        ));
        let older = HistoryEntry {
            seq: 1,
            attempt_id: "attempt".into(),
            kind: "user".into(),
            text: "older".into(),
            status: "accepted".into(),
        };
        history.apply(vec![older], Load::Older);
        browser.page_loaded(Load::Older, &history, &mut journal);
        assert_eq!(browser.selection(), Some(&Selection::Message(1)));
        browser.page_loaded(Load::Anchor(2), &history, &mut journal);
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
        assert!(!browser.select_message(999, &history, &mut journal));
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
    }

    #[test]
    fn fx_focus_and_selection_use_brightness_and_a_thin_separator_without_markers() {
        let (mut browser, mut history, journal) = fixture();
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        for focus in [Focus::List, Focus::Preview] {
            browser.focus = focus;
            browser.prepare(120, 12, &mut history);
            terminal
                .draw(|frame| browser.render(frame, frame.area(), &journal))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(0, 0)].fg,
                if focus == Focus::List {
                    strong().fg.unwrap()
                } else {
                    muted().fg.unwrap()
                }
            );
            assert_eq!(
                buffer[(41, 0)].fg,
                if focus == Focus::Preview {
                    strong().fg.unwrap()
                } else {
                    muted().fg.unwrap()
                }
            );
            assert_eq!(
                buffer[(2, 2)].fg,
                if focus == Focus::List {
                    strong().fg.unwrap()
                } else {
                    notice().fg.unwrap()
                }
            );
            assert_eq!(buffer[(2, 1)].fg, muted().fg.unwrap());
            for y in 0..12 {
                assert_eq!(buffer[(40, y)].symbol(), "\u{2502}");
            }
            assert!(
                !buffer
                    .content
                    .iter()
                    .any(|cell| matches!(cell.symbol(), ">" | "|"))
            );
        }
    }

    #[test]
    fn hints_fit_the_width_and_describe_the_current_focus_and_available_artifact() {
        let (mut browser, mut history, mut journal) = fixture();
        assert!(browser.hint(60).contains("Enter preview"));
        assert!(browser.hint(120).contains("Home/End messages"));
        for action in [false, true] {
            if action {
                browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal);
            }
            for focus in [Focus::List, Focus::Preview] {
                browser.focus = focus;
                for width in 0..140 {
                    assert!(
                        browser.hint(width).len() <= width as usize,
                        "width {width}: {}",
                        browser.hint(width)
                    );
                }
                if focus == Focus::Preview {
                    assert!(browser.hint(120).contains("Esc list"));
                    assert!(!browser.hint(120).contains("Esc back"));
                    assert_eq!(browser.hint(120).contains("Space fold"), action);
                    assert_eq!(browser.hint(120).contains("Home/End message"), !action);
                } else {
                    assert!(browser.hint(120).contains("Esc back"));
                }
            }
            browser.focus = Focus::List;
        }
        journal.actions[0].output = None;
        browser.sync(&history, &mut journal);
        browser.focus = Focus::Preview;
        assert!(!browser.hint(120).contains("Space"));
        assert!(matches!(
            browser.key(KeyCode::Char(' '), 120, 12, &mut history, &mut journal),
            Effect::None
        ));
        browser.chronological = true;
        assert!(browser.hint(120).contains("c/Esc list"));
        for width in 0..140 {
            assert!(browser.hint(width).len() <= width as usize);
        }
    }

    #[test]
    fn mixed_stream_arrows_cross_without_exhausting_pages_and_pg_keys_stay_in_their_stream() {
        let (mut browser, mut history, mut journal) = fixture();
        let latest = history.entries().last().unwrap().clone();
        history.apply(vec![latest], Load::Anchor(2));
        let mut action = journal.actions[0].clone();
        action.seq = 100;
        action.id = "action-100".into();
        journal = JournalView::default();
        journal.append_actions(vec![action.clone()]);
        browser.select_message(2, &history, &mut journal);
        assert!(matches!(
            browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal),
            Effect::SelectionChanged
        ));
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action-100".into()))
        );
        assert!(matches!(
            browser.key(KeyCode::Up, 120, 12, &mut history, &mut journal),
            Effect::SelectionChanged
        ));
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
        assert!(matches!(
            browser.key(KeyCode::PageDown, 120, 12, &mut history, &mut journal),
            Effect::LoadMessages(Load::Newer)
        ));
        let mut next = history.entries().last().unwrap().clone();
        next.seq = 3;
        history.apply(vec![next], Load::Newer);
        browser.page_loaded(Load::Newer, &history, &mut journal);
        assert_eq!(browser.selection(), Some(&Selection::Message(3)));
        browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal);
        assert!(matches!(
            browser.key(KeyCode::PageUp, 120, 12, &mut history, &mut journal),
            Effect::LoadActions { older: true }
        ));
        action.seq = 99;
        action.id = "action-99".into();
        journal.append_actions(vec![action]);
        browser.actions_loaded(&history, &mut journal);
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action-99".into()))
        );
        assert!(matches!(
            browser.key(KeyCode::Up, 120, 12, &mut history, &mut journal),
            Effect::SelectionChanged
        ));
        assert_eq!(browser.selection(), Some(&Selection::Message(3)));
        browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal);
        assert!(matches!(
            browser.key(KeyCode::Home, 120, 12, &mut history, &mut journal),
            Effect::LoadMessages(Load::Oldest)
        ));
        assert!(matches!(
            browser.key(KeyCode::End, 120, 12, &mut history, &mut journal),
            Effect::LoadMessages(Load::Latest)
        ));
    }

    #[test]
    fn home_and_end_can_reload_saved_messages_even_when_the_list_is_empty() {
        let mut browser = HistoryBrowser::default();
        let mut history = HistoryView::new(8);
        let mut journal = JournalView::default();
        browser.sync(&history, &mut journal);
        assert!(matches!(
            browser.key(KeyCode::Home, 60, 12, &mut history, &mut journal),
            Effect::LoadMessages(Load::Oldest)
        ));
        assert!(matches!(
            browser.key(KeyCode::End, 60, 12, &mut history, &mut journal),
            Effect::LoadMessages(Load::Latest)
        ));
    }

    #[test]
    fn page_keys_remain_in_their_stream_at_known_oldest_and_newest_boundaries() {
        let (mut browser, mut history, mut journal) = fixture();
        history.apply(Vec::new(), Load::Older);
        let mut second = journal.actions[0].clone();
        second.id = "second-action".into();
        second.seq += 1;
        journal.append_actions(vec![second]);
        browser.actions_oldest = true;
        browser.actions_newest = true;
        browser.sync(&history, &mut journal);
        assert!(matches!(
            browser.key(KeyCode::PageDown, 120, 12, &mut history, &mut journal),
            Effect::None
        ));
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
        browser.key(KeyCode::PageUp, 120, 12, &mut history, &mut journal);
        assert_eq!(browser.selection(), Some(&Selection::Message(1)));
        browser.key(KeyCode::PageDown, 120, 12, &mut history, &mut journal);
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
        browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal);
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action".into()))
        );
        assert!(matches!(
            browser.key(KeyCode::PageUp, 120, 12, &mut history, &mut journal),
            Effect::None
        ));
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action".into()))
        );
        browser.key(KeyCode::PageDown, 120, 12, &mut history, &mut journal);
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("second-action".into()))
        );
        browser.key(KeyCode::PageUp, 120, 12, &mut history, &mut journal);
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action".into()))
        );
        browser.key(KeyCode::Up, 120, 12, &mut history, &mut journal);
        assert_eq!(browser.selection(), Some(&Selection::Message(2)));
    }

    #[test]
    fn distinct_interleaved_action_ids_and_long_fields_stay_inside_the_list_pane() {
        let (mut browser, mut history, mut journal) = fixture();
        let mut actions = Vec::new();
        for seq in 1..=3 {
            let mut action = journal.actions[0].clone();
            action.seq = seq;
            action.id = format!("action-{seq}");
            action.group_id = if seq == 2 { "b".into() } else { "a".into() };
            action.label = "similar label ".repeat(1000);
            action.status = if seq == 2 {
                "running".into()
            } else {
                "done".into()
            };
            actions.push(action);
        }
        journal = JournalView::default();
        journal.append_actions(actions);
        browser.sync(&history, &mut journal);
        browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal);
        browser.key(KeyCode::Down, 120, 12, &mut history, &mut journal);
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action-2".into()))
        );
        let screen = draw(&mut browser, &mut history, &journal, 120, 12);
        for seq in 1..=3 {
            assert!(screen.contains(&format!("action #{seq}")));
        }
        assert!(screen.contains("[running]"));
        for row in screen.lines() {
            assert_eq!(row.chars().nth(40), Some('\u{2502}'));
        }
        journal.append_actions(vec![journal.actions[2].clone()]);
        browser.sync(&history, &mut journal);
        assert_eq!(
            browser.selection(),
            Some(&Selection::Action("action-2".into()))
        );
    }
}

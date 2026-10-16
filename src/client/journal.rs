use std::collections::HashMap;

use ratatui::{Frame, layout::Rect, text::Line, widgets::Paragraph};

use crate::runtime::ActionRecord;

#[cfg(test)]
use super::transcript::tail;
#[cfg(test)]
use crate::runtime::ArtifactChunk;

use super::{
    theme::{muted, notice, strong},
    transcript::{sanitized, wrapped},
};

const MAX_ACTIONS: usize = 200;
#[cfg(test)]
const MAX_PREVIEW_BYTES: usize = 8192;
const MAX_PREVIEW_ROWS: usize = 10_000;

#[derive(Default)]
pub(super) struct JournalView {
    pub(super) actions: Vec<ActionRecord>,
    pub(super) selected: usize,
    pub(super) preview: Vec<Line<'static>>,
    pub(super) preview_offset: usize,
    pub(super) next_offset: u64,
    pub(super) eof: bool,
    pub(super) path: String,
    pub(super) incomplete: bool,
    pub(super) collapsed: bool,
    /// Sanitized, bounded source retained so a resize can rewrap without IO.
    pub(super) preview_text: String,
    preview_id: Option<String>,
    preview_width: usize,
    preview_truncated: bool,
}

impl JournalView {
    pub(super) fn selected_action(&self) -> Option<&ActionRecord> {
        self.actions.get(self.selected)
    }

    #[cfg(test)]
    pub(super) fn replace_actions(&mut self, actions: Vec<ActionRecord>) {
        self.update_actions(actions, false);
    }

    /// Merge older or newer pages by immutable action ID, not display labels.
    pub(super) fn append_actions(&mut self, actions: Vec<ActionRecord>) {
        self.update_actions(actions, true);
    }

    fn update_actions(&mut self, incoming: Vec<ActionRecord>, append: bool) {
        let selected_id = self.selected_action().map(|a| a.id.clone());
        let output_id = self
            .selected_action()
            .and_then(|a| a.output.as_ref())
            .map(|o| o.id.clone());
        let older =
            append && incoming.iter().map(|a| a.seq).min() < self.actions.first().map(|a| a.seq);
        let mut by_id = HashMap::new();
        if append {
            for action in std::mem::take(&mut self.actions) {
                by_id.insert(action.id.clone(), action);
            }
        }
        for action in incoming {
            by_id.insert(action.id.clone(), action);
        }
        let mut actions: Vec<_> = by_id.into_values().collect();
        actions.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)));
        let selected = selected_id
            .as_ref()
            .and_then(|id| actions.iter().position(|a| &a.id == id));
        let excess = actions.len().saturating_sub(MAX_ACTIONS);
        let mut start = if older { 0 } else { excess };
        // Prefer the requested page, but never evict the selected action.
        if let Some(index) = selected {
            start = start.min(index).max(index.saturating_sub(MAX_ACTIONS - 1));
        }
        actions.truncate(start.saturating_add(MAX_ACTIONS));
        actions.drain(..start);
        self.selected = selected
            .map(|index| index - start)
            .unwrap_or_else(|| self.selected.min(actions.len().saturating_sub(1)));
        self.actions = actions;
        let new_action = self.selected_action();
        let changed = selected_id.as_deref() != new_action.map(|a| a.id.as_str())
            || output_id.as_deref()
                != new_action
                    .and_then(|a| a.output.as_ref())
                    .map(|o| o.id.as_str());
        if changed {
            self.reset_preview();
        } else if let Some((path, incomplete, bytes)) = self
            .selected_action()
            .and_then(|a| a.output.as_ref())
            .map(|o| (single_line(&o.path, 1024), o.incomplete, o.bytes))
        {
            self.path = path;
            self.incomplete = incomplete;
            if bytes > self.next_offset {
                self.eof = false;
            }
        }
    }

    pub(super) fn select_id(&mut self, id: &str) -> bool {
        let Some(index) = self.actions.iter().position(|action| action.id == id) else {
            return false;
        };
        if index != self.selected {
            self.selected = index;
            self.reset_preview();
        }
        true
    }

    fn reset_preview(&mut self) {
        self.preview.clear();
        self.preview_text.clear();
        self.preview_id = None;
        self.preview_offset = 0;
        self.next_offset = 0;
        self.eof = false;
        self.preview_truncated = false;
        self.path.clear();
        self.incomplete = false;
        if let Some((path, incomplete)) = self
            .selected_action()
            .and_then(|a| a.output.as_ref())
            .map(|o| (single_line(&o.path, 1024), o.incomplete))
        {
            self.path = path;
            self.incomplete = incomplete;
        }
    }

    /// Streaming test fixture only. Production replaces independent pages in
    /// the UI, where request generations and backward byte offsets are tracked.
    #[cfg(test)]
    pub(super) fn set_artifact(&mut self, chunk: ArtifactChunk, width: usize) -> bool {
        if self
            .selected_action()
            .and_then(|a| a.output.as_ref())
            .is_none_or(|output| output.id != chunk.id)
        {
            return false;
        }
        let same = self.preview_id.as_deref() == Some(chunk.id.as_str());
        if same && chunk.next_offset < self.next_offset {
            return false;
        }
        if !same {
            self.reset_preview();
            self.preview_id = Some(chunk.id);
        }
        // An identical reply may be delivered twice; metadata can still change.
        if !same || chunk.next_offset != self.next_offset {
            let (text, truncated) = sanitized(&chunk.text, MAX_PREVIEW_BYTES);
            self.preview_truncated |= truncated;
            self.preview_text.push_str(&text);
            if self.preview_text.len() > MAX_PREVIEW_BYTES {
                let keep = tail(&self.preview_text, MAX_PREVIEW_BYTES);
                let removed = self.preview_text.len() - keep.len();
                let rows = wrapped(&self.preview_text[..removed], width.max(1)).len();
                self.preview_offset = self.preview_offset.saturating_sub(rows.saturating_sub(1));
                self.preview_text = keep.to_owned();
                self.preview_truncated = true;
            }
        }
        self.path = single_line(&chunk.path, 1024);
        self.incomplete = chunk.incomplete;
        self.next_offset = chunk.next_offset;
        self.eof = chunk.eof;
        self.rewrap(width);
        true
    }

    pub(super) fn rewrap(&mut self, width: usize) {
        self.preview_width = width.max(1);
        self.preview = preview_rows(&self.preview_text, self.preview_width);
        self.preview_offset = self
            .preview_offset
            .min(self.preview.len().saturating_sub(1));
    }
}

fn single_line(text: &str, limit: usize) -> String {
    sanitized(text, limit).0.replace(['\n', '\t'], " ")
}

fn preview_rows(text: &str, width: usize) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut rows = wrapped(text, width.max(1));
    rows.truncate(MAX_PREVIEW_ROWS);
    rows
}

fn render_main(frame: &mut Frame, area: Rect, journal: &JournalView, preview_focused: bool) {
    let area = area.intersection(frame.area());
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            "Preview",
            if preview_focused { strong() } else { muted() },
        )),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    if body.is_empty() {
        return;
    }
    let Some(action) = journal.selected_action() else {
        frame.render_widget(
            Paragraph::new(Line::styled("Select an action to inspect.", muted())),
            body,
        );
        return;
    };
    let mut metadata = vec![Line::styled(
        format!(
            "{} [{}]",
            single_line(&action.label, 512),
            single_line(&action.status, 48)
        ),
        notice(),
    )];
    if let Some(output) = &action.output {
        let path = if journal.path.is_empty() {
            &output.path
        } else {
            &journal.path
        };
        metadata.push(Line::styled(
            format!(
                "{} bytes{}{}{} / {}",
                output.bytes,
                if journal.incomplete || output.incomplete {
                    " / incomplete"
                } else {
                    ""
                },
                if journal.collapsed {
                    " / collapsed"
                } else {
                    ""
                },
                if journal.preview_truncated {
                    " / display truncated"
                } else {
                    ""
                },
                single_line(path, 1024),
            ),
            muted(),
        ));
    } else {
        metadata.push(Line::styled("No output artifact.", muted()));
    }
    let metadata_height = body.height.min(metadata.len() as u16);
    frame.render_widget(
        Paragraph::new(metadata),
        Rect::new(body.x, body.y, body.width, metadata_height),
    );
    if journal.collapsed || action.output.is_none() {
        return;
    }
    let output_area = Rect::new(
        body.x,
        body.y + metadata_height,
        body.width,
        body.height - metadata_height,
    );
    if output_area.is_empty() {
        return;
    }
    let rewrapped;
    let rows = if journal.preview_width == output_area.width as usize {
        &journal.preview
    } else {
        rewrapped = preview_rows(&journal.preview_text, output_area.width as usize);
        &rewrapped
    };
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                if journal.eof {
                    "Empty output."
                } else {
                    "Output not loaded yet."
                },
                muted(),
            )),
            output_area,
        );
    } else {
        let offset = journal.preview_offset.min(rows.len().saturating_sub(1));
        frame.render_widget(
            Paragraph::new(
                rows.iter()
                    .skip(offset)
                    .take(output_area.height as usize)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            output_area,
        );
    }
}

pub(super) fn render_artifact_preview(
    frame: &mut Frame,
    area: Rect,
    journal: &JournalView,
    focused: bool,
) {
    render_main(frame, area, journal, focused);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ArtifactRecord;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn action(seq: i64, group: &str) -> ActionRecord {
        ActionRecord {
            seq,
            id: format!("action-{seq}"),
            group_id: group.to_owned(),
            category: "read".to_owned(),
            label: format!("Inspect {seq}"),
            status: "done".to_owned(),
            output: Some(ArtifactRecord {
                id: format!("artifact-{seq}"),
                path: format!("output/{seq}.txt"),
                bytes: 42,
                incomplete: false,
            }),
        }
    }

    fn chunk(seq: i64, text: &str, next_offset: u64) -> ArtifactChunk {
        ArtifactChunk {
            id: format!("artifact-{seq}"),
            text: text.to_owned(),
            next_offset,
            eof: true,
            path: format!("output/{seq}.txt"),
            incomplete: false,
        }
    }

    fn draw(width: u16, height: u16, journal: &JournalView) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_artifact_preview(frame, frame.area(), journal, true);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn screen(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn refresh_preserves_selection_and_preview_scroll_by_id() {
        let mut journal = JournalView::default();
        journal.append_actions(vec![action(1, "a"), action(2, "a")]);
        assert!(journal.select_id("action-2"));
        journal.set_artifact(chunk(2, "a\nb\nc\nd", 7), 20);
        journal.preview_offset = 2;
        let mut updated = action(2, "a");
        updated.status = "complete".to_owned();
        journal.append_actions(vec![action(0, "z"), updated, action(1, "a")]);
        assert_eq!(journal.selected_action().unwrap().id, "action-2");
        assert_eq!(journal.selected, 2);
        assert_eq!(journal.preview_offset, 2);
        assert_eq!(journal.preview_text, "a\nb\nc\nd");
    }

    #[test]
    fn older_pages_merge_deduplicate_and_keep_selection_bounded() {
        let mut journal = JournalView::default();
        journal.replace_actions((200..400).map(|i| action(i, "a")).collect());
        journal.append_actions((100..201).map(|i| action(i, "a")).collect());
        assert_eq!(journal.actions.len(), MAX_ACTIONS);
        assert_eq!(journal.actions.first().unwrap().seq, 100);
        assert_eq!(journal.selected_action().unwrap().seq, 200);
        let last = journal.actions.last().unwrap().id.clone();
        assert!(journal.select_id(&last));
        let selected = journal.selected_action().unwrap().id.clone();
        journal.append_actions((0..100).map(|i| action(i, "a")).collect());
        assert_eq!(journal.actions.len(), MAX_ACTIONS);
        assert_eq!(journal.selected_action().unwrap().id, selected);
        journal.append_actions((400..650).map(|i| action(i, "b")).collect());
        assert_eq!(journal.actions.len(), MAX_ACTIONS);
        assert_eq!(journal.selected_action().unwrap().id, selected);
    }

    #[test]
    fn selection_clears_output_and_late_responses_are_discarded() {
        let mut journal = JournalView::default();
        journal.replace_actions(vec![action(1, "a"), action(2, "a")]);
        assert!(journal.set_artifact(chunk(1, "first", 5), 20));
        assert!(!journal.select_id("missing"));
        assert!(journal.select_id("action-1"));
        assert_eq!(journal.preview_text, "first");
        assert!(journal.select_id("action-2"));
        assert!(journal.preview.is_empty());
        assert_eq!(journal.next_offset, 0);
        assert_eq!(journal.path, "output/2.txt");
        assert!(!journal.set_artifact(chunk(1, "late", 9), 20));
        assert!(journal.set_artifact(chunk(2, "current", 7), 20));
        assert!(!journal.set_artifact(chunk(2, "stale", 3), 20));
        assert!(journal.set_artifact(chunk(2, "current", 7), 20));
        assert_eq!(journal.preview_text, "current");
        let mut changed = action(2, "a");
        changed.output.as_mut().unwrap().id = "replacement".to_owned();
        journal.replace_actions(vec![changed]);
        assert!(journal.preview.is_empty());
        assert!(!journal.set_artifact(chunk(2, "old artifact", 19), 20));
    }

    #[test]
    fn terminal_controls_are_removed_from_chunks_and_metadata() {
        let mut journal = JournalView::default();
        let mut item = action(1, "a");
        item.label = "label\x1b[2J\x07\nspoof".to_owned();
        journal.replace_actions(vec![item]);
        let mut output = chunk(1, "safe\x1b[31m\x00\x07\x08\u{009b}end\r\nnext\trow", 50);
        output.path = "path\x1b]52;c;bad\x07\nspoof".to_owned();
        journal.set_artifact(output, 60);
        assert!(
            journal
                .preview_text
                .chars()
                .all(|c| !c.is_control() || c == '\n' || c == '\t')
        );
        assert!(!journal.path.chars().any(char::is_control));
        assert!(
            !screen(&draw(100, 12, &journal))
                .chars()
                .any(|c| c.is_control() && c != '\n')
        );
    }

    #[test]
    fn preview_rewraps_on_resize_and_collapse_shows_only_metadata() {
        let mut journal = JournalView::default();
        journal.replace_actions(vec![action(1, "a")]);
        journal.set_artifact(chunk(1, "abcdefghij", 10), 10);
        assert_eq!(journal.preview.len(), 1);
        journal.rewrap(3);
        assert_eq!(journal.preview.len(), 4);
        assert_eq!(journal.preview[0], Line::from("abc"));
        // Rendering also adapts without requiring a mutable view or a read.
        assert!(screen(&draw(100, 12, &journal)).contains("abcdefghij"));
        journal.collapsed = true;
        let text = screen(&draw(100, 12, &journal));
        assert!(text.contains("collapsed"));
        assert!(text.contains("output/1.txt"));
        assert!(!text.contains("abcdefghij"));
    }

    #[test]
    fn large_chunks_pages_and_rendering_stay_bounded() {
        let mut journal = JournalView::default();
        journal.replace_actions((0..10_000).map(|i| action(i, "a")).collect());
        assert_eq!(journal.actions.len(), MAX_ACTIONS);
        let seq = journal.selected_action().unwrap().seq;
        journal.set_artifact(chunk(seq, &"\t".repeat(100_000), 100_000), 1);
        assert!(journal.preview_text.len() <= MAX_PREVIEW_BYTES);
        assert!(journal.preview.len() <= MAX_PREVIEW_ROWS);
        journal.set_artifact(chunk(seq, &"\u{00e9}".repeat(10_000), 120_000), 1);
        assert!(journal.preview_text.len() <= MAX_PREVIEW_BYTES);
        assert!(journal.preview.len() <= MAX_PREVIEW_ROWS);
        journal.preview_offset = usize::MAX;
        draw(1, 2, &journal);
        assert!(screen(&draw(120, 12, &journal)).contains("display truncated"));
    }

    #[test]
    fn artifact_focus_uses_brightness_without_markers_and_tiny_areas_are_safe() {
        let mut journal = JournalView::default();
        journal.append_actions(vec![action(1, "a")]);
        journal.set_artifact(chunk(1, "artifact sentinel", 17), 80);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        for focused in [false, true] {
            terminal
                .draw(|frame| render_artifact_preview(frame, frame.area(), &journal, focused))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!(row(buffer, 0).starts_with("Preview"));
            assert!(!screen(buffer).contains('>'));
            assert_eq!(
                buffer[(0, 0)].fg,
                if focused {
                    strong().fg.unwrap()
                } else {
                    muted().fg.unwrap()
                }
            );
        }
        for width in [1, 2, 5, 40, 100] {
            for height in [1, 2, 3, 4] {
                journal.preview_offset = usize::MAX;
                draw(width, height, &journal);
            }
        }
        terminal
            .draw(|frame| {
                render_artifact_preview(frame, Rect::new(2, 2, 0, 0), &journal, true);
                render_artifact_preview(frame, Rect::new(100, 100, 2, 2), &journal, true);
            })
            .unwrap();
    }
}

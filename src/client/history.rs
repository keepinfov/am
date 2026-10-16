use std::{collections::VecDeque, ops::Range};

use ratatui::text::Line;

use super::transcript::transcript_rows;
use crate::runtime::{ActiveView, Command, HistoryEntry};

#[path = "history_browser.rs"]
pub(super) mod browser;

// Two neighboring segments still fit after tab expansion at narrow widths.
const CHUNK_BYTES: usize = 1024;
const CACHE_MESSAGES: usize = 16;
const CACHE_ROWS: usize = 10_000;

#[derive(Clone, Copy, Debug)]
pub enum Load {
    Latest,
    Oldest,
    Older,
    Newer,
    Anchor(i64),
}

struct Message {
    entry: HistoryEntry,
    chunks: Vec<Range<usize>>,
}

impl Message {
    fn new(entry: HistoryEntry) -> Self {
        let chunks = chunks(&entry.text, 0, 0);
        Self { entry, chunks }
    }
}

// Only byte ranges are indexed eagerly. Wrapping and styling are lazy.
fn chunks(text: &str, mut start: usize, stable_until: usize) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    while start < text.len() {
        let mut end = start.saturating_add(CHUNK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let line_start = start.max(stable_until);
        if end < text.len()
            && line_start < end
            && let Some(newline) = text[line_start..end].rfind('\n')
        {
            end = line_start + newline + 1;
        }
        result.push(start..end);
        start = end;
    }
    if result.is_empty() {
        result.push(start..start);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    seq: i64,
    chunk: usize,
    row: usize,
}

struct Wrapped {
    seq: i64,
    chunk: usize,
    rows: Vec<Line<'static>>,
}

pub struct HistoryView {
    limit: usize,
    messages: Vec<Message>,
    wrapped: VecDeque<Wrapped>,
    cached_rows: usize,
    width: u16,
    height: u16,
    top: Option<Position>,
    follow: bool,
    oldest: bool,
    newest: bool,
    pending: isize,
}

impl HistoryView {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.clamp(1, 8),
            messages: Vec::new(),
            wrapped: VecDeque::new(),
            cached_rows: 0,
            width: 0,
            height: 0,
            top: None,
            follow: true,
            oldest: false,
            newest: false,
            pending: 0,
        }
    }

    pub fn command(&self, load: Load) -> Command {
        if let Load::Anchor(seq) = load {
            return Command::TranscriptAnchor {
                seq,
                limit: self.limit,
            };
        }
        let (before, after) = match load {
            Load::Latest => (None, None),
            Load::Oldest => (None, Some(0)),
            Load::Older => (self.messages.first().map(|m| m.entry.seq), None),
            Load::Newer => (None, Some(self.messages.last().map_or(0, |m| m.entry.seq))),
            Load::Anchor(_) => unreachable!(),
        };
        Command::Transcript {
            before,
            after,
            limit: self.limit,
        }
    }

    pub(super) fn refresh_command(&self) -> Option<Command> {
        let seq = self
            .messages
            .iter()
            .find(|message| message.entry.status == "running")?
            .entry
            .seq;
        Some(Command::Transcript {
            before: seq.checked_add(1),
            after: seq.checked_sub(1),
            limit: 1,
        })
    }

    pub(super) fn refresh(&mut self, entries: Vec<HistoryEntry>) {
        for entry in entries {
            if let Some(index) = self.index(entry.seq) {
                let mut message = self.messages.remove(index);
                self.update(&mut message, entry);
                self.messages.insert(index, message);
            }
        }
    }

    pub fn apply(&mut self, mut entries: Vec<HistoryEntry>, load: Load) {
        entries.sort_by_key(|e| e.seq);
        entries.dedup_by_key(|e| e.seq);
        let empty = entries.is_empty();
        let pending = std::mem::take(&mut self.pending);
        let replace = matches!(load, Load::Latest | Load::Oldest | Load::Anchor(_));
        let mut previous = std::mem::take(&mut self.messages);
        let mut messages = if replace {
            Vec::new()
        } else {
            std::mem::take(&mut previous)
        };
        for entry in entries {
            if let Some(index) = messages.iter().position(|m| m.entry.seq == entry.seq) {
                self.update(&mut messages[index], entry);
            } else if let Some(index) = previous.iter().position(|m| m.entry.seq == entry.seq) {
                let mut message = previous.remove(index);
                self.update(&mut message, entry);
                messages.push(message);
            } else {
                messages.push(Message::new(entry));
            }
        }
        messages.sort_by_key(|m| m.entry.seq);
        match load {
            Load::Latest => {
                self.oldest = empty;
                self.newest = true;
                self.follow = true;
                self.top = None;
            }
            Load::Oldest => {
                self.oldest = true;
                self.newest = empty;
                self.follow = false;
                self.top = None;
            }
            Load::Older => {
                self.oldest = empty;
                self.follow = false;
            }
            Load::Newer => self.newest = empty,
            Load::Anchor(seq) => {
                self.oldest = false;
                self.newest = false;
                self.follow = false;
                self.top = Some(Position {
                    seq,
                    chunk: 0,
                    row: 0,
                });
            }
        }
        if messages.len() > CACHE_MESSAGES {
            if matches!(load, Load::Older | Load::Oldest) {
                messages.truncate(CACHE_MESSAGES);
                self.newest = false;
            } else {
                messages.drain(..messages.len() - CACHE_MESSAGES);
                self.oldest = false;
            }
        }
        self.messages = messages;
        self.wrapped.retain(|w| {
            self.messages
                .binary_search_by_key(&w.seq, |m| m.entry.seq)
                .is_ok()
        });
        self.cached_rows = self.wrapped.iter().map(|w| w.rows.len()).sum();
        if let Some(top) = self.top
            && self.index(top.seq).is_none()
        {
            self.top = self.first();
        }
        if !replace && pending != 0 && self.width != 0 && self.height != 0 {
            // Consume the key that requested the page, without moving the seam
            // merely because rows were prepended or dropped from the cache.
            let _ = self.scroll(pending, self.width, self.height);
            self.pending = 0;
        }
    }

    fn update(&mut self, message: &mut Message, mut entry: HistoryEntry) {
        let old = &message.entry;
        if old.attempt_id == entry.attempt_id {
            if old.status != "running" && entry.status == "running" {
                return;
            }
            if old.status == "running"
                && entry.status == "running"
                && old.text.starts_with(&entry.text)
            {
                // A transcript checkpoint can lag behind the live overlay.
                entry.text.clone_from(&old.text);
            }
        }
        if old.text == entry.text
            && old.kind == entry.kind
            && old.status == entry.status
            && old.attempt_id == entry.attempt_id
        {
            return;
        }
        let append = old.kind == entry.kind
            && old.status == entry.status
            && old.attempt_id == entry.attempt_id
            && entry.text.starts_with(&old.text);
        let changed_chunk = if append { message.chunks.len() - 1 } else { 0 };
        let start = message.chunks[changed_chunk].start;
        // Appending must not move a chunk seam backwards through text that
        // already has a visible row anchor.
        let stable_until = if append { old.text.len() } else { 0 };
        message.chunks.truncate(changed_chunk);
        message
            .chunks
            .extend(chunks(&entry.text, start, stable_until));
        self.wrapped
            .retain(|w| w.seq != entry.seq || w.chunk < changed_chunk);
        self.cached_rows = self.wrapped.iter().map(|w| w.rows.len()).sum();
        message.entry = entry;
    }

    pub fn sync_active(&mut self, active: Option<&ActiveView>) {
        let Some(active) = active else { return };
        let Some(index) = self
            .messages
            .iter()
            .position(|m| m.entry.attempt_id == active.attempt_id && m.entry.status == "running")
        else {
            return;
        };
        let message = &self.messages[index];
        if active.text.len() <= message.entry.text.len()
            || !active.text.starts_with(&message.entry.text)
        {
            return;
        }
        let mut entry = message.entry.clone();
        entry.text.clone_from(&active.text);
        let mut message = self.messages.remove(index);
        self.update(&mut message, entry);
        self.messages.insert(index, message);
    }

    fn index(&self, seq: i64) -> Option<usize> {
        self.messages
            .binary_search_by_key(&seq, |m| m.entry.seq)
            .ok()
    }

    fn first(&self) -> Option<Position> {
        self.messages.first().map(|m| Position {
            seq: m.entry.seq,
            chunk: 0,
            row: 0,
        })
    }

    fn last(&self) -> Option<Position> {
        self.messages.last().map(|m| Position {
            seq: m.entry.seq,
            chunk: m.chunks.len() - 1,
            row: usize::MAX,
        })
    }

    fn rows(&mut self, position: Position) -> &[Line<'static>] {
        if let Some(index) = self
            .wrapped
            .iter()
            .position(|w| w.seq == position.seq && w.chunk == position.chunk)
        {
            let wrapped = self.wrapped.remove(index).unwrap();
            self.wrapped.push_back(wrapped);
        } else {
            let message = &self.messages[self.index(position.seq).unwrap()];
            let chunk = &message.chunks[position.chunk];
            let mut rows = transcript_rows(
                &message.entry,
                &message.entry.text[chunk.clone()],
                position.chunk + 1 == message.chunks.len(),
                usize::from(self.width.max(1)),
            );
            if rows.is_empty() {
                rows.push(Line::default());
            }
            // A 1 KiB segment with basic wrapping fits comfortably in this
            // budget, even at a one-column terminal width.
            assert!(
                rows.len() <= CACHE_ROWS,
                "transcript segment exceeds row budget"
            );
            while self.cached_rows + rows.len() > CACHE_ROWS {
                self.cached_rows -= self.wrapped.pop_front().unwrap().rows.len();
            }
            self.cached_rows += rows.len();
            self.wrapped.push_back(Wrapped {
                seq: position.seq,
                chunk: position.chunk,
                rows,
            });
        }
        &self.wrapped.back().unwrap().rows
    }

    fn adjacent(&self, position: Position, forward: bool) -> Option<Position> {
        let index = self.index(position.seq)?;
        let message = &self.messages[index];
        if forward {
            if position.chunk + 1 < message.chunks.len() {
                Some(Position {
                    chunk: position.chunk + 1,
                    row: 0,
                    ..position
                })
            } else {
                self.messages.get(index + 1).map(|m| Position {
                    seq: m.entry.seq,
                    chunk: 0,
                    row: 0,
                })
            }
        } else if position.chunk > 0 {
            Some(Position {
                chunk: position.chunk - 1,
                row: usize::MAX,
                ..position
            })
        } else {
            index.checked_sub(1).map(|i| Position {
                seq: self.messages[i].entry.seq,
                chunk: self.messages[i].chunks.len() - 1,
                row: usize::MAX,
            })
        }
    }

    fn walk(
        &mut self,
        mut position: Position,
        mut count: usize,
        forward: bool,
    ) -> (Position, usize) {
        loop {
            let last_row = self.rows(position).len() - 1;
            position.row = position.row.min(last_row);
            let available = if forward {
                last_row - position.row
            } else {
                position.row
            };
            let step = count.min(available);
            if forward {
                position.row += step;
            } else {
                position.row -= step;
            }
            count -= step;
            if count == 0 {
                return (position, 0);
            }
            let Some(next) = self.adjacent(position, forward) else {
                return (position, count);
            };
            position = next;
            count -= 1;
        }
    }

    fn prepare(&mut self, width: u16, height: u16) {
        if self.width != width.max(1) {
            self.width = width.max(1);
            self.wrapped.clear();
            self.cached_rows = 0;
        }
        self.height = height.min(CACHE_ROWS as u16);
        if self.follow {
            self.top = self.bottom();
        } else {
            self.top = self.top.or_else(|| self.first());
            if let Some(mut top) = self.top {
                let index = self.index(top.seq).unwrap();
                top.chunk = top.chunk.min(self.messages[index].chunks.len() - 1);
                top.row = top.row.min(self.rows(top).len() - 1);
                let (_, missing) = self.walk(top, usize::from(self.height.saturating_sub(1)), true);
                if missing > 0 {
                    top = self.walk(top, missing, false).0;
                }
                self.top = Some(top);
            }
        }
    }

    fn bottom(&mut self) -> Option<Position> {
        self.last().map(|last| {
            self.walk(last, usize::from(self.height.saturating_sub(1)), false)
                .0
        })
    }

    pub fn scroll(&mut self, delta: isize, width: u16, height: u16) -> Option<Load> {
        if delta == 0 || height == 0 || width == 0 {
            return None;
        }
        self.prepare(width, height);
        if delta < 0 {
            self.follow = false;
        }
        let Some(top) = self.top else {
            let load = if delta < 0 { Load::Older } else { Load::Newer };
            return if (delta < 0 && !self.oldest) || (delta > 0 && !self.newest) {
                self.pending = delta;
                Some(load)
            } else {
                None
            };
        };
        let count = delta.unsigned_abs().min(CACHE_ROWS);
        // Stop at the last full viewport, rather than scrolling its first row
        // all the way to the end and leaving an otherwise empty screen.
        let (mut next, mut remaining) = self.walk(top, count, delta > 0);
        let mut at_bottom = false;
        if delta > 0 {
            let (end, missing) = self.walk(next, usize::from(self.height.saturating_sub(1)), true);
            at_bottom = self.adjacent(end, true).is_none() && end.row + 1 == self.rows(end).len();
            let overshoot = missing.min(count - remaining);
            next = self.walk(next, overshoot, false).0;
            remaining += overshoot;
        }
        self.top = Some(next);
        if delta < 0 && remaining > 0 && !self.oldest {
            self.pending = -(remaining as isize);
            return Some(Load::Older);
        }
        if delta > 0 {
            if at_bottom && !self.newest && (remaining > 0 || next == top) {
                self.pending = remaining.max(1) as isize;
                return Some(Load::Newer);
            }
            self.follow = at_bottom && self.newest;
        }
        None
    }

    pub fn lines(&mut self, width: u16, height: u16) -> Vec<Line<'static>> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        self.prepare(width, height);
        let Some(mut position) = self.top else {
            return Vec::new();
        };
        let height = usize::from(self.height);
        let mut visible = Vec::with_capacity(height);
        while visible.len() < height {
            let rows = self.rows(position);
            let end = rows.len().min(position.row + height - visible.len());
            visible.extend_from_slice(&rows[position.row..end]);
            let Some(next) = self.adjacent(position, true) else {
                break;
            };
            position = next;
        }
        visible
    }

    pub fn follows_tail(&self) -> bool {
        self.follow
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.messages.iter().map(|message| &message.entry)
    }

    pub(super) fn bounds(&self) -> (bool, bool) {
        (self.oldest, self.newest)
    }

    pub(super) fn anchor(&mut self, seq: i64) -> bool {
        if self.index(seq).is_none() {
            return false;
        }
        self.top = Some(Position {
            seq,
            chunk: 0,
            row: 0,
        });
        self.follow = false;
        self.pending = 0;
        true
    }

    pub(super) fn cached_edge(&mut self, newest: bool) {
        self.follow = newest;
        self.top = if newest { None } else { self.first() };
        self.pending = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: i64, text: impl Into<String>) -> HistoryEntry {
        HistoryEntry {
            seq,
            attempt_id: format!("attempt-{seq}"),
            kind: "assistant".into(),
            text: text.into(),
            status: "completed".into(),
        }
    }

    fn plain(lines: Vec<Line<'static>>) -> String {
        lines
            .into_iter()
            .map(|line| {
                let mut text = line
                    .spans
                    .into_iter()
                    .map(|s| s.content.into_owned())
                    .collect::<String>();
                text.push('\n');
                text
            })
            .collect()
    }

    #[test]
    fn segments_are_lossless_utf8_and_prefer_lines() {
        for text in [
            String::new(),
            "\u{1f642}".repeat(5000),
            "abc\n".repeat(5000),
            format!("{}\n{}", "x".repeat(4094), "\u{e9}".repeat(5000)),
        ] {
            let parts = chunks(&text, 0, 0);
            let joined = parts
                .iter()
                .map(|r| {
                    assert!(r.len() <= CHUNK_BYTES);
                    assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
                    &text[r.clone()]
                })
                .collect::<String>();
            assert_eq!(joined, text);
        }
        let text = format!("{}\n{}", "x".repeat(500), "y".repeat(5000));
        assert_eq!(chunks(&text, 0, 0)[0], 0..501);
    }

    #[test]
    fn anchored_page_starts_at_saved_entry_and_keeps_chronological_paging() {
        let mut view = HistoryView::new(8);
        assert!(matches!(
            view.command(Load::Anchor(42)),
            Command::TranscriptAnchor { seq: 42, limit: 8 }
        ));
        view.apply(
            vec![entry(42, "anchored message"), entry(43, "next message")],
            Load::Anchor(42),
        );
        assert!(!view.follows_tail());
        assert_eq!(view.top.unwrap().seq, 42);
        assert!(plain(view.lines(80, 1)).contains("anchored message"));
        assert!(matches!(view.scroll(-1, 80, 1), Some(Load::Older)));
        assert!(!view.anchor(999));
        assert!(view.anchor(43));
        assert_eq!(view.top.unwrap().seq, 43);
    }

    #[test]
    fn entire_long_message_is_reachable_in_both_directions() {
        let text = (0..2400)
            .map(|i| format!("marker{i:04}\n"))
            .collect::<String>();
        assert!(text.len() > 16 * 1024);
        let mut view = HistoryView::new(100);
        view.apply(vec![entry(1, text)], Load::Oldest);
        let mut forward = String::new();
        for _ in 0..5000 {
            forward.push_str(&plain(view.lines(80, 5)));
            let before = view.top;
            view.scroll(1, 80, 5);
            assert!(view.cached_rows <= CACHE_ROWS);
            if view.top == before {
                break;
            }
        }
        for i in 0..2400 {
            assert!(
                forward.contains(&format!("marker{i:04}")),
                "missing forward row {i}"
            );
        }
        let mut backward = String::new();
        for _ in 0..5000 {
            backward.push_str(&plain(view.lines(80, 5)));
            let before = view.top;
            view.scroll(-1, 80, 5);
            if view.top == before {
                break;
            }
        }
        for i in 0..2400 {
            assert!(
                backward.contains(&format!("marker{i:04}")),
                "missing backward row {i}"
            );
        }
    }

    #[test]
    fn live_overlay_preserves_scrolled_view_and_durable_completion() {
        let mut running = entry(1, "first\n".repeat(100));
        running.status = "running".into();
        let mut view = HistoryView::new(8);
        view.apply(vec![running.clone()], Load::Latest);
        view.lines(40, 4);
        assert!(view.follows_tail());
        view.scroll(-10, 40, 4);
        let before = plain(view.lines(40, 4));
        let active = ActiveView {
            attempt_id: running.attempt_id.clone(),
            text: format!("{}live suffix", running.text),
            truncated: false,
            message_id: String::new(),
            durable_bytes: running.text.len(),
        };
        view.sync_active(Some(&active));
        assert_eq!(plain(view.lines(40, 4)), before);
        assert!(!view.follows_tail());
        assert!(view.messages[0].entry.text.ends_with("live suffix"));
        view.sync_active(None);
        view.apply(vec![running.clone()], Load::Newer);
        assert!(view.messages[0].entry.text.ends_with("live suffix"));
        let mut incompatible = active.clone();
        incompatible.text = "incompatible".repeat(100);
        view.sync_active(Some(&incompatible));
        assert!(view.messages[0].entry.text.ends_with("live suffix"));
        let completed = entry(1, format!("{}durable finish", active.text));
        view.apply(vec![completed.clone()], Load::Newer);
        view.sync_active(Some(&active));
        view.apply(vec![running], Load::Latest);
        assert_eq!(view.messages[0].entry.text, completed.text);
        assert_eq!(view.messages[0].entry.status, "completed");
        assert!(plain(view.lines(80, 4)).contains("durable finish"));
    }

    #[test]
    fn refresh_finalizes_cached_running_entry_without_moving_reader() {
        let mut running = entry(1, "first\n".repeat(100));
        running.status = "running".into();
        let mut view = HistoryView::new(8);
        view.apply(vec![running.clone()], Load::Latest);
        view.lines(40, 4);
        view.scroll(-10, 40, 4);
        let before = plain(view.lines(40, 4));
        let anchor = view.top;
        assert!(matches!(
            view.refresh_command(),
            Some(Command::Transcript {
                before: Some(2),
                after: Some(0),
                limit: 1
            })
        ));
        let completed = entry(1, format!("{}final suffix", running.text));
        view.refresh(vec![completed.clone(), entry(2, "not cached")]);
        assert!(!view.follows_tail());
        assert_eq!(view.top, anchor);
        assert_eq!(plain(view.lines(40, 4)), before);
        assert_eq!(view.messages.len(), 1);
        assert_eq!(view.messages[0].entry.text, completed.text);
        assert!(view.refresh_command().is_none());
        view.refresh(vec![running]);
        assert_eq!(view.messages[0].entry.status, "completed");
        assert!(view.messages[0].entry.text.ends_with("final suffix"));
    }

    #[test]
    fn cursors_bounds_cache_and_page_seams() {
        let mut view = HistoryView::new(999);
        fn cursor(view: &HistoryView, load: Load) -> (Option<i64>, Option<i64>, usize) {
            match view.command(load) {
                Command::Transcript {
                    before,
                    after,
                    limit,
                } => (before, after, limit),
                _ => panic!("wrong command"),
            }
        }
        assert_eq!(cursor(&view, Load::Latest), (None, None, 8));
        assert_eq!(cursor(&view, Load::Oldest), (None, Some(0), 8));
        view.apply(
            (17..=24).map(|i| entry(i, format!("message{i}"))).collect(),
            Load::Latest,
        );
        assert!(plain(view.lines(80, 3)).contains("message24"));
        assert!(view.follows_tail());
        assert_eq!(cursor(&view, Load::Older), (Some(17), None, 8));
        assert_eq!(cursor(&view, Load::Newer), (None, Some(24), 8));
        view.scroll(-999, 80, 3);
        view.pending = 0;
        let seam = plain(view.lines(80, 3));
        view.apply(
            (9..=17).map(|i| entry(i, format!("message{i}"))).collect(),
            Load::Older,
        );
        assert_eq!(view.messages.len(), CACHE_MESSAGES);
        assert_eq!(plain(view.lines(80, 3)), seam);
        view.apply(
            (1..=8).map(|i| entry(i, format!("message{i}"))).collect(),
            Load::Older,
        );
        assert_eq!(view.messages.first().unwrap().entry.seq, 1);
        assert_eq!(view.messages.last().unwrap().entry.seq, 16);
        assert!(!view.newest);
        assert_eq!(
            view.scroll(-999, 80, 3).map(|l| format!("{l:?}")),
            Some("Older".into())
        );
        view.apply(vec![], Load::Older);
        assert!(view.scroll(-1, 80, 3).is_none());
        assert!(view.oldest);
        view.apply(
            (1..=8).map(|i| entry(i, format!("message{i}"))).collect(),
            Load::Oldest,
        );
        assert!(plain(view.lines(80, 3)).contains("message1"));
        assert!(!view.follows_tail());
        view.scroll(999, 80, 3);
        view.pending = 0;
        let seam = plain(view.lines(80, 3));
        view.apply(
            (8..=16).map(|i| entry(i, format!("message{i}"))).collect(),
            Load::Newer,
        );
        assert_eq!(plain(view.lines(80, 3)), seam);
        view.apply(
            (17..=24).map(|i| entry(i, format!("message{i}"))).collect(),
            Load::Newer,
        );
        assert_eq!(view.messages.first().unwrap().entry.seq, 9);
        assert!(!view.oldest);
        view.scroll(999, 80, 3);
        view.apply(vec![], Load::Newer);
        assert!(view.scroll(1, 80, 3).is_none());
        assert!(view.follows_tail());
    }

    #[test]
    fn requested_page_consumes_scroll_without_skipping_seam() {
        let mut view = HistoryView::new(1);
        view.apply(vec![entry(2, "second")], Load::Latest);
        let load = (0..10).find_map(|_| view.scroll(-1, 80, 1));
        assert!(matches!(load, Some(Load::Older)));
        view.apply(vec![entry(1, "first")], Load::Older);
        assert_ne!(view.top.unwrap().seq, 2);
        view.apply(vec![entry(1, "first")], Load::Oldest);
        assert!(matches!(view.scroll(99, 80, 1), Some(Load::Newer)));
        view.apply(vec![entry(2, "second")], Load::Newer);
        assert!(plain(view.lines(80, 1)).contains("second") || view.top.unwrap().seq == 2);
    }

    #[test]
    fn narrow_long_line_exceeds_row_cache_and_can_be_read_backwards() {
        let text = format!("BEGIN{}MIDDLE{}END", "a".repeat(10_000), "b".repeat(10_000));
        let mut view = HistoryView::new(8);
        view.apply(vec![entry(1, text.clone())], Load::Oldest);
        let mut forward = String::new();
        for _ in 0..text.len() + 20 {
            forward.push_str(plain(view.lines(3, 1)).trim());
            let before = view.top;
            view.scroll(1, 3, 1);
            assert!(view.cached_rows <= CACHE_ROWS);
            if view.top == before {
                break;
            }
        }
        assert_eq!(forward, text);
        assert!(!view.wrapped.iter().any(|w| w.chunk == 0));
        let mut backward = String::new();
        for _ in 0..text.len() + 20 {
            backward.push_str(plain(view.lines(3, 1)).trim());
            let before = view.top;
            view.scroll(-1, 3, 1);
            assert!(view.cached_rows <= CACHE_ROWS);
            if view.top == before {
                break;
            }
        }
        assert_eq!(backward.chars().rev().collect::<String>(), text);
    }

    #[test]
    fn stable_frames_reuse_nearby_rows_and_width_changes_rewrap() {
        let mut view = HistoryView::new(8);
        view.apply(vec![entry(1, "x".repeat(50_000))], Load::Oldest);
        view.scroll(500, 3, 3);
        view.lines(3, 3);
        let top = view.top.unwrap();
        let rows = view.rows(top).as_ptr();
        for _ in 0..10 {
            view.lines(3, 3);
            assert_eq!(view.rows(top).as_ptr(), rows);
        }
        assert!(!view.wrapped.iter().any(|w| w.chunk > 2));
        let narrow_count = view.rows(top).len();
        view.lines(80, 3);
        assert!(view.rows(view.top.unwrap()).len() < narrow_count);
        assert_eq!(view.width, 80);
        view.apply(vec![entry(1, "\t".repeat(50_000))], Load::Oldest);
        assert_eq!(view.lines(1, 3).len(), 3);
        assert!(view.cached_rows <= CACHE_ROWS);
    }

    #[test]
    fn growing_partial_segment_does_not_move_existing_row_anchor() {
        let mut running = entry(1, format!("head\n{}", "01234567".repeat(120)));
        running.status = "running".into();
        let mut view = HistoryView::new(8);
        view.apply(vec![running.clone()], Load::Latest);
        view.scroll(-10, 10, 3);
        let before = plain(view.lines(10, 3));
        assert!(view.top.unwrap().row > 10);
        view.sync_active(Some(&ActiveView {
            attempt_id: running.attempt_id,
            text: format!("{}{}", running.text, "next".repeat(100)),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        }));
        assert_eq!(plain(view.lines(10, 3)), before);
        assert!(!view.follows_tail());
    }

    #[test]
    fn arrows_cross_pages_in_both_directions_after_message_eviction() {
        let all = (1..=32)
            .map(|i| entry(i, format!("message {i:02}")))
            .collect::<Vec<_>>();
        let mut view = HistoryView::new(8);
        view.apply(all[..8].to_vec(), Load::Oldest);
        for delta in [1, -1] {
            let mut seen = String::new();
            for _ in 0..500 {
                seen.push_str(&plain(view.lines(80, 1)));
                let before = view.top;
                if let Some(load) = view.scroll(delta, 80, 1) {
                    let Command::Transcript {
                        before,
                        after,
                        limit,
                    } = view.command(load)
                    else {
                        panic!("wrong command");
                    };
                    let mut page = all
                        .iter()
                        .filter(|e| {
                            before.is_none_or(|b| e.seq < b) && after.is_none_or(|a| e.seq > a)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if before.is_some() {
                        page.drain(..page.len().saturating_sub(limit));
                    } else {
                        page.truncate(limit);
                    }
                    view.apply(page, load);
                }
                assert!(view.messages.len() <= CACHE_MESSAGES);
                if view.top == before {
                    break;
                }
            }
            for i in 1..=32 {
                assert!(
                    seen.contains(&format!("message {i:02}")),
                    "direction {delta}, message {i}"
                );
            }
        }
        assert!(view.oldest);
    }
}

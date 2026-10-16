//! Bounded, keymap-independent composer. The caller sanitizes text before editing
//! and renders the returned lines without further wrapping or tab expansion.

use std::ops::Range;

use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

const TAB_STOP: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Insertion {
    pub(super) inserted_bytes: usize,
    pub(super) truncated: bool,
}

#[derive(Debug, Default)]
pub(super) struct Rows {
    pub(super) lines: Vec<Line<'static>>,
    /// Coordinates relative to `lines`; absent when either dimension is zero.
    pub(super) cursor: Option<(u16, u16)>,
    pub(super) hidden_above: usize,
    pub(super) hidden_below: usize,
}

/// Movement returns whether the caret changed; backspace/delete methods return
/// whether text changed. Setters and insertion report validity or accepted input.
#[derive(Debug)]
pub(super) struct Editor {
    text: String,
    cursor: usize,
    max_bytes: usize,
    desired_column: Option<(u16, u16)>,
}

#[derive(Clone, Copy, Debug)]
struct Caret {
    byte: usize,
    x: u16,
    y: usize,
}

struct Layout {
    lines: Vec<String>,
    carets: Vec<Caret>,
}

impl Editor {
    /// Uses the caller's exact byte limit, including for text setters.
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            max_bytes,
            desired_column: None,
        }
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(super) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Rejects out-of-range offsets and offsets inside an extended grapheme.
    pub(super) fn set_cursor(&mut self, byte: usize) -> bool {
        if !self.is_boundary(byte) {
            return false;
        }
        self.move_to(byte);
        true
    }

    /// Replaces all text and places the caret at the accepted text's end.
    pub(super) fn set_text(&mut self, text: &str) -> Insertion {
        self.replace(0..self.text.len(), text).unwrap()
    }

    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.move_to(0);
    }

    pub(super) fn insert(&mut self, text: &str) -> Insertion {
        self.replace(self.cursor..self.cursor, text).unwrap()
    }

    /// Completion can replace a byte range without knowing the editor's keymap.
    /// Invalid or non-grapheme-aligned ranges are rejected without mutation.
    /// Accepts only a whole-grapheme prefix that fits; never sanitizes input.
    pub(super) fn replace(&mut self, range: Range<usize>, text: &str) -> Option<Insertion> {
        if range.start > range.end || !self.is_boundary(range.start) || !self.is_boundary(range.end)
        {
            return None;
        }
        let available = self.max_bytes - (self.text.len() - range.len());
        let mut accepted = 0;
        for grapheme in text.graphemes(true) {
            if grapheme.len() > available - accepted {
                break;
            }
            accepted += grapheme.len();
        }
        self.text.replace_range(range.clone(), &text[..accepted]);
        // Inserting a joiner/combining mark or removing a separator can merge
        // surrounding graphemes. Prefer the end of the newly merged grapheme.
        let requested = range.start + accepted;
        let cursor = self
            .text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .find(|&byte| byte >= requested)
            .unwrap_or(self.text.len());
        self.move_to(cursor);
        Some(Insertion {
            inserted_bytes: accepted,
            truncated: accepted < text.len(),
        })
    }

    pub(super) fn backspace(&mut self) -> bool {
        self.remove(self.previous_boundary()..self.cursor)
    }

    pub(super) fn delete(&mut self) -> bool {
        self.remove(self.cursor..self.next_boundary())
    }

    pub(super) fn left(&mut self) -> bool {
        self.move_to(self.previous_boundary())
    }

    pub(super) fn right(&mut self) -> bool {
        self.move_to(self.next_boundary())
    }

    /// Word movement skips punctuation/whitespace using Unicode word boundaries.
    pub(super) fn word_left(&mut self) -> bool {
        self.move_to(self.word_start())
    }

    pub(super) fn word_right(&mut self) -> bool {
        self.move_to(self.word_end())
    }

    pub(super) fn delete_word_left(&mut self) -> bool {
        self.remove(self.word_start()..self.cursor)
    }

    pub(super) fn delete_word_right(&mut self) -> bool {
        self.remove(self.cursor..self.word_end())
    }

    /// Home/end address logical lines, not soft-wrapped visual rows.
    pub(super) fn home(&mut self) -> bool {
        self.move_to(self.line_start())
    }

    pub(super) fn end(&mut self) -> bool {
        self.move_to(self.line_end())
    }

    pub(super) fn delete_to_line_start(&mut self) -> bool {
        self.remove(self.line_start()..self.cursor)
    }

    pub(super) fn delete_to_line_end(&mut self) -> bool {
        self.remove(self.cursor..self.line_end())
    }

    pub(super) fn up(&mut self, width: u16) -> bool {
        self.vertical(width, false)
    }

    pub(super) fn down(&mut self, width: u16) -> bool {
        self.vertical(width, true)
    }

    /// Returns a caret-centered window, including an empty row for a caret at
    /// an exact wrap boundary. Width/height zero returns no rows or cursor.
    /// Tabs use eight-cell stops, capped to the row width, and wrap as a unit.
    /// A grapheme wider than the viewport uses a one-cell replacement glyph.
    /// Only the returned rows are limited: layout scans and allocates for all
    /// editor text, as does vertical movement, not just the visible viewport.
    pub(super) fn rows(&self, width: u16, max_visible: u16) -> Rows {
        if width == 0 || max_visible == 0 {
            return Rows::default();
        }
        let layout = self.layout(width);
        let caret = layout.carets[self.caret_index(&layout)];
        let visible = usize::from(max_visible).min(layout.lines.len());
        let top = caret
            .y
            .saturating_sub(visible / 2)
            .min(layout.lines.len() - visible);
        Rows {
            cursor: Some((caret.x, (caret.y - top) as u16)),
            hidden_above: top,
            hidden_below: layout.lines.len() - top - visible,
            lines: layout
                .lines
                .into_iter()
                .skip(top)
                .take(visible)
                .map(Line::raw)
                .collect(),
        }
    }

    fn is_boundary(&self, byte: usize) -> bool {
        byte == self.text.len()
            || self
                .text
                .grapheme_indices(true)
                .any(|(start, _)| start == byte)
    }

    fn previous_boundary(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(byte, _)| byte)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    fn word_start(&self) -> usize {
        self.text
            .unicode_word_indices()
            .take_while(|(byte, _)| *byte < self.cursor)
            .last()
            .map_or(0, |(byte, _)| byte)
    }

    fn word_end(&self) -> usize {
        self.text
            .unicode_word_indices()
            .map(|(byte, word)| byte + word.len())
            .find(|&byte| byte > self.cursor)
            .unwrap_or(self.text.len())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |byte| byte + 1)
    }

    fn line_end(&self) -> usize {
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |byte| self.cursor + byte);
        // CRLF is a single grapheme; stop before both bytes.
        if end < self.text.len() && end > self.cursor && self.text.as_bytes()[end - 1] == b'\r' {
            end - 1
        } else {
            end
        }
    }

    fn move_to(&mut self, byte: usize) -> bool {
        self.desired_column = None;
        let changed = self.cursor != byte;
        self.cursor = byte;
        changed
    }

    fn remove(&mut self, range: Range<usize>) -> bool {
        let changed = !range.is_empty();
        self.replace(range, "").unwrap();
        changed
    }

    fn caret_index(&self, layout: &Layout) -> usize {
        layout
            .carets
            .binary_search_by_key(&self.cursor, |caret| caret.byte)
            .expect("cursor must be a grapheme boundary")
    }

    fn vertical(&mut self, width: u16, down: bool) -> bool {
        if width == 0 {
            return false;
        }
        let layout = self.layout(width);
        let caret = layout.carets[self.caret_index(&layout)];
        let column = match self.desired_column {
            Some((previous_width, column)) if previous_width == width => column,
            _ => caret.x,
        };
        self.desired_column = Some((width, column));
        let target_row = if down {
            (caret.y + 1).min(layout.lines.len() - 1)
        } else {
            caret.y.saturating_sub(1)
        };
        if target_row == caret.y {
            return false;
        }
        let target = layout
            .carets
            .iter()
            .filter(|c| c.y == target_row && c.x <= column)
            .max_by_key(|c| c.x)
            .expect("each visual row has a caret at column zero");
        let changed = self.cursor != target.byte;
        self.cursor = target.byte;
        changed
    }

    fn layout(&self, width: u16) -> Layout {
        let width = usize::from(width);
        let mut layout = Layout {
            lines: vec![String::new()],
            carets: vec![Caret {
                byte: 0,
                x: 0,
                y: 0,
            }],
        };
        let mut x = 0;
        let mut wrapped = false;
        for (byte, grapheme) in self.text.grapheme_indices(true) {
            if matches!(grapheme, "\n" | "\r\n") {
                // Exact soft wrap already created the next logical line's row.
                if !wrapped {
                    layout.lines.push(String::new());
                }
                x = 0;
                wrapped = false;
            } else {
                let tab = grapheme == "\t";
                let natural_width = if tab {
                    (TAB_STOP - x % TAB_STOP).min(width)
                } else {
                    Line::raw(grapheme).width()
                };
                let cells = if natural_width > width {
                    1
                } else {
                    natural_width
                };
                if x + cells > width {
                    layout.lines.push(String::new());
                    x = 0;
                    let before = layout.carets.last_mut().unwrap();
                    before.x = 0;
                    before.y = layout.lines.len() - 1;
                }
                let cells = if tab {
                    (TAB_STOP - x % TAB_STOP).min(width)
                } else {
                    cells
                };
                let line = layout.lines.last_mut().unwrap();
                if tab {
                    line.extend(std::iter::repeat_n(' ', cells));
                } else if natural_width > width {
                    line.push('\u{fffd}');
                } else {
                    line.push_str(grapheme);
                }
                x += cells;
                wrapped = x == width;
                if wrapped {
                    layout.lines.push(String::new());
                    x = 0;
                }
            }
            layout.carets.push(Caret {
                byte: byte + grapheme.len(),
                x: x as u16,
                y: layout.lines.len() - 1,
            });
        }
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::new(4096);
        assert!(!editor.set_text(text).truncated);
        editor
    }

    fn plain(rows: &Rows) -> Vec<String> {
        rows.lines.iter().map(ToString::to_string).collect()
    }

    fn assert_boundary(editor: &Editor) {
        assert!(editor.is_boundary(editor.cursor()));
        assert!(editor.text().len() <= editor.max_bytes());
    }

    #[test]
    fn empty_and_edge_operations() {
        let mut e = editor("");
        assert_eq!(e.cursor(), 0);
        assert!(!e.left());
        assert!(!e.right());
        assert!(!e.backspace());
        assert!(!e.delete());
        assert!(!e.up(20));
        assert!(!e.down(20));
        assert_eq!(plain(&e.rows(20, 3)), [""]);
        assert_eq!(e.rows(20, 3).cursor, Some((0, 0)));
        assert!(e.rows(0, 3).cursor.is_none());
        assert!(e.rows(20, 0).lines.is_empty());
        assert!(!e.down(0));
    }

    #[test]
    fn edits_at_cursor_not_only_at_tail() {
        let mut e = editor("ac");
        e.left();
        assert_eq!(e.insert("b").inserted_bytes, 1);
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 2);
        assert!(e.delete());
        assert_eq!(e.text(), "ab");
        assert!(e.backspace());
        assert_eq!(e.text(), "a");
        e.clear();
        assert_eq!(e.text(), "");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn movement_and_deletion_use_extended_graphemes() {
        let graphemes = [
            "a\u{301}",
            "\u{1f469}\u{200d}\u{1f4bb}",
            "\u{1f1fa}\u{1f1f8}",
        ];
        let text = graphemes.concat();
        let mut e = editor(&text);
        for g in graphemes.iter().rev() {
            let old = e.cursor();
            assert!(e.left());
            assert_eq!(old - e.cursor(), g.len());
        }
        for g in graphemes {
            let old = e.cursor();
            assert!(e.right());
            assert_eq!(e.cursor() - old, g.len());
        }
        assert!(e.backspace());
        assert_eq!(e.text(), graphemes[..2].concat());
        e.home();
        assert!(e.delete());
        assert_eq!(e.text(), graphemes[1]);
    }

    #[test]
    fn insertion_and_deletion_snap_merged_graphemes_forward() {
        let mut e = editor("a\u{1f469}\u{1f4bb}z");
        e.set_cursor(1 + "\u{1f469}".len());
        e.insert("\u{200d}");
        assert_eq!(e.cursor(), "a\u{1f469}\u{200d}\u{1f4bb}".len());
        assert_boundary(&e);
        e.set_cursor(1);
        e.insert("\u{301}");
        assert_eq!(e.cursor(), "a\u{301}".len());
        assert_boundary(&e);

        let mut e = editor("\rX\n");
        e.set_cursor(1);
        e.delete();
        assert_eq!(e.text(), "\r\n");
        assert_eq!(e.cursor(), 2);
        assert_boundary(&e);
    }

    #[test]
    fn limits_do_not_split_utf8_or_graphemes() {
        let mut e = Editor::new(4);
        assert_eq!(
            e.insert("a\u{1f600}b"),
            Insertion {
                inserted_bytes: 1,
                truncated: true
            }
        );
        assert_eq!(e.text(), "a");
        assert!(!e.insert("e\u{301}").truncated);
        assert_eq!(e.text().len(), 4);
        assert!(e.insert("x").truncated);
        assert_eq!(e.set_text("\u{1f1fa}\u{1f1f8}").inserted_bytes, 0);
        assert_eq!(e.text(), "");
        assert_eq!(Editor::new(usize::MAX).max_bytes(), usize::MAX);
        assert!(Editor::new(0).insert("x").truncated);
    }

    #[test]
    fn configured_limit_bounds_text_and_render_keeps_the_caret_visible() {
        let max_bytes = 128 * 1024;
        let mut e = Editor::new(max_bytes);
        let input = "\n".repeat(max_bytes + 10);
        let inserted = e.set_text(&input);
        assert_eq!(inserted.inserted_bytes, max_bytes);
        assert!(inserted.truncated);
        let rows = e.rows(1, 2);
        assert_eq!(plain(&rows), ["", ""]);
        assert_eq!(rows.cursor, Some((0, 1)));
        assert_eq!((rows.hidden_above, rows.hidden_below), (max_bytes - 1, 0));
    }

    #[test]
    fn queued_message_at_one_mib_limit_is_preserved_through_editing() {
        let max_bytes = 1024 * 1024;
        let text = "x".repeat(max_bytes);
        let mut e = Editor::new(max_bytes);
        assert_eq!(e.max_bytes(), max_bytes);
        assert_eq!(
            e.set_text(&text),
            Insertion {
                inserted_bytes: max_bytes,
                truncated: false,
            }
        );
        assert_eq!(e.text(), text);
        assert_eq!(e.cursor(), max_bytes);
        assert!(e.backspace());
        assert!(!e.insert("x").truncated);
        assert_eq!(e.text(), text);

        let offset = 70 * 1024;
        assert!(!e.replace(offset..offset + 4, "edit").unwrap().truncated);
        assert_eq!(e.text().len(), max_bytes);
        assert_eq!(&e.text()[..offset], &text[..offset]);
        assert_eq!(&e.text()[offset..offset + 4], "edit");
        assert_eq!(&e.text()[offset + 4..], &text[offset + 4..]);
        assert_eq!(e.cursor(), offset + 4);
        assert_boundary(&e);
    }

    #[test]
    fn replacement_uses_freed_capacity_and_validates_ranges() {
        let mut e = Editor::new(6);
        e.set_text("/mod x");
        assert_eq!(e.replace(0..4, "/mode").unwrap().inserted_bytes, 4);
        assert_eq!(e.text(), "/mod x");
        assert_eq!(e.cursor(), 4);
        assert!(e.replace(0..99, "x").is_none());
        let reversed = Range { start: 3, end: 1 };
        assert!(e.replace(reversed, "x").is_none());
        e.set_text("e\u{301}z");
        let before = (e.text().to_owned(), e.cursor());
        assert!(e.replace(1..3, "x").is_none());
        assert!(!e.set_cursor(1));
        assert!(!e.set_cursor(2));
        assert!(!e.set_cursor(99));
        assert_eq!((e.text().to_owned(), e.cursor()), before);
        assert_eq!(e.replace(0..3, "abcd").unwrap().inserted_bytes, 4);
        assert_eq!(e.text(), "abcdz");
    }

    #[test]
    fn insertion_is_not_sanitization() {
        let mut e = editor("");
        let text = "\x1b[31m\r\n\t\0";
        assert!(!e.insert(text).truncated);
        assert_eq!(e.text(), text);
    }

    #[test]
    fn word_movement_skips_separators_in_both_directions() {
        let mut e = editor("one,  \u{e9}lan! two   ");
        assert!(e.word_left());
        assert_eq!(&e.text()[e.cursor()..], "two   ");
        e.word_left();
        assert_eq!(&e.text()[e.cursor()..], "\u{e9}lan! two   ");
        e.word_left();
        assert_eq!(e.cursor(), 0);
        e.word_right();
        assert_eq!(&e.text()[..e.cursor()], "one");
        e.word_right();
        assert_eq!(&e.text()[..e.cursor()], "one,  \u{e9}lan");
        e.word_right();
        e.word_right();
        assert_eq!(e.cursor(), e.text().len());
        assert!(!e.word_right());
    }

    #[test]
    fn word_movement_uses_unicode_words_without_splitting_graphemes() {
        let mut e = editor("na\u{00ef}ve, \u{65e5}\u{672c} cafe\u{301}!");
        for word in ["cafe\u{301}", "\u{672c}", "\u{65e5}", "na\u{00ef}ve"] {
            assert!(e.word_left());
            assert!(e.text()[e.cursor()..].starts_with(word));
            assert_boundary(&e);
        }
        assert!(!e.word_left());
        for prefix in [
            "na\u{00ef}ve",
            "na\u{00ef}ve, \u{65e5}",
            "na\u{00ef}ve, \u{65e5}\u{672c}",
            "na\u{00ef}ve, \u{65e5}\u{672c} cafe\u{301}",
        ] {
            assert!(e.word_right());
            assert_eq!(&e.text()[..e.cursor()], prefix);
            assert_boundary(&e);
        }
        assert!(e.word_right());
        assert_eq!(e.cursor(), e.text().len());
        assert!(!e.word_right());
    }

    #[test]
    fn word_deletion_removes_intervening_whitespace_and_punctuation() {
        let mut e = editor("one,  two   ");
        assert!(e.delete_word_left());
        assert_eq!(e.text(), "one,  ");
        e.home();
        assert!(e.delete_word_right());
        assert_eq!(e.text(), ",  ");
        assert!(e.delete_word_right());
        assert_eq!(e.text(), "");
        assert!(!e.delete_word_left());
    }

    #[test]
    fn word_operations_remain_on_grapheme_boundaries() {
        for text in [
            "\u{600}abc def",
            "a\u{200d}\u{1f469} b",
            "\u{915}\u{94d}\u{200d}\u{937} x",
            "\u{301}a\u{301} x",
        ] {
            let boundaries: Vec<_> = text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain([text.len()])
                .collect();
            for byte in boundaries {
                for operation in [
                    Editor::word_left,
                    Editor::word_right,
                    Editor::delete_word_left,
                    Editor::delete_word_right,
                ] {
                    let mut e = editor(text);
                    e.set_cursor(byte);
                    operation(&mut e);
                    assert_boundary(&e);
                }
            }
        }
    }

    #[test]
    fn home_end_and_line_deletion_are_logical_and_crlf_aware() {
        let mut e = editor("abcdef\r\nxy\n");
        e.set_cursor(4);
        e.home();
        assert_eq!(e.cursor(), 0);
        e.end();
        assert_eq!(e.cursor(), 6);
        e.right();
        assert_eq!(e.cursor(), 8);
        e.end();
        assert_eq!(e.cursor(), 10);
        e.delete_to_line_start();
        assert_eq!(e.text(), "abcdef\r\n\n");
        e.set_cursor(2);
        e.delete_to_line_end();
        assert_eq!(e.text(), "ab\r\n\n");
        e.set_cursor(e.text().len());
        assert!(!e.home());
        assert!(!e.end());
        e.set_text("abc\r");
        e.home();
        e.end();
        assert_eq!(e.cursor(), 4);
    }

    #[test]
    fn exact_wrap_has_downstream_caret_and_no_double_newline() {
        let mut e = editor("abcd");
        let rows = e.rows(4, 10);
        assert_eq!(plain(&rows), ["abcd", ""]);
        assert_eq!(rows.cursor, Some((0, 1)));
        e.set_text("abcd\nx");
        assert_eq!(plain(&e.rows(4, 10)), ["abcd", "x"]);
        e.set_cursor(4);
        assert_eq!(e.rows(4, 10).cursor, Some((0, 1)));
        e.set_cursor(5);
        assert_eq!(e.rows(4, 10).cursor, Some((0, 1)));
        e.set_text("abcd\n\n");
        assert_eq!(plain(&e.rows(4, 10)), ["abcd", "", ""]);
    }

    #[test]
    fn wide_graphemes_wrap_before_overflow_without_splitting() {
        let mut e = editor("abc\u{754c}z");
        e.set_cursor(3);
        let rows = e.rows(4, 10);
        assert_eq!(plain(&rows), ["abc", "\u{754c}z"]);
        assert_eq!(rows.cursor, Some((0, 1)));
        e.right();
        assert_eq!(e.rows(4, 10).cursor, Some((2, 1)));
        assert_eq!(plain(&editor("\u{754c}").rows(1, 10)), ["\u{fffd}", ""]);
    }

    #[test]
    fn tab_stops_and_wrap_use_the_same_cursor_geometry() {
        let mut e = editor("a\tb");
        assert_eq!(plain(&e.rows(10, 10)), ["a       b"]);
        e.set_cursor(2);
        assert_eq!(e.rows(10, 10).cursor, Some((8, 0)));
        let rows = e.rows(4, 10);
        assert_eq!(plain(&rows), ["a", "    ", "b"]);
        assert_eq!(rows.cursor, Some((0, 2)));
        e.left();
        assert_eq!(e.rows(4, 10).cursor, Some((0, 1)));
        e.set_text("\t\t");
        assert_eq!(plain(&e.rows(10, 10)), ["        ", "        "]);
    }

    #[test]
    fn vertical_movement_preserves_column_across_short_rows() {
        let mut e = editor("abcdef\nx\nabcdef");
        e.set_cursor(5);
        e.down(20);
        assert_eq!(e.cursor(), 8);
        e.down(20);
        assert_eq!(e.cursor(), 14);
        assert!(!e.down(20));
        e.up(20);
        assert_eq!(e.cursor(), 8);
        e.up(20);
        assert_eq!(e.cursor(), 5);
        e.down(20);
        e.left();
        e.down(20);
        assert_eq!(e.cursor(), 9);
    }

    #[test]
    fn vertical_motion_uses_cells_not_bytes_and_resets_on_resize() {
        let mut e = editor("ab\n\u{754c}z\na\tb");
        e.set_cursor(2);
        e.down(10);
        assert_eq!(e.cursor(), 3 + "\u{754c}".len());
        e.down(10);
        assert_eq!(&e.text()[..e.cursor()], "ab\n\u{754c}z\na");
        e.up(10);
        assert_eq!(e.rows(10, 10).cursor, Some((2, 1)));
        e.down(2);
        assert_boundary(&e);
    }

    #[test]
    fn vertical_movement_crosses_soft_wraps_and_blank_lines() {
        let mut e = editor("abcdefgh\n\nx");
        e.set_cursor(2);
        e.down(4);
        assert_eq!(e.cursor(), 6);
        e.down(4);
        assert_eq!(e.rows(4, 10).cursor, Some((0, 2)));
        e.down(4);
        assert_eq!(e.rows(4, 10).cursor, Some((1, 3)));
        e.up(4);
        e.up(4);
        assert_eq!(e.cursor(), 6);
    }

    #[test]
    fn window_tracks_caret_instead_of_always_showing_tail() {
        let mut e = editor("0\n1\n2\n3\n4\n5\n6");
        e.set_cursor(6);
        let rows = e.rows(10, 3);
        assert_eq!(plain(&rows), ["2", "3", "4"]);
        assert_eq!(rows.cursor, Some((0, 1)));
        assert_eq!((rows.hidden_above, rows.hidden_below), (2, 2));
        e.set_cursor(0);
        let rows = e.rows(10, 2);
        assert_eq!(plain(&rows), ["0", "1"]);
        assert_eq!((rows.hidden_above, rows.hidden_below), (0, 5));
        e.set_cursor(e.text().len());
        assert_eq!(plain(&e.rows(10, 2)), ["5", "6"]);
    }

    #[test]
    fn layout_invariants_at_every_boundary_and_narrow_width() {
        for text in [
            "",
            "abc\n\n",
            "a\tb\t\nc",
            "\r\n\r\n",
            "a\u{301}\u{754c}\u{1f469}\u{200d}\u{1f4bb}z",
            "\u{1f1fa}\u{1f1f8}\u{1f1e8}\u{1f1e6}",
            "\u{301}\u{200b}x",
        ] {
            let mut e = editor(text);
            let boundaries: Vec<_> = text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain([text.len()])
                .collect();
            for width in 1..=12 {
                for &byte in &boundaries {
                    assert!(e.set_cursor(byte));
                    let rows = e.rows(width, 3);
                    let (x, y) = rows.cursor.unwrap();
                    assert!(x < width);
                    assert!(usize::from(y) < rows.lines.len());
                    assert!(
                        rows.lines
                            .iter()
                            .all(|line| line.width() <= usize::from(width))
                    );
                    e.up(width);
                    assert_boundary(&e);
                    e.down(width);
                    assert_boundary(&e);
                }
            }
        }
    }
}

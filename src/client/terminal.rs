use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    text::{Line, Text},
    widgets::Paragraph,
};

pub(super) type Term = Terminal<CursorBackend<CrosstermBackend<Stdout>>>;

// After startup the UI is the only writer. A second DSR reader would compete
// with EventStream; keep the cursor known from our own writes instead.
#[derive(Clone)]
pub(super) struct CursorBackend<B> {
    inner: B,
    cursor: Position,
}

impl<B: Backend> Backend for CursorBackend<B> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut cursor = self.cursor;
        self.inner
            .draw(content.inspect(|(x, y, _)| cursor = Position::new(*x, *y)))?;
        self.cursor = cursor;
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)?;
        self.cursor.x = 0;
        self.cursor.y = self
            .cursor
            .y
            .saturating_add(n)
            .min(self.inner.size()?.height.saturating_sub(1));
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let size = self.inner.size()?;
        Ok(Position::new(
            self.cursor.x.min(size.width.saturating_sub(1)),
            self.cursor.y.min(size.height.saturating_sub(1)),
        ))
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.cursor = position;
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn set_inline_height<B: Backend>(
    terminal: &mut Terminal<B>,
    current_height: &mut u16,
    height: u16,
    new_backend: impl FnOnce(&B) -> B,
) -> io::Result<()> {
    let physical_height = terminal.size()?.height;
    let height = height.min(physical_height);
    if *current_height == height {
        return Ok(());
    }
    let mut anchor = terminal.get_frame().area().as_position();
    anchor.y = anchor.y.min(physical_height.saturating_sub(1));
    // Inline construction queries the cursor with zero offset, not the old caret offset.
    // Clear before shrinking so rows outside the replacement viewport are also erased.
    terminal.set_cursor_position(anchor)?;
    terminal
        .backend_mut()
        .clear_region(ClearType::AfterCursor)?;
    terminal.backend_mut().flush()?;
    *terminal = Terminal::with_options(
        new_backend(terminal.backend()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    *current_height = height;
    Ok(())
}

fn publish_rows<B: Backend>(terminal: &mut Terminal<B>, rows: &[Line<'static>]) -> io::Result<()> {
    // Prewrap into bounded rows so insert_before never allocates a huge buffer.
    terminal.autoresize()?;
    for chunk in rows.chunks(64) {
        terminal.insert_before(chunk.len() as u16, |buffer| {
            use ratatui::widgets::Widget;
            Paragraph::new(Text::from(chunk.to_vec())).render(buffer.area, buffer);
        })?;
    }
    Ok(())
}

// The UI is the only terminal writer. Drop also runs on errors and unwinding.
pub(super) struct Screen {
    pub(super) inline: Term,
    pub(super) full: Option<Term>,
    height: u16,
    mouse: bool,
}

pub(super) struct Restore;

/// Keep resize, native publication and drawing in one visible terminal update.
pub(super) struct FrameGuard {
    active: bool,
}

impl FrameGuard {
    pub(super) fn finish(mut self) -> Result<()> {
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(io::stdout(), EndSynchronizedUpdate);
        }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            EndSynchronizedUpdate,
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture,
            Show
        );
        let _ = disable_raw_mode();
    }
}

impl Screen {
    pub(super) fn begin_frame(&self) -> Result<FrameGuard> {
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        Ok(FrameGuard { active: true })
    }

    pub(super) fn open() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        execute!(io::stdout(), EnableBracketedPaste)?;
        let mut inner = CrosstermBackend::new(io::stdout());
        let cursor = inner.get_cursor_position()?;
        let backend = CursorBackend { inner, cursor };
        // The initial empty composer needs two blank separators and one status row.
        let height = 4.min(backend.size()?.height);
        let inline = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        Ok(Self {
            inline,
            full: None,
            height,
            mouse: false,
        })
    }

    pub(super) fn fullscreen(&mut self, enabled: bool) -> Result<()> {
        if enabled && self.full.is_none() {
            execute!(io::stdout(), EnterAlternateScreen)?;
            let mut terminal = Terminal::new(CursorBackend {
                inner: CrosstermBackend::new(io::stdout()),
                cursor: Position::ORIGIN,
            })?;
            terminal.clear()?;
            self.full = Some(terminal);
        } else if !enabled && self.full.is_some() {
            self.capture_mouse(false)?;
            self.full = None;
            execute!(io::stdout(), LeaveAlternateScreen)?;
            // Keep the inline caret offset; the emulator restores the main-screen cursor.
            self.inline.autoresize()?;
            self.inline.clear()?;
        }
        Ok(())
    }

    pub(super) fn capture_mouse(&mut self, enabled: bool) -> Result<()> {
        if enabled == self.mouse {
            return Ok(());
        }
        if enabled {
            execute!(io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        self.mouse = enabled;
        Ok(())
    }

    pub(super) fn set_height(&mut self, height: u16) -> Result<()> {
        if self.full.is_some() {
            return Ok(());
        }
        set_inline_height(&mut self.inline, &mut self.height, height, |backend| {
            CursorBackend {
                inner: CrosstermBackend::new(io::stdout()),
                cursor: backend.cursor,
            }
        })?;
        Ok(())
    }

    pub(super) fn publish_rows(&mut self, rows: &[Line<'static>]) -> Result<()> {
        publish_rows(&mut self.inline, rows)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, layout::Rect};

    use super::*;

    #[test]
    fn publication_replaces_live_rows_at_the_same_absolute_composer_above_and_at_bottom()
    -> anyhow::Result<()> {
        use super::super::{render, state::Ui};
        use crate::{
            config::Config,
            runtime::{ActiveView, HistoryEntry, Phase, Snapshot},
        };
        for anchor in [3, 20] {
            let now = std::time::Instant::now();
            let mut snapshot = Snapshot {
                session_id: "session".into(),
                agent_id: "agent".into(),
                conversation_id: "conversation".into(),
                phase: Phase::Waiting,
                phase_started: now,
                last_event: now,
                active: Some(ActiveView {
                    message_id: String::new(),
                    attempt_id: "attempt".into(),
                    text: String::new(),
                    truncated: false,
                    durable_bytes: 0,
                }),
                queue: Vec::new(),
                paused: false,
                recovered: false,
                cursor: 0,
                error: None,
            };
            let mut ui = Ui::new(&Config::default(), &snapshot);
            ui.draft.set_text("retained composer");
            let shell: Vec<_> = (0..anchor)
                .map(|row| format!("shell sentinel {row:02}"))
                .collect();
            let mut inner =
                TestBackend::with_lines(shell.iter().map(|line| format!("{line:<80}")).chain(
                    std::iter::repeat_n(" ".repeat(80), usize::from(24 - anchor)),
                ));
            inner.set_cursor_position((0, anchor))?;
            let mut terminal = Terminal::with_options(
                CursorBackend {
                    inner,
                    cursor: Position::new(0, anchor),
                },
                TerminalOptions {
                    viewport: Viewport::Inline(4),
                },
            )?;
            let mut height = 4;
            assert_eq!(render::inline_height(&ui, &snapshot, 80, 24), 4);
            terminal.draw(|frame| render::render(frame, &ui, &snapshot))?;
            assert_eq!(terminal.get_cursor_position()?.y, anchor + 1);
            ui.pending.insert(
                1,
                HistoryEntry {
                    seq: 1,
                    attempt_id: "attempt".into(),
                    kind: "assistant".into(),
                    text: "head\n".into(),
                    status: "running".into(),
                },
            );
            snapshot.active.as_mut().unwrap().text = "head\n".into();
            ui.observe_live(&snapshot);
            let plan = ui.plan_publication(&snapshot, 80);
            let post = render::publication_height(&ui, &snapshot, 80, 24, &plan);
            set_inline_height(&mut terminal, &mut height, post, Clone::clone)?;
            publish_rows(&mut terminal, plan.rows.as_ref().unwrap())?;
            ui.commit_publication(plan);
            for chunk in 0..16 {
                snapshot
                    .active
                    .as_mut()
                    .unwrap()
                    .text
                    .push_str(&format!("replace-{chunk}-a\nreplace-{chunk}-b\n"));
                ui.observe_live(&snapshot);
                let pre = render::inline_height(&ui, &snapshot, 80, 24);
                assert_eq!(pre, 6);
                set_inline_height(&mut terminal, &mut height, pre, Clone::clone)?;
                terminal.draw(|frame| render::render(frame, &ui, &snapshot))?;
                let composer = terminal.get_cursor_position()?;
                let old_anchor = terminal.get_frame().area().y;
                let plan = ui.plan_publication(&snapshot, 80);
                assert_eq!(plan.rows.as_ref().unwrap().len(), 2);
                let post = render::publication_height(&ui, &snapshot, 80, 24, &plan);
                assert_eq!(post, 4);
                set_inline_height(&mut terminal, &mut height, post, Clone::clone)?;
                assert_eq!(terminal.get_frame().area(), Rect::new(0, old_anchor, 80, 4));
                publish_rows(&mut terminal, plan.rows.as_ref().unwrap())?;
                ui.commit_publication(plan);
                assert_eq!(render::inline_height(&ui, &snapshot, 80, 24), post);
                terminal.draw(|frame| render::render(frame, &ui, &snapshot))?;
                assert_eq!(terminal.get_cursor_position()?, composer);
                assert!(composer.y < 23);
                assert_eq!(terminal.get_frame().area().y, old_anchor + 2);
                let area = terminal.get_frame().area();
                // Shrinking does not pin the composer or keep retired blank rows owned.
                assert!(area.bottom() <= 24);
                if area.bottom() < 24 {
                    assert!((area.bottom()..24).all(|y| {
                        (0..80).all(|x| terminal.backend().inner.buffer()[(x, y)].symbol() == " ")
                    }));
                }
                assert_eq!(
                    terminal.backend().inner.buffer()[(2, area.y - 2)].symbol(),
                    "r"
                );
            }
            let native_height = terminal.get_frame().area().y;
            let backend = &terminal.backend().inner;
            assert!(backend.scrollback().area.height > 0);
            let native_rows: Vec<_> = backend
                .scrollback()
                .content
                .chunks(80)
                .chain(
                    backend
                        .buffer()
                        .content
                        .chunks(80)
                        .take(usize::from(native_height)),
                )
                .map(|row| {
                    row.iter()
                        .map(Cell::symbol)
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            let expected: Vec<_> = shell
                .into_iter()
                .chain([String::new(), "  head".to_owned()])
                .chain((0..16).flat_map(|chunk| {
                    [
                        format!("  replace-{chunk}-a"),
                        format!("  replace-{chunk}-b"),
                    ]
                }))
                .collect();
            // Check the entire ordered native transcript, not an arbitrary glyph
            // whose presence in scrollback depends on how far this fixture scrolled.
            assert_eq!(native_rows, expected);
        }
        Ok(())
    }

    #[test]
    fn queued_user_publication_replaces_the_consumed_preview_without_moving_composer()
    -> anyhow::Result<()> {
        use super::super::{render, state::Ui};
        use crate::{
            config::Config,
            runtime::{ActiveView, HistoryEntry, Phase, QueueItem, QueueState, Snapshot},
        };
        for anchor in [3, 18] {
            let now = std::time::Instant::now();
            let mut snapshot = Snapshot {
                session_id: "session".into(),
                agent_id: "agent".into(),
                conversation_id: "conversation".into(),
                phase: Phase::Waiting,
                phase_started: now,
                last_event: now,
                active: Some(ActiveView {
                    message_id: String::new(),
                    attempt_id: "old".into(),
                    text: String::new(),
                    truncated: false,
                    durable_bytes: 0,
                }),
                queue: vec![QueueItem {
                    id: "queued-id".into(),
                    text: "queued input".into(),
                    revision: 0,
                    state: QueueState::Queued,
                }],
                paused: false,
                recovered: false,
                cursor: 0,
                error: None,
            };
            let mut ui = Ui::new(&Config::default(), &snapshot);
            ui.draft.set_text("retained composer");
            ui.observe_live(&snapshot);
            let mut backend = TestBackend::new(80, 24);
            backend.set_cursor_position((0, anchor))?;
            let mut terminal = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(6),
                },
            )?;
            let mut height = 6;
            terminal.draw(|frame| render::render(frame, &ui, &snapshot))?;
            let composer = terminal.get_cursor_position()?;
            assert_eq!(composer.y, anchor + 3);
            snapshot.active.as_mut().unwrap().attempt_id = "new".into();
            snapshot.active.as_mut().unwrap().message_id = "queued-id".into();
            snapshot.queue.clear();
            ui.observe_live(&snapshot);
            for _ in 0..3 {
                let plan = ui.plan_publication(&snapshot, 80);
                assert!(plan.rows.is_none());
                assert_eq!(render::publication_height(&ui, &snapshot, 80, 24, &plan), 6);
                ui.commit_publication(plan);
                terminal.draw(|frame| render::render(frame, &ui, &snapshot))?;
                assert_eq!(terminal.get_cursor_position()?, composer);
                assert_eq!(
                    render::projected_queue_rows(&ui, &snapshot, 80, 6, None).len(),
                    1
                );
                assert!(snapshot.queue.is_empty());
            }
            ui.pending.insert(
                1,
                HistoryEntry {
                    seq: 1,
                    attempt_id: "new".into(),
                    kind: "user".into(),
                    text: "queued input".into(),
                    status: "accepted".into(),
                },
            );
            assert!(
                ui.publish_with(&snapshot, 80, |_| anyhow::bail!("output failed"))
                    .is_err()
            );
            assert_eq!(render::inline_height(&ui, &snapshot, 80, 24), 6);
            assert_eq!(ui.native_offset, 0);
            assert!(ui.published.is_none());
            let plan = ui.plan_publication(&snapshot, 80);
            assert_eq!(plan.rows.as_ref().unwrap().len(), 2);
            let post = render::publication_height(&ui, &snapshot, 80, 24, &plan);
            assert_eq!(post, 4);
            assert!(render::projected_queue_rows(&ui, &snapshot, 80, 6, Some(&plan)).is_empty());
            set_inline_height(&mut terminal, &mut height, post, Clone::clone)?;
            publish_rows(&mut terminal, plan.rows.as_ref().unwrap())?;
            ui.commit_publication(plan);
            terminal.draw(|frame| render::render(frame, &ui, &snapshot))?;
            assert_eq!(terminal.get_cursor_position()?, composer);
            assert_eq!(terminal.get_frame().area(), Rect::new(0, anchor + 2, 80, 4));
            assert!(render::projected_queue_rows(&ui, &snapshot, 80, 6, None).is_empty());
            let rows: Vec<_> = terminal
                .backend()
                .scrollback()
                .content
                .chunks(80)
                .chain(terminal.backend().buffer().content.chunks(80))
                .map(|row| row.iter().map(Cell::symbol).collect::<String>())
                .collect();
            assert_eq!(
                rows.iter()
                    .filter(|row| row.trim_end() == "\u{2503} queued input")
                    .count(),
                1
            );
            assert!(!rows.iter().any(|row| row.contains("\u{250b}")));
            assert_eq!(ui.published, Some(1));
        }
        Ok(())
    }

    #[test]
    fn dynamic_inline_frames_use_owned_cursor_without_requerying_input() -> io::Result<()> {
        let mut inner = TestBackend::new(40, 24);
        inner.set_cursor_position((0, 0))?;
        let backend = CursorBackend {
            inner,
            cursor: Position::new(0, 4),
        };
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )?;
        assert_eq!(terminal.get_frame().area().y, 4);
        let mut height = 4;
        for desired in [4, 12, 4, 6, 4, 12, 4] {
            set_inline_height(&mut terminal, &mut height, desired, Clone::clone)?;
            assert_eq!(terminal.get_frame().area(), Rect::new(0, 4, 40, desired));
            terminal.draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new("\ninput"), area);
                frame.set_cursor_position((5, area.y + 1));
            })?;
            assert_eq!(terminal.get_cursor_position()?, Position::new(5, 5));
            assert_eq!(
                terminal.backend_mut().inner.get_cursor_position()?,
                Position::new(5, 5)
            );
        }
        Ok(())
    }

    #[test]
    fn inline_height_changes_keep_anchor_and_clear_retired_rows() -> io::Result<()> {
        let mut backend =
            TestBackend::with_lines(["shell above "; 4].into_iter().chain(["            "; 20]));
        backend.set_cursor_position((0, 4))?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )?;
        let mut height = 4;
        for requested in [4, 12, 4, 12, 4] {
            set_inline_height(&mut terminal, &mut height, requested, Clone::clone)?;
            assert_eq!(terminal.get_frame().area(), Rect::new(0, 4, 12, requested));
            terminal.draw(|frame| {
                let area = frame.area();
                let rows: Vec<_> = (0..area.height)
                    .map(|row| Line::from(format!("ui row {row:02}")))
                    .collect();
                frame.render_widget(Paragraph::new(rows), area);
                frame.set_cursor_position((2, area.bottom() - 1));
            })?;
            let expected: Vec<_> = std::iter::repeat_n("shell above ".to_owned(), 4)
                .chain((0..requested).map(|row| format!("ui row {row:02}   ")))
                .chain(std::iter::repeat_n(
                    "            ".to_owned(),
                    (20 - requested).into(),
                ))
                .collect();
            terminal.backend().assert_buffer_lines(expected);
            terminal.backend().assert_scrollback_empty();
        }
        Ok(())
    }

    #[test]
    fn inline_growth_scrolls_only_needed_rows_and_clamps_height() -> io::Result<()> {
        let mut backend = TestBackend::with_lines(
            (0..12)
                .map(|row| format!("shell {row:02}    "))
                .chain(std::iter::repeat_n("            ".to_owned(), 8)),
        );
        backend.set_cursor_position((0, 12))?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(6),
            },
        )?;
        let mut height = 6;
        terminal.set_cursor_position((2, 17))?;
        set_inline_height(&mut terminal, &mut height, 14, Clone::clone)?;
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 6, 12, 14));
        terminal
            .backend()
            .assert_scrollback_lines((0..6).map(|row| format!("shell {row:02}    ")));
        terminal.backend().assert_buffer_lines(
            (6..12)
                .map(|row| format!("shell {row:02}    "))
                .chain(std::iter::repeat_n("            ".to_owned(), 14)),
        );
        set_inline_height(&mut terminal, &mut height, 6, Clone::clone)?;
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 6, 12, 6));
        set_inline_height(&mut terminal, &mut height, u16::MAX, Clone::clone)?;
        assert_eq!(height, 20);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 12, 20));
        terminal
            .backend()
            .assert_scrollback_lines((0..12).map(|row| format!("shell {row:02}    ")));
        terminal.backend_mut().resize(12, 3);
        set_inline_height(&mut terminal, &mut height, 14, Clone::clone)?;
        assert_eq!(height, 3);
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 12, 3));
        assert_eq!(terminal.backend().scrollback().area.height, 12);
        Ok(())
    }
}

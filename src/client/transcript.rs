use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

use super::theme::{divider, muted, notice, strong, warning};
use crate::runtime::HistoryEntry;

pub(crate) fn transcript_rows(
    entry: &HistoryEntry,
    text: &str,
    last: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let text = clean(text, text.len());
    let width = width.saturating_sub(2).max(1);
    let mut rows = if entry.kind == "user" {
        wrapped(&text, width)
            .into_iter()
            .map(|line| {
                let mut line = line.style(strong());
                line.spans.insert(0, Span::styled("\u{2503} ", strong()));
                line
            })
            .collect::<Vec<_>>()
    } else if entry.kind == "notice" {
        let style = match entry.status.as_str() {
            "error" => warning(),
            "status" => muted(),
            _ => notice(),
        };
        wrapped(&text, width)
            .into_iter()
            .map(|mut line| {
                line.spans.insert(0, Span::raw("  "));
                line.style(style)
            })
            .collect()
    } else {
        markdown_rows(&text, width)
    };
    if text.ends_with('\n') {
        rows.pop();
    }
    if text.is_empty() && entry.status != "running" {
        rows.clear();
    }
    if text.is_empty() && entry.status == "running" {
        rows = vec![Line::styled("  waiting for response...", muted())];
    }
    if last
        && entry.kind != "notice"
        && !matches!(entry.status.as_str(), "accepted" | "completed" | "running")
    {
        rows.push(Line::styled(
            format!("  {}", entry.status),
            if entry.status == "failed" {
                warning()
            } else {
                muted()
            },
        ));
    }
    if last {
        rows.push(Line::from(""));
    }
    rows
}

#[cfg(test)]
pub(super) fn message_rows(entry: &HistoryEntry, width: usize, limit: usize) -> Vec<Line<'static>> {
    transcript_rows(entry, &clean(&entry.text, limit), true, width)
}

pub(super) fn markdown_rows(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut code = None;
    let mut rows = Vec::new();
    for text in text.split('\n') {
        let text = text.replace('\t', "    ");
        let fence = text.bytes().take_while(|&ch| ch == b'`').count();
        let separator = if let Some(open) = code {
            if fence >= open && text[fence..].trim().is_empty() {
                code = None;
                Some("")
            } else {
                None
            }
        } else if fence >= 3 {
            code = Some(fence);
            Some(text[fence..].trim())
        } else {
            None
        };
        if let Some(language) = separator {
            let label = if language.is_empty() {
                String::new()
            } else {
                format!("\u{2500} {language} ")
            };
            // Wrap long info strings rather than clipping their text or widening a row.
            for mut line in wrapped(&label, width) {
                let remaining = width.saturating_sub(line.width());
                line.spans
                    .push(Span::styled("\u{2500}".repeat(remaining), divider()));
                line = line.style(muted());
                line.spans.insert(0, Span::raw("  "));
                rows.push(line);
            }
            continue;
        }

        let mut continuation = String::new();
        let line = if code.is_some() {
            Line::styled(text, muted())
        } else {
            let indent = text.len() - text.trim_start_matches(' ').len();
            let mut body = &text[indent..];
            let mut prefix = text[..indent].to_owned();
            while let Some(quoted) = body.strip_prefix('>') {
                prefix.push_str("\u{2502} ");
                body = quoted.strip_prefix(' ').unwrap_or(quoted);
            }
            if prefix.len() > indent {
                continuation = prefix.clone();
            }
            let marker =
                if body.starts_with("- ") || body.starts_with("* ") || body.starts_with("+ ") {
                    2
                } else {
                    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
                    if digits > 0
                        && (body[digits..].starts_with(". ") || body[digits..].starts_with(") "))
                    {
                        digits + 2
                    } else {
                        0
                    }
                };
            if marker > 0 {
                if continuation.is_empty() {
                    continuation = prefix.clone();
                }
                continuation.push_str(&" ".repeat(marker));
                prefix.push_str(&body[..marker]);
                body = &body[marker..];
            }
            let level = body.bytes().take_while(|&ch| ch == b'#').count();
            let style = if (1..=6).contains(&level) && body[level..].starts_with(' ') {
                body = body[level..].trim_start_matches(' ');
                match level {
                    1 => strong().add_modifier(Modifier::UNDERLINED),
                    2 => strong(),
                    3 => Style::default().add_modifier(Modifier::UNDERLINED),
                    4 => muted().add_modifier(Modifier::BOLD),
                    5 => muted().add_modifier(Modifier::UNDERLINED),
                    _ => muted(),
                }
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(prefix, muted())];
            spans.extend(inline_spans(body));
            Line::from(spans).style(style)
        };
        for mut line in wrap_styled(line, width, code.is_none(), &continuation) {
            line.spans.insert(0, Span::raw("  "));
            rows.push(line);
        }
    }
    rows
}

fn inline_spans(mut text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    while !text.is_empty() {
        let next = [
            text.find('`').map(|at| (at, "`", muted())),
            text.find("**").map(|at| (at, "**", strong())),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(at, _, _)| *at);
        let Some((at, marker, style)) = next else {
            spans.push(Span::raw(text.to_owned()));
            break;
        };
        let marker = if marker == "`" {
            let run = text[at..].bytes().take_while(|&ch| ch == b'`').count();
            &text[at..at + run]
        } else {
            marker
        };
        let content = at + marker.len();
        let end = text[content..]
            .match_indices(marker)
            .find(|&(end, _)| {
                !marker.starts_with('`')
                    || (!text[..content + end].ends_with('`')
                        && !text[content + end + marker.len()..].starts_with('`'))
            })
            .map(|(end, _)| end);
        let Some(end) = end else {
            spans.push(Span::raw(text.to_owned()));
            break;
        };
        if at > 0 {
            spans.push(Span::raw(text[..at].to_owned()));
        }
        spans.push(Span::styled(text[content..content + end].to_owned(), style));
        text = &text[content + end + marker.len()..];
    }
    spans
}

fn wrap_styled(
    line: Line<'static>,
    width: usize,
    word_wrap: bool,
    continuation: &str,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let continuation_width = Line::from(continuation).width();
    let continuation = if continuation_width < width {
        continuation
    } else {
        ""
    };
    let text = line.to_string();
    // Segment the whole line, not individual spans: a style boundary may fall
    // inside a combining sequence or an emoji grapheme.
    let graphemes: Vec<_> = text.grapheme_indices(true).collect();
    let mut rows = Vec::new();
    let mut start = 0;
    let mut span_index = 0;
    let mut span_start = 0;
    while start < graphemes.len() {
        let hanging = if start == 0
            || Line::from(graphemes[start].1).width() + Line::from(continuation).width() > width
        {
            ""
        } else {
            continuation
        };
        let available = width - Line::from(hanging).width();
        let mut used = 0;
        let mut end = start;
        let mut word_break = None;
        while end < graphemes.len() {
            let symbol = graphemes[end].1;
            if word_wrap
                && end > start
                && graphemes[end - 1].1.chars().all(char::is_whitespace)
                && !symbol.chars().all(char::is_whitespace)
            {
                word_break = Some(end);
            }
            let cells = Line::from(symbol).width();
            if used + cells > available {
                break;
            }
            used += cells;
            end += 1;
        }
        if end < graphemes.len() {
            end = word_break.unwrap_or(end);
        }
        // A single grapheme wider than the viewport cannot be split losslessly.
        // Isolate it; all other rows, including arbitrarily long words, are bounded.
        end = end.max(start + 1);
        let byte_start = graphemes[start].0;
        let byte_end = graphemes.get(end).map_or(text.len(), |&(at, _)| at);
        let mut spans = Vec::new();
        if !hanging.is_empty() {
            spans.push(Span::styled(hanging.to_owned(), muted()));
        }
        while span_index < line.spans.len() && span_start < byte_end {
            let span = &line.spans[span_index];
            let span_end = span_start + span.content.len();
            let from = byte_start.saturating_sub(span_start);
            let to = byte_end.min(span_end) - span_start;
            if from < to {
                spans.push(Span::styled(span.content[from..to].to_owned(), span.style));
            }
            if span_end > byte_end {
                break;
            }
            span_index += 1;
            span_start = span_end;
        }
        rows.push(Line::from(spans).style(line.style));
        start = end;
    }
    if rows.is_empty() {
        rows.push(Line::from("").style(line.style));
    }
    rows
}

pub(super) fn wrapped(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let line = Line::from(line.replace('\t', "    "));
        rows.extend(wrap_styled(line, width, true, ""));
    }
    rows
}

pub(super) fn tail(text: &str, bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

pub(super) fn clean(text: &str, limit: usize) -> String {
    let (mut text, truncated) = sanitized(text, limit);
    if truncated {
        text.push_str("\n[display truncated]");
    }
    text
}

pub(super) fn sanitized(text: &str, limit: usize) -> (String, bool) {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let ch = if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            '\n'
        } else {
            ch
        };
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        if out.len() + ch.len_utf8() > limit {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(ToString::to_string).collect()
    }

    fn entry(kind: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            seq: 1,
            attempt_id: "attempt".into(),
            kind: kind.into(),
            text: String::new(),
            status: status.into(),
        }
    }

    #[test]
    fn word_wrapping_preserves_spaces_tabs_and_explicit_newlines() {
        assert_eq!(
            strings(&wrapped("hello world again", 8)),
            ["hello ", "world ", "again"]
        );
        assert_eq!(strings(&wrapped("a\n\nb\n", 8)), ["a", "", "b", ""]);
        let text = "  one  two\tthree   ";
        for width in 1..=24 {
            let rows = wrapped(text, width);
            assert_eq!(strings(&rows).concat(), text.replace('\t', "    "));
            assert!(rows.iter().all(|row| row.width() <= width));
        }
        assert_eq!(strings(&wrapped("", 0)), [""]);
    }

    #[test]
    fn long_words_are_bounded_without_losing_text_or_graphemes() {
        let text = format!(
            "{} a\u{0301}\u{1f469}\u{200d}\u{1f680}\u{754c}",
            "x".repeat(20_000)
        );
        let boundaries: Vec<_> = text.grapheme_indices(true).map(|(at, _)| at).collect();
        for width in [2, 3, 8, 79] {
            let rows = wrapped(&text, width);
            assert_eq!(strings(&rows).concat(), text);
            assert!(rows.iter().all(|row| row.width() <= width));
            let mut offset = 0;
            for row in rows {
                offset += row.to_string().len();
                assert!(offset == text.len() || boundaries.binary_search(&offset).is_ok());
            }
        }
    }

    #[test]
    fn styled_wrapping_keeps_line_and_span_styles_and_cross_span_graphemes() {
        let line = Line::from(vec![
            Span::styled("a", strong()),
            Span::styled("\u{0301} hello ", muted()),
            Span::styled("world", strong()),
        ])
        .style(notice());
        let rows = wrap_styled(line, 8, true, "");
        assert_eq!(strings(&rows), ["a\u{0301} hello ", "world"]);
        assert!(rows.iter().all(|row| row.style == notice()));
        assert_eq!(rows[0].spans[0].style, strong());
        assert_eq!(rows[0].spans[1].style, muted());
        assert_eq!(rows[1].spans[0].style, strong());
        assert_eq!(
            strings(&wrap_styled(
                Line::from(vec![Span::raw("a"), Span::styled("\u{0301}b", strong())]),
                1,
                true,
                "",
            )),
            ["a\u{0301}", "b"]
        );
    }

    #[test]
    fn unavoidable_wide_grapheme_is_isolated_in_one_column() {
        let rows = wrapped("a\u{754c}b", 1);
        assert_eq!(strings(&rows), ["a", "\u{754c}", "b"]);
        assert_eq!(rows[1].width(), 2);
        let rows = markdown_rows("> \u{754c}\u{754c}", 3);
        assert!(rows.iter().all(|row| row.width() <= 5));
    }

    #[test]
    fn code_has_full_width_language_rules_without_side_rails() {
        let rows = markdown_rows("```rust\n    let x = `**literal**`;  \n\n```", 32);
        assert_eq!(rows[0].width(), 34);
        assert_eq!(rows.last().unwrap().width(), 34);
        assert!(rows[0].to_string().starts_with("  \u{2500} rust "));
        assert_eq!(rows[1].to_string(), "      let x = `**literal**`;  ");
        assert_eq!(rows[2].to_string(), "  ");
        assert!(rows.iter().all(|row| !row.to_string().contains('\u{2502}')));
        assert_eq!(rows[1].style, muted());
        assert_eq!(rows[0].spans.last().unwrap().style, divider());
    }

    #[test]
    fn long_code_wraps_literally_without_changing_indentation_or_spaces() {
        let code = format!("\t  x = {}  ", "abcdef".repeat(300));
        let text = format!("```text\n{code}\n```");
        for width in [2, 8, 19] {
            let rows = markdown_rows(&text, width);
            // The info label itself can span multiple separator rows at small widths.
            let opening_rows = wrapped("\u{2500} text ", width).len();
            let body = rows[opening_rows..rows.len() - 1]
                .iter()
                .map(|row| row.to_string()[2..].to_owned())
                .collect::<String>();
            assert_eq!(body, code.replace('\t', "    "));
            assert!(rows.iter().all(|row| row.width() <= width + 2));
        }
    }

    #[test]
    fn long_unicode_language_labels_are_not_clipped_or_overwide() {
        let language = "long-\u{754c}\u{1f469}\u{200d}\u{1f680}-language";
        for width in [2, 5, 12] {
            let rows = markdown_rows(&format!("```{language}"), width);
            assert!(rows.iter().all(|row| row.width() == width + 2));
            let label = rows
                .iter()
                .map(|row| {
                    let text = row.to_string();
                    text[2..].trim_end_matches('\u{2500}').to_owned()
                })
                .collect::<String>();
            assert_eq!(label, format!("\u{2500} {language} "));
        }
    }

    #[test]
    fn longer_fences_keep_inner_backticks_literal_and_do_not_synthesize_closure() {
        let rows = markdown_rows("````md\n```rust\n# literal\n```\n````\n# heading", 24);
        assert_eq!(rows[1].to_string(), "  ```rust");
        assert_eq!(rows[2].to_string(), "  # literal");
        assert_eq!(rows[3].to_string(), "  ```");
        assert_eq!(rows[5].to_string(), "  heading");
        assert_eq!(rows[5].style, strong().add_modifier(Modifier::UNDERLINED));
        let partial = markdown_rows("```rust\nlet value =", 24);
        assert_eq!(partial.len(), 2);
        assert_eq!(partial[1].to_string(), "  let value =");
        assert_eq!(strings(&markdown_rows("``", 24)), ["  ``"]);
        assert_eq!(strings(&markdown_rows("use ``a`b``", 24)), ["  use a`b"]);
        // A new chunk still starts in prose, with no cross-chunk parser state.
        assert_eq!(
            strings(&markdown_rows("# next chunk", 24)),
            ["  next chunk"]
        );
    }

    #[test]
    fn headings_bold_and_inline_code_keep_styles_after_wrapping() {
        let rows = markdown_rows(
            "# **hello world** and `code`\n### third\n####### literal",
            8,
        );
        assert_eq!(
            strings(&rows[..4]),
            ["  hello ", "  world ", "  and code", "  third"]
        );
        assert!(
            rows[..3]
                .iter()
                .all(|row| row.style == strong().add_modifier(Modifier::UNDERLINED))
        );
        assert_eq!(rows[0].spans.last().unwrap().style, strong());
        assert_eq!(rows[2].spans.last().unwrap().style, muted());
        assert_eq!(
            rows[3].style,
            Style::default().add_modifier(Modifier::UNDERLINED)
        );
        assert_eq!(strings(&rows[4..]).concat(), "  #######   literal");
        assert_eq!(
            strings(&markdown_rows("**partial and `partial", 40)),
            ["  **partial and `partial"]
        );
    }

    #[test]
    fn lists_and_quotes_have_hanging_continuations() {
        assert_eq!(
            strings(&markdown_rows("- one two three", 8)),
            ["  - one ", "    two ", "    three"]
        );
        assert_eq!(
            strings(&markdown_rows("12. one two three", 10)),
            ["  12. one ", "      two ", "      three"]
        );
        let rows = markdown_rows("> one two three", 8);
        assert_eq!(
            strings(&rows),
            ["  \u{2502} one ", "  \u{2502} two ", "  \u{2502} three"]
        );
        assert_eq!(rows[1].spans[1].style, muted());
        assert_eq!(
            strings(&markdown_rows("> - one two three", 10)),
            [
                "  \u{2502} - one ",
                "  \u{2502}   two ",
                "  \u{2502}   three"
            ]
        );
        for width in 1..=16 {
            assert!(
                markdown_rows("  123. one verylongword", width)
                    .iter()
                    .all(|row| row.width() <= width + 2)
            );
        }
    }

    #[test]
    fn shared_transcript_keeps_bold_user_rail_and_assistant_indent() {
        let user = transcript_rows(&entry("user", "accepted"), "hello world\n", false, 10);
        assert_eq!(strings(&user), ["\u{2503} hello ", "\u{2503} world"]);
        assert!(
            user.iter()
                .all(|row| row.style == strong() && row.spans[0].style == strong())
        );
        let assistant = entry("assistant", "completed");
        let text = "# title\n```rs\n  x\n```\n";
        let mut direct = markdown_rows(text, 18);
        direct.pop();
        assert_eq!(transcript_rows(&assistant, text, false, 20), direct);
        assert!(direct.iter().all(|row| row.to_string().starts_with("  ")));
        assert_eq!(
            strings(&transcript_rows(&assistant, "", false, 20)),
            Vec::<String>::new()
        );
        assert_eq!(
            strings(&transcript_rows(
                &entry("assistant", "running"),
                "",
                false,
                40
            )),
            ["  waiting for response..."]
        );
        assert_eq!(
            strings(&transcript_rows(
                &entry("assistant", "failed"),
                "body",
                true,
                20
            )),
            ["  body", "  failed", ""]
        );
    }

    #[test]
    fn sanitizer_and_notice_rendering_remain_literal() {
        assert_eq!(
            sanitized("a\r\nb\rc\u{1b}\u{7f}\t", 100),
            ("a\nb\nc\t".into(), false)
        );
        assert_eq!(clean("a\u{754c}b", 3), "a\n[display truncated]");
        let rows = transcript_rows(&entry("notice", "error"), "**literal**\u{1b}\n", false, 40);
        assert_eq!(strings(&rows), ["  **literal**"]);
        assert_eq!(rows[0].style, warning());
    }
}

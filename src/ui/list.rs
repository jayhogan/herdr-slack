//! The inbox screen: header, two-line rows, status line, key hints (spec §9).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::app::{Body, Header, RowView, Screen, Sort};
use crate::text::{truncate, width, wrap};

const HINTS: [&str; 10] = [
    "j/k move",
    "␣ expand",
    "o open",
    "O open+clear",
    "x clear",
    "u undo",
    "r refresh",
    "s sort",
    "g/G top/bottom",
    "q quit",
];
const EXPANDED_LINES: usize = 5;
/// The channel is cut to this many cells before the author starts to shrink.
const MIN_CHANNEL: usize = 4;

pub fn render(frame: &mut Frame, screen: &Screen, scroll: &mut usize) {
    let area = frame.area();
    let w = area.width as usize;
    let hints = hint_lines(w);

    let header_h = area.height.min(1);
    let rest = area.height - header_h;
    let bottom_h = ((1 + hints.len()) as u16).min(rest);
    let list_h = rest - bottom_h;

    let (lines, start, end) = body_lines(screen, w);
    let h = list_h as usize;
    if h > 0 {
        if start < *scroll {
            *scroll = start;
        } else if end >= *scroll + h {
            *scroll = (end + 1 - h).min(start);
        }
        *scroll = (*scroll).min(lines.len().saturating_sub(h));
    }
    let visible: Vec<Line> = lines.into_iter().skip(*scroll).take(h).collect();

    let rect = |y: u16, h: u16| Rect::new(area.x, area.y + y, area.width, h);
    frame.render_widget(
        Paragraph::new(header_line(&screen.header, w)),
        rect(0, header_h),
    );
    frame.render_widget(Paragraph::new(visible), rect(header_h, list_h));
    let mut bottom = vec![status_line(screen, w)];
    bottom.extend(hints);
    frame.render_widget(Paragraph::new(bottom), rect(header_h + list_h, bottom_h));
}

fn header_line(h: &Header, w: usize) -> Line<'static> {
    let sort = match h.sort {
        Sort::OldestWaiting => "oldest waiting",
        Sort::NewestActivity => "newest activity",
    };
    let middle = match &h.warning {
        Some(warn) => format!("⚠ {warn}"),
        None => h
            .by_kind
            .iter()
            .map(|(k, n)| format!("{n} {}", k.label()))
            .collect::<Vec<_>>()
            .join(" · "),
    };
    let build = |with_middle: bool, with_sort: bool, with_synced: bool| {
        let mut s = format!("Slack inbox │ {} need attention", h.count);
        if with_middle && !middle.is_empty() {
            s.push_str(" · ");
            s.push_str(&middle);
        }
        let mut tail = Vec::new();
        if with_sort {
            tail.push(format!("sort: {sort}"));
        }
        if let (true, Some(age)) = (with_synced, &h.synced) {
            tail.push(format!("synced {age}"));
        }
        if !tail.is_empty() {
            s.push_str("   ");
            s.push_str(&tail.join(" │ "));
        }
        s
    };
    // Narrow: drop the breakdown first, then the sort, then the sync age. A warning replaces the
    // breakdown and outlives the sort.
    let steps = if h.warning.is_some() {
        [
            (true, true, true),
            (true, false, true),
            (false, false, true),
            (false, false, false),
        ]
    } else {
        [
            (true, true, true),
            (false, true, true),
            (false, false, true),
            (false, false, false),
        ]
    };
    let text = steps
        .into_iter()
        .map(|(m, s, y)| build(m, s, y))
        .find(|t| width(t) <= w)
        .unwrap_or_else(|| truncate(&build(false, false, false), w));
    Line::styled(pad(&text, w), theme::bar())
}

fn status_line(screen: &Screen, w: usize) -> Line<'static> {
    let pos = match screen.body {
        Body::Rows => format!("{}/{}", screen.selected + 1, screen.rows.len()),
        _ => String::new(),
    };
    let age = screen
        .header
        .synced
        .as_ref()
        .map(|a| format!(" · data from {a}"))
        .unwrap_or_default();
    let (text, style) = if let Some(s) = &screen.status {
        (s.clone(), theme::bold())
    } else if let Some(e) = &screen.error {
        (format!("⚠ {e}{age}"), theme::warn())
    } else if screen.warnings > 0 {
        let n = screen.warnings;
        let noun = if n == 1 { "source" } else { "sources" };
        (format!("⚠ {n} {noun} failed{age}"), theme::warn())
    } else {
        (String::new(), Style::new())
    };
    let room = w.saturating_sub(width(&pos) + usize::from(!pos.is_empty()));
    let text = truncate(&text, room);
    let gap = w.saturating_sub(width(&text) + width(&pos));
    Line::from(vec![
        Span::styled(text, style),
        Span::raw(" ".repeat(gap)),
        Span::styled(pos, theme::dim()),
    ])
}

/// Hints packed greedily into lines of at most `w` cells.
fn hint_lines(w: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<String> = Vec::new();
    for hint in HINTS {
        match lines.last_mut() {
            Some(cur) if width(cur) + 2 + width(hint) <= w => {
                cur.push_str("  ");
                cur.push_str(hint);
            }
            _ => lines.push(hint.to_owned()),
        }
    }
    lines
        .into_iter()
        .map(|l| Line::styled(l, theme::dim()))
        .collect()
}

/// Body lines with the selected row's first and last line index.
fn body_lines(screen: &Screen, w: usize) -> (Vec<Line<'static>>, usize, usize) {
    match &screen.body {
        Body::Loading => (vec![Line::styled(" Loading Slack…", theme::dim())], 0, 0),
        Body::Empty => (
            vec![
                Line::raw(" Nothing needs you right now."),
                Line::styled(
                    " New activity on cleared threads will bring them back.",
                    theme::dim(),
                ),
            ],
            0,
            0,
        ),
        Body::AuthError(msg) => {
            let lines = wrap(msg, w.saturating_sub(2), 8)
                .into_iter()
                .map(|l| Line::styled(pad(&format!(" {l}"), w), theme::hot_row()))
                .collect();
            (lines, 0, 0)
        }
        Body::Rows => {
            let mut out = Vec::new();
            let (mut start, mut end) = (0, 0);
            for (i, row) in screen.rows.iter().enumerate() {
                if i > 0 {
                    out.push(Line::raw(""));
                }
                let selected = i == screen.selected;
                if selected {
                    start = out.len();
                }
                out.extend(row_lines(row, selected, w));
                if selected {
                    end = out.len() - 1;
                }
            }
            (out, start, end)
        }
    }
}

fn row_lines(r: &RowView, selected: bool, w: usize) -> Vec<Line<'static>> {
    let emph = |s: Style| if selected { s.patch(theme::bold()) } else { s };
    let age_w = width(&r.age);
    let text_w = w.saturating_sub(1 + 2 + age_w);
    let texts = if r.expanded {
        wrap(&r.full, text_w, EXPANDED_LINES)
    } else {
        vec![truncate(&r.text, text_w)]
    };

    let mut lines = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let mut spans = vec![if i == 0 && selected {
            Span::styled("▌", theme::bold())
        } else {
            Span::raw(" ")
        }];
        if i == 0 {
            spans.push(Span::styled(pad(t, text_w), emph(Style::new())));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(r.age.clone(), emph(theme::dim())));
        } else {
            spans.push(Span::styled(t.clone(), emph(Style::new())));
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::raw(" "));
    }
    lines.push(second_line(r, &emph, w));
    lines
}

/// Author, channel, then every kind whole; the channel shrinks first, then the author.
fn second_line(r: &RowView, emph: &dyn Fn(Style) -> Style, w: usize) -> Line<'static> {
    let kinds: Vec<String> = r.kinds.iter().map(|k| format!("[{}]", k.label())).collect();
    let kinds_w = kinds.iter().map(|k| width(k)).sum::<usize>() + kinds.len().saturating_sub(1);
    // Room for author + channel, after the marker column and the two 2-cell gaps.
    let room = w.saturating_sub(1 + kinds_w + 4);
    let (au_w, ch_w) = (width(&r.author), width(&r.channel));
    let (author, channel) = if au_w + ch_w <= room {
        (r.author.clone(), r.channel.clone())
    } else {
        let ch = ch_w.min(room.saturating_sub(au_w).max(MIN_CHANNEL));
        (
            truncate(&r.author, room.saturating_sub(ch)),
            truncate(&r.channel, ch.min(room)),
        )
    };

    let mut spans = vec![Span::raw(" ")];
    for (text, style) in [(author, Style::new()), (channel, theme::dim())] {
        if !text.is_empty() {
            spans.push(Span::styled(text, emph(style)));
            spans.push(Span::raw("  "));
        }
    }
    for (i, (label, k)) in kinds.into_iter().zip(&r.kinds).enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(label, emph(theme::kind(*k))));
    }
    Line::from(spans)
}

fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

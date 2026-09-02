//! Drawing (DESIGN §7).
//!
//! The shape of a screen: a badge bar that reads as a title, a band of facts
//! about the file, the form itself, a shortcut strip for the current mode, and
//! one line of status. Every field paints its editable region, so the form
//! looks like a form before you focus anything.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::{protocol::StatefulProtocol, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::model::schema::Control;
use crate::model::value::{Agg, Value};
use crate::tags::plan::FilePlan;
use crate::ui::app::{App, Mode, Row, WriteProgress, WriteResults};
use crate::ui::edit::{stars_glyphs, Opt, Validation};
use crate::ui::theme as t;

const LABEL_COLS: u16 = 15;
const GUTTER: u16 = 1;
/// Blank columns of field background either side of a value.
const PAD: u16 = 1;

/// Cells are about twice as tall as they are wide, so an image of pixel aspect
/// `a` needs `2 * rows * a` columns to keep its proportions. Sizing the band
/// this way is what lets a portrait clip render as a portrait picture instead
/// of a three-column sliver.
fn thumb_cols(rows: u16, aspect: f32) -> u16 {
    ((2.0 * rows as f32 * aspect).round() as u16).clamp(4, 40)
}

/// A portrait picture earns a taller band; a landscape one does not need it.
fn header_rows(area_h: u16, aspect: Option<f32>) -> u16 {
    if area_h < 20 {
        return 0;
    }
    match aspect {
        Some(a) if a < 0.95 => 6.max((area_h / 3).min(14)),
        _ => 6,
    }
}

pub fn draw(f: &mut Frame, app: &App, proto: Option<&mut StatefulProtocol>) {
    let area = f.area();
    let header_h = header_rows(area.height, app.thumb_aspect);
    let chunks = Layout::vertical([
        Constraint::Length(1),        // badge bar
        Constraint::Length(1),        // breathing room under it
        Constraint::Length(header_h), // thumbnail + file facts
        Constraint::Min(3),           // the form
        Constraint::Length(1),        // shortcuts for this mode
        Constraint::Length(1),        // status / validation
    ])
    .split(area);

    draw_badge_bar(f, chunks[0], app);

    // A dialog takes everything below the header: it is the whole message.
    if app.pending.is_some() || app.results.is_some() || app.progress.is_some() {
        let top = chunks[2].y;
        let body = Rect {
            x: area.x,
            y: top,
            width: area.width,
            height: area.height.saturating_sub(top.saturating_sub(area.y)),
        };
        if let Some(p) = &app.progress {
            draw_progress(f, body, p);
        } else if let Some(plans) = &app.pending {
            draw_confirm(f, body, app, plans);
        } else if let Some(r) = &app.results {
            draw_results(f, body, r);
        }
        return;
    }

    if header_h > 0 {
        if app.inspector {
            draw_inspector(f, chunks[2], app);
        } else {
            draw_header(f, chunks[2], app, proto);
        }
    }
    draw_fields(f, chunks[3], app);
    draw_shortcuts(f, chunks[4], app);
    draw_status(f, chunks[5], app);
}

/// The name sits in a filled badge and the bar carries its own background the
/// full width, so the header reads as a title rather than as one more row of
/// text competing with the form.
fn draw_badge_bar(f: &mut Frame, area: Rect, app: &App) {
    let view = match app.view {
        Some(i) => format!("file {}/{}", i + 1, app.files.len()),
        None => format!("{} file{}", app.files.len(), plural(app.files.len())),
    };
    let mut left = format!("  {view}");
    if app.n_custom > 0 {
        left.push_str(&format!(" · {} custom", app.n_custom));
    }
    let mut right = String::new();
    if !app.staged.is_empty() {
        right.push_str(&format!("{} staged · ", app.staged_count()));
    }
    // The mode lives in the shortcut strip now, next to the keys it governs;
    // saying it twice, in two vocabularies, was worse than saying it once.
    right.push_str(&format!(
        "faststart {}",
        if app.faststart { "on" } else { "off" }
    ));
    let tail = "  ".to_string();

    let badge = " tagform ";
    let used = badge.width() + left.width() + right.width() + tail.width();
    let gap = (area.width as usize).saturating_sub(used);
    let bar = Style::default().bg(t::header_bg());

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                badge,
                Style::default().bg(t::badge_bg()).fg(t::badge_fg()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(left, bar.fg(t::header_fg())),
            Span::styled(" ".repeat(gap), bar),
            Span::styled(
                right,
                bar.fg(if app.staged.is_empty() { t::muted() } else { t::staged() }),
            ),
            Span::styled(tail, bar),
        ]))
        .style(bar),
        area,
    );
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, proto: Option<&mut StatefulProtocol>) {
    let idx = app.current_file();
    let Some(file) = app.files.get(idx) else { return };

    let want = app.thumb_aspect.map(|a| thumb_cols(area.height, a)).unwrap_or(0);
    let has_thumb = proto.is_some() && want > 0 && area.width > want + 20;
    let cols = Layout::horizontal([
        Constraint::Length(if has_thumb { want } else { 0 }),
        Constraint::Min(10),
    ])
    .split(area);

    if has_thumb {
        if let Some(p) = proto {
            f.render_stateful_widget(StatefulImage::default(), cols[0], p);
        }
    }

    let name = file_label(&file.path);
    let dir = file
        .path
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    let summary = app.media.get(idx).map(|m| m.summary()).unwrap_or_default();
    let pad = if has_thumb { "  " } else { " " };

    let lines = vec![
        Line::from(Span::styled(
            format!("{pad}{name}"),
            Style::default().fg(t::header_fg()).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{pad}{}", if summary.is_empty() { "probing…".into() } else { summary }),
            Style::default().fg(t::muted()),
        )),
        Line::from(Span::styled(format!("{pad}{dir}"), Style::default().fg(t::path()))),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), cols[1]);
}

/// The answer to "what does ‹multiple› actually contain" -- the thing the old
/// fzf-based tagger could only show in a preview pane.
fn draw_inspector(f: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.rows.get(app.focus) else { return };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {} ", row.label), Style::default().bg(t::rule()).fg(t::label_focus())),
        Span::styled("  per file", Style::default().fg(t::muted())),
    ])];

    // The effective values, so a per-file edit is visible here as the thing
    // that will be written to that file.
    let scope = app.scope();
    match &row.eff {
        Agg::Mixed { values } => {
            for (i, v) in values.iter().enumerate() {
                let file = scope.get(i).copied().unwrap_or(i);
                let shown = match v {
                    Some(Value::Text(s)) => s.clone(),
                    Some(Value::List(l)) => l.join(" · "),
                    None => "—".into(),
                };
                let style = if app.file_is_staged(file, &row.key) {
                    Style::default().fg(t::staged())
                } else if v.is_some() {
                    Style::default().fg(t::value())
                } else {
                    Style::default().fg(t::value_empty())
                };
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(t::fit(&shown, 34), style),
                    Span::styled(
                        app.files.get(file).map(|f| file_label(&f.path)).unwrap_or_default(),
                        Style::default().fg(t::muted()),
                    ),
                ]));
            }
        }
        Agg::Same { .. } => lines.push(Line::from(Span::styled(
            " identical in every file",
            Style::default().fg(t::muted()),
        ))),
        Agg::Absent => lines.push(Line::from(Span::styled(
            " present in no file",
            Style::default().fg(t::value_empty()),
        ))),
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_fields(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(t::rule()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width <= LABEL_COLS + GUTTER + 2 * PAD + 4 {
        return;
    }

    let height = inner.height as usize;
    // The box is the full width; the text sits inside it with a blank column of
    // its own background either side, so it reads as an input rather than as a
    // block of colour butted straight up against the label.
    let value_w = inner.width.saturating_sub(1 + LABEL_COLS + GUTTER + 1) as usize;
    let text_w = value_w.saturating_sub(2 * PAD as usize);
    let value_x = inner.x + 1 + LABEL_COLS + GUTTER;

    // Every row is laid out, then a window of it is drawn: a group rule is a
    // line with no row of its own, so the focused row's position is no longer
    // its index and scrolling has to count lines rather than fields.
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_line: Option<(u16, usize)> = None;
    let mut focus_line = 0usize;
    for (i, row) in app.rows.iter().enumerate() {
        let focused = i == app.focus;
        if focused {
            focus_line = lines.len();
        }
        let editing = focused && app.mode == Mode::Edit;
        let staged = row.staged;
        let custom = row.def.is_none();
        let readonly = !row.editable();

        // The marker column says where you are, and nothing else. It used to
        // carry the staged dot as well, which put an edit indicator in the
        // caret's column -- so a staged row looked mis-caretted, and a row that
        // was both staged and focused lost its indicator entirely because the
        // caret won. Edited-ness is carried by the label colour instead.
        let (marker, marker_fg) = if editing {
            ("▶", t::accent())
        } else if focused {
            ("▍", t::accent())
        } else {
            (" ", t::rule())
        };

        // Staged outranks focus here precisely so it survives being focused.
        let label_fg = match (staged, focused, custom) {
            (true, _, _) => t::staged(),
            (false, true, _) => t::label_focus(),
            (false, false, true) => t::label_custom(),
            (false, false, false) => t::label(),
        };
        let label_style = if focused {
            Style::default().fg(label_fg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(label_fg)
        };

        // Every control paints its editable region, so the form reads as a form
        // rather than as a list of colons.
        let bg = if readonly {
            t::input_bg_readonly()
        } else if editing {
            t::input_bg_edit()
        } else if focused {
            t::input_bg_focus()
        } else {
            t::input_bg()
        };

        let (raw, fg) = if editing {
            let (text, cur) = app
                .editor
                .as_ref()
                .map(|e| e.display())
                .unwrap_or_else(|| (String::new(), None));
            if let Some(c) = cur {
                let x = value_x + PAD + (c as u16).min(text_w.saturating_sub(1) as u16);
                cursor_line = Some((x, lines.len()));
            }
            let fg = match app.validation() {
                Validation::Error(_) => t::error(),
                Validation::Warn(_) => t::warn(),
                Validation::Ok => t::value(),
            };
            (text, fg)
        } else {
            match display_row(app, row).filter(|v| !v.is_empty()) {
                Some(v) if staged => (v, t::staged()),
                Some(v) if row.is_mixed() => (v, t::mixed()),
                Some(v) if readonly => (v, t::muted()),
                Some(v) => (v, t::value()),
                // A field cleared by ⌫ draws the same em dash as one that was
                // never set -- an empty row must not read as a drawing gap --
                // but keeps the staged colour, since it is still an edit.
                None if staged => ("—".into(), t::staged()),
                None => ("—".into(), t::value_empty()),
            }
        };
        // Star colour belongs to stars. An empty rating draws the same "—" as
        // every other empty field and must look like one.
        let has_value = app.shown_value(row).is_some();
        let value_fg = if row.control == Control::Stars && !editing && has_value {
            t::star()
        } else {
            fg
        };

        // A fixed set is always drawn as its set, laid along the value box
        // with the current answer lit. There is no open state to draw
        // differently: h/l step it in place, so the options are the control
        // and the row never reflows.
        //
        // A set brings its own left pad -- every cell is ` label ` -- so it
        // skips the box's, and takes that column back as width. Painting both
        // put a set's first option one column right of every other row's
        // value, which is exactly the misalignment the pad exists to prevent.
        let set = closed_set(app, row);
        let lead = if set.is_some() { 0 } else { PAD as usize };
        let value_spans = match set {
            Some((labels, sel)) => {
                set_spans(&labels, sel, text_w + PAD as usize - lead, bg, focused)
            }
            None => vec![Span::styled(
                t::fit(&raw, text_w),
                Style::default().bg(bg).fg(value_fg),
            )],
        };

        let mut spans = vec![
            Span::styled(marker, Style::default().fg(marker_fg)),
            Span::styled(
                t::fit(
                    &if custom { t::short_key(&row.label) } else { row.label.clone() },
                    LABEL_COLS as usize,
                ),
                label_style,
            ),
            Span::raw(" "),
            Span::styled(" ".repeat(lead), Style::default().bg(bg)),
        ];
        spans.extend(value_spans);
        spans.push(Span::styled(" ".repeat(PAD as usize), Style::default().bg(bg)));
        lines.push(Line::from(spans));
        if group_break_after(row) {
            lines.push(Line::from(Span::styled(
                "\u{2500}".repeat(inner.width as usize),
                Style::default().fg(t::rule()),
            )));
        }
    }

    let start = if focus_line >= height { focus_line + 1 - height } else { 0 };
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if let Some((x, line)) = cursor_line {
        if let Some(y) = line.checked_sub(start).filter(|y| *y < height) {
            f.set_cursor_position((x, inner.y + y as u16));
        }
    }
}

/// Category is drawn alone above a rule. It is not one field among the others:
/// it says what the file *is*, and which of the fields below it are worth
/// showing at all (DESIGN §3.5, §16). A form where that choice sits eighth,
/// indistinguishable from Tags, hides the one answer everything else follows.
fn group_break_after(row: &Row) -> bool {
    row.def.is_some_and(|d| d.id == "category")
}

/// The set to draw for a fixed-set row: its options, and which one the row
/// holds. `None` for anything that is not a set, and for a set with no options
/// (no `--alias` configured), so the row falls back to the ordinary value box
/// rather than to a blank strip. A mixed row selects nothing -- there is no
/// single answer to light.
///
/// A value the set does not know is appended rather than dropped, the same way
/// `nudge` appends it: an unfamiliar Category has to be visible and steppable,
/// or the first h would silently replace it.
fn closed_set(app: &App, row: &Row) -> Option<(Vec<String>, Option<usize>)> {
    if row.control != Control::Enum {
        return None;
    }
    let mut opts = app.options_for(row);
    if opts.is_empty() {
        return None;
    }
    let sel = match app.shown_value(row) {
        Some(Value::Text(s))
            if !s.trim().is_empty() && (!row.is_mixed() || row.staged) =>
        {
            match opts.iter().position(|o| o.code == s) {
                Some(i) => Some(i),
                None => {
                    opts.push(Opt { code: s.clone(), label: s });
                    Some(opts.len() - 1)
                }
            }
        }
        _ => None,
    };
    Some((opts.into_iter().map(|o| o.label).collect(), sel))
}

/// The set, laid out along the value box with the current one lit.
///
/// Scrolls to keep the selection in view rather than eliding the tail: with
/// seven kinds and "Podcast" chosen, a naive trim showed every option except
/// the one you were on.
/// `lit` is whether the row is the one you are on: an open or focused set lights
/// its selection in accent, a set sitting quietly further down the form marks
/// it without competing with the caret for attention.
fn set_spans(
    labels: &[String],
    sel: Option<usize>,
    width: usize,
    bg: ratatui::style::Color,
    lit: bool,
) -> Vec<Span<'static>> {
    let cell = |i: usize| format!(" {} ", labels[i]);
    let mut first = 0usize;
    loop {
        let used: usize = (first..labels.len()).map(|i| cell(i).width()).sum();
        if used <= width || first >= sel.unwrap_or(0) {
            break;
        }
        first += 1;
    }

    let mut spans = Vec::new();
    let mut used = 0usize;
    for i in first..labels.len() {
        let text = cell(i);
        if used + text.width() > width {
            break;
        }
        used += text.width();
        spans.push(Span::styled(
            text,
            match (Some(i) == sel, lit) {
                (true, true) => {
                    Style::default().bg(t::accent()).fg(t::badge_fg()).add_modifier(Modifier::BOLD)
                }
                (true, false) => {
                    Style::default().bg(t::rule()).fg(t::value()).add_modifier(Modifier::BOLD)
                }
                (false, _) => Style::default().bg(bg).fg(t::muted()),
            },
        ));
    }
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), Style::default().bg(bg)));
    }
    spans
}

/// An unfocused row shows the staged edit if there is one, else what is on disk.
fn display_row(app: &App, row: &Row) -> Option<String> {
    let v = app.shown_value(row)?;
    Some(match (&v, row.control) {
        (_, Control::Stars) => stars_glyphs(
            match &v {
                Value::Text(s) => s.trim().parse::<u8>().unwrap_or(0),
                _ => 0,
            }
            .min(5),
        ),
        (Value::List(l), Control::HashTags) => {
            l.iter().map(|x| format!("#{x}")).collect::<Vec<_>>().join(" ")
        }
        (Value::List(l), _) => l.join(" · "),
        (Value::Text(s), Control::Enum) => app.enum_label(row, s).unwrap_or_else(|| s.clone()),
        (Value::Text(s), _) => s.replace('\n', " "),
    })
}

/// The keys that matter right now, and only those. Which keys are live depends
/// on the mode, so a fixed strip would be wrong half the time.
///
/// The strip opens with a vim-style mode indicator, and the bar is always
/// painted -- dark in Select, lit while a field is open or the case menu is
/// armed. The keymap alone changing under you is easy to miss, and typing into
/// a field you thought was closed is the mistake worth pricing a colour
/// against; a permanent ground is what makes the lit states read as a change
/// rather than as the bar simply appearing.
fn draw_shortcuts(f: &mut Frame, area: Rect, app: &App) {
    // There is no third mode any more: a fixed set is stepped from Normal
    // with h/l and never opens, so the strip has only the two states the app
    // actually has.
    let (mode_name, mode_fg, bar_bg) = if app.mode == Mode::Edit {
        ("EDIT", t::staged(), Some(t::input_bg_edit()))
    } else {
        ("NORMAL", t::accent(), Some(t::bar_bg()))
    };
    // The case menu is modal for exactly one keystroke, and the strip is the
    // only place that says so -- so it replaces the strip outright rather than
    // appending to it.
    let (mode_name, mode_fg, bar_bg) = if app.format_pending {
        ("FORMAT", t::star(), Some(t::input_bg_focus()))
    } else {
        (mode_name, mode_fg, bar_bg)
    };
    let pairs: &[(&str, &str)] = if app.format_pending {
        &[
            ("c", "capitalize"),
            ("t", "title case"),
            ("l", "lower case"),
            ("u", "upper case"),
            ("esc", "cancel"),
        ]
    } else if app.mode == Mode::Edit {
        &[
            ("⏎", "save"),
            ("⇥", "save + next"),
            ("←→", "rating"),
            ("esc", "cancel"),
            ("^c", "quit"),
        ]
    } else {
        &[
            // h and l move along a set rather than between rows, but they are
            // the same hand's movement keys and a strip that named only two of
            // the four read as though the other two did nothing.
            ("hjkl", "move"),
            ("⏎", "edit"),
            ("w", "write"),
            ("r", "rename"),
            ("m", "merge"),
            ("i", "inspect"),
            ("][", "file"),
            ("a", "all files"),
            ("o", "overwrite"),
            ("b", "backfill"),
            ("u", "undo"),
            // The glyph is one column wide by the width tables and wider than
            // that in most terminals, so it carries its own trailing space
            // rather than letting the next label collide with it.
            ("⌫ ", "clear"),
            ("f", "format"),
            ("y", "yank"),
            ("p", "paste"),
            ("t", "theme"),
            ("F", "fast"),
            ("q", "quit"),
        ]
    };
    // Drop hints that do not fit rather than letting the strip run off the
    // edge: a half-rendered key name is worse than one fewer hint.
    let badge = format!(" {mode_name} ");
    let mut used = badge.width() + 1;
    let mut spans = vec![
        Span::styled(
            badge,
            Style::default().bg(mode_fg).fg(t::badge_fg()).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    let mut dropped = 0usize;
    for (k, d) in pairs {
        let key = format!(" {k} ");
        let desc = format!(" {d}  ");
        let w = key.width() + desc.width();
        if used + w + 2 > area.width as usize {
            dropped += 1;
            continue;
        }
        used += w;
        spans.push(Span::styled(
            key,
            Style::default().bg(t::rule()).fg(t::accent()).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(desc, Style::default().fg(t::muted())));
    }
    if dropped > 0 {
        spans.push(Span::styled("…", Style::default().fg(t::rule())));
    }
    let strip = Paragraph::new(Line::from(spans));
    f.render_widget(
        match bar_bg {
            Some(bg) => strip.style(Style::default().bg(bg)),
            None => strip,
        },
        area,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    // Validation is about the field under the cursor, so it outranks the
    // transient status line -- but only while a field is actually open.
    let live = if app.mode == Mode::Edit { app.validation() } else { Validation::Ok };
    let (text, fg) = match live {
        Validation::Error(m) => (m, t::error()),
        Validation::Warn(m) => (m, t::warn()),
        Validation::Ok if !app.status.is_empty() => (app.status.clone(), t::muted()),
        Validation::Ok => (String::new(), t::muted()),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {text}"), Style::default().fg(fg)))),
        area,
    );
}

/// The plan, in the terms the user thinks in: which field, to what, and by
/// which route. The route matters because it is the difference between an
/// in-place update and a full rewrite of a multi-gigabyte file.
fn draw_confirm(f: &mut Frame, area: Rect, app: &App, plans: &[FilePlan]) {
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" Write {} file{} ", plans.len(), plural(plans.len())),
            Style::default().bg(t::accent()).fg(t::badge_fg()).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for edit in app.staged_summary() {
        let mut spans = vec![
            Span::styled(format!("  {}", t::fit(&edit.label, 14)), Style::default().fg(t::label())),
            Span::styled("→ ", Style::default().fg(t::muted())),
            Span::styled(edit.shown, Style::default().fg(t::staged())),
        ];
        // Which files, because an edit no longer belongs to whatever happens to
        // be in view: it belongs to the files it was made on.
        if edit.files < app.files.len() {
            spans.push(Span::styled(
                format!("   on {} file{}", edit.files, plural(edit.files)),
                Style::default().fg(t::muted()),
            ));
        }
        // Replacing one value is an edit; replacing several distinct ones is a
        // different act, and this is the last place to notice it.
        if edit.overwrites > 1 {
            spans.push(Span::styled(
                format!("   replaces {} distinct values", edit.overwrites),
                Style::default().fg(t::warn()),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));

    for p in plans {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", t::fit(&file_label(&p.path), 28)), Style::default().fg(t::header_fg())),
            Span::styled(t::fit(p.writer.label(), 22), Style::default().fg(t::accent())),
            Span::styled(p.why, Style::default().fg(t::muted())),
        ]));
        lines.push(Line::from(Span::styled(
            format!("  {}{:?}", " ".repeat(28), p.layout),
            Style::default().fg(t::rule()),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  faststart {} · originals replaced only after the result is verified",
            if app.faststart { "on" } else { "off" }
        ),
        Style::default().fg(t::muted()),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ⏎ ", Style::default().bg(t::rule()).fg(t::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" write   ", Style::default().fg(t::value())),
        Span::styled(" esc ", Style::default().bg(t::rule()).fg(t::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", Style::default().fg(t::value())),
    ]));

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default().borders(Borders::ALL).border_style(Style::default().fg(t::accent())),
        ),
        area,
    );
}

/// The bar, drawn by hand rather than with `Gauge` so the filled and unfilled
/// halves take their colours from the theme like everything else.
fn bar(width: usize, frac: f64) -> Line<'static> {
    let filled = ((width as f64) * frac.clamp(0.0, 1.0)).round() as usize;
    Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(t::accent())),
        Span::styled("░".repeat(width.saturating_sub(filled)), Style::default().fg(t::rule())),
    ])
}

/// A write in flight. The stage name matters as much as the bar: "remuxing" for
/// two minutes is patience, the same two minutes unlabelled is a hang.
fn draw_progress(f: &mut Frame, area: Rect, p: &WriteProgress) {
    let overall = p.overall();
    let width = (area.width as usize).saturating_sub(6).clamp(10, 60);
    let secs = p.started.elapsed().as_secs();

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" Writing {} of {} ", p.file + 1, p.total),
            Style::default().bg(t::accent()).fg(t::badge_fg()).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", t::fit(&p.name, width)),
            Style::default().fg(t::header_fg()),
        )),
        Line::from(""),
    ];

    let mut b = vec![Span::raw("  ")];
    b.extend(bar(width, overall).spans);
    b.push(Span::styled(
        format!(" {:>3}%", (overall * 100.0).round() as u32),
        Style::default().fg(t::value()),
    ));
    lines.push(Line::from(b));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", p.label), Style::default().fg(t::value())),
        Span::styled(format!("   {}m {:02}s elapsed", secs / 60, secs % 60), Style::default().fg(t::muted())),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Originals are replaced only after the result is verified; nothing is half-written.",
        Style::default().fg(t::muted()),
    )));

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default().borders(Borders::ALL).border_style(Style::default().fg(t::accent())),
        ),
        area,
    );
}

/// What actually happened, per file. A one-line status is fine for one file and
/// useless for forty: a batch needs to say which ones failed and why, without
/// the successes scrolling them away.
fn draw_results(f: &mut Frame, area: Rect, r: &WriteResults) {
    let total = r.ok.len() + r.failed.len();
    let ok = r.failed.is_empty();
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" Wrote {} of {} ", r.ok.len(), total),
            Style::default()
                .bg(if ok { t::staged() } else { t::error() })
                .fg(t::badge_fg())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for p in &r.ok {
        lines.push(Line::from(vec![
            Span::styled("  ✓ ", Style::default().fg(t::staged())),
            Span::styled(file_label(p), Style::default().fg(t::value())),
        ]));
    }
    // A failure gets the file on its own line and the reason indented under
    // it, wrapped to the box. Held on one line, a stream mismatch ran off the
    // right edge of the terminal and the part that said what went wrong was
    // the part that fell off.
    let text_width = (area.width as usize).saturating_sub(10).max(20);
    for (p, err) in &r.failed {
        lines.push(Line::from(vec![
            Span::styled("  ✕ ", Style::default().fg(t::error())),
            Span::styled(
                file_label(p),
                Style::default().fg(t::error()).add_modifier(Modifier::BOLD),
            ),
        ]));
        for l in wrap(err, text_width) {
            lines.push(Line::from(Span::styled(
                format!("      {l}"),
                Style::default().fg(t::muted()),
            )));
        }
        lines.push(Line::from(""));
    }
    if !ok {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Files that failed are unchanged; nothing was half-written.",
            Style::default().fg(t::muted()),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  any key to continue",
        Style::default().fg(t::value()).add_modifier(Modifier::BOLD),
    )));

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if ok { t::staged() } else { t::error() })),
        ),
        area,
    );
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Break `text` to `width` columns, keeping the newlines it already has: an
/// error that laid itself out in lines (`verify_streams` does) keeps that
/// layout, and a line that already fits is passed through untouched so its
/// columns stay lined up. Only an over-long line is folded, and its
/// continuations keep the original's indent. A word longer than the width is
/// left to overflow rather than cut -- a truncated path helps nobody.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.lines() {
        if para.chars().count() <= width {
            out.push(para.to_string());
            continue;
        }
        let indent: String = para.chars().take_while(|c| *c == ' ').collect();
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() { indent.chars().count() + word.chars().count() }
            else { line.chars().count() + 1 + word.chars().count() };
            if line.is_empty() {
                line = format!("{indent}{word}");
            } else if candidate <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line = format!("{indent}{word}");
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

fn file_label(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_gets_a_taller_band_than_landscape() {
        assert!(header_rows(40, Some(0.5625)) > header_rows(40, Some(1.78)));
    }

    #[test]
    fn short_terminals_drop_the_band_entirely() {
        assert_eq!(header_rows(18, Some(0.56)), 0);
    }

    /// A 16:9 picture is wide; a 9:16 one is narrow. The point of the fix.
    #[test]
    fn columns_follow_the_aspect() {
        assert!(thumb_cols(6, 1.78) > thumb_cols(6, 0.5625));
    }

    #[test]
    fn columns_stay_within_bounds() {
        assert!(thumb_cols(6, 0.01) >= 4);
        assert!(thumb_cols(60, 10.0) <= 40);
    }

    /// The rule is a line with no row behind it, so the whole window has to be
    /// counted in lines: this is the test that fails if the group break is
    /// added back into the field loop without adjusting the scroll.
    #[test]
    fn category_is_drawn_first_and_the_rule_follows_it() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        let f = FileTags {
            path: std::path::PathBuf::from("/tmp/tagform-render-test.mp4"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let app = crate::ui::app::App::new(vec![f], BTreeMap::new(), false);
        assert_eq!(app.rows[0].key, "category");

        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|fr| draw_fields(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let line = |y: u16| -> String {
            (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect()
        };
        // y=0 is the block's top border; the form starts at y=1.
        assert!(line(1).contains("Category"), "{:?}", line(1));
        assert!(line(2).trim_end().chars().all(|c| c == '\u{2500}'), "{:?}", line(2));
        assert!(line(3).contains("Title"), "{:?}", line(3));
    }

    /// Category's set is painted on the closed row, and the value it holds is
    /// the one lit -- the whole point of giving it the row.
    #[test]
    fn a_closed_category_row_draws_its_set_and_lights_the_choice() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        let f = FileTags {
            path: std::path::PathBuf::from("/tmp/tagform-render-test.mp4"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let mut app = crate::ui::app::App::new(vec![f], BTreeMap::new(), false);
        let chosen = app.enums.category[1].clone();
        app.set_staged(0, "category", Value::Text(chosen.clone()));

        // Wide enough for the whole set: `set_spans` scrolls to keep the
        // selection in view, so a narrow terminal drops the head of the set
        // and the first option is no longer the one to look for.
        let w = 140;
        let mut term = Terminal::new(TestBackend::new(w, 12)).unwrap();
        term.draw(|fr| draw_fields(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let row: String = (0..w).map(|x| buf[(x, 1)].symbol().to_string()).collect();

        assert!(row.contains(&app.enums.category[0]), "{row:?}");
        assert!(row.contains(&chosen), "{row:?}");
        // The chosen cell is bold; the ones either side of it are not.
        let at = row.find(&chosen).unwrap() as u16;
        assert!(buf[(at, 1)].style().add_modifier.contains(Modifier::BOLD), "{row:?}");
        assert!(!buf[(2 + LABEL_COLS, 1)].style().add_modifier.contains(Modifier::BOLD));
    }

    /// The strip is one line and its keys are a fixed vocabulary, so a hint
    /// that renders into its neighbour is a permanent smudge. The clear key is
    /// the one at risk: ⌫ is one column by the width tables and wider than
    /// that in most terminals.
    #[test]
    fn every_shortcut_hint_keeps_a_gap_after_its_key() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        let f = FileTags {
            path: std::path::PathBuf::from("/tmp/tagform-strip-test.mp4"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let app = crate::ui::app::App::new(vec![f], BTreeMap::new(), false);
        let w = 240;
        let mut term = Terminal::new(TestBackend::new(w, 1)).unwrap();
        term.draw(|fr| draw_shortcuts(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let strip: String = (0..w).map(|x| buf[(x, 0)].symbol().to_string()).collect();

        // Two spaces of padding plus the one the key carries, so the gap
        // still reads as one column once the terminal draws the glyph wide.
        assert!(strip.contains(" ⌫   clear"), "{strip:?}");
        assert!(!strip.contains("…"), "the whole strip should fit at {w} cols: {strip:?}");
        for key in ["o", "b", "f", "F", "t"] {
            assert!(strip.contains(&format!(" {key}  ")), "{key} crowded: {strip:?}");
        }
    }

    #[test]
    fn wrap_keeps_short_lines_and_their_columns() {
        let msg = "the remux did not reproduce this file's tracks\nlost:    data/mebx x3\ngained:  data/stts x3";
        assert_eq!(
            wrap(msg, 60),
            vec![
                "the remux did not reproduce this file's tracks",
                "lost:    data/mebx x3",
                "gained:  data/stts x3",
            ]
        );
    }

    #[test]
    fn wrap_folds_a_long_line_under_its_own_indent() {
        let out = wrap("  aaa bbb ccc ddd eee", 11);
        assert_eq!(out, vec!["  aaa bbb", "  ccc ddd", "  eee"]);
    }

    #[test]
    fn a_failure_puts_the_reason_under_the_file_name() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let r = WriteResults {
            ok: vec![],
            failed: vec![(
                std::path::PathBuf::from("/x/IMG_4855.MOV"),
                "the remux did not reproduce this file's tracks\nlost:    data/mebx x3".into(),
            )],
        };
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|fr| draw_results(fr, fr.area(), &r)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| -> String { (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let rows: Vec<String> = (0..12).map(row).collect();
        let at = rows.iter().position(|l| l.contains("IMG_4855.MOV")).unwrap();
        // The name owns its line; the reason follows, indented, unwrapped.
        assert!(!rows[at].contains("remux"), "{:?}", rows[at]);
        assert!(rows[at + 1].contains("the remux did not reproduce"), "{:?}", rows[at + 1]);
        assert!(rows[at + 2].contains("lost:    data/mebx x3"), "{:?}", rows[at + 2]);
    }


}

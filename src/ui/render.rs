//! Drawing (DESIGN §7).
//!
//! The shape of a screen: a badge bar that reads as a title and carries the
//! keys the current mode takes, a band of facts about the file under a line
//! saying which file it is, the form itself, a mode bar, and one line of
//! status. Every field paints its editable region, so the form looks like a
//! form before you focus anything.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::{protocol::StatefulProtocol, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::model::schema::Control;
use crate::model::tag;
use crate::model::value::{Agg, Value};
use crate::tags::atoms::Layout as Container;
use crate::tags::plan::FilePlan;
use crate::ui::app::{
    App, FileEdit, ImportSource, Locate, Mode, QueuePlace, QueueRow, Row, WriteResults,
};
use crate::ui::edit::{stars_glyphs, Opt, Validation};
use crate::ui::keymap::{key_width, KEYMAP};
use crate::ui::theme as t;

const LABEL_COLS: u16 = 15;
const GUTTER: u16 = 1;
/// Blank columns of field background either side of a value.
const PAD: u16 = 1;
/// SF Symbols, like the rest of the glyph vocabulary here. `􁒖` marks the bulk
/// heading, `􀈏` a file waiting in the write queue.
const BULK_ICON: &str = "\u{101496}";
const QUEUE_ICON: &str = "\u{10020f}";
/// `􀽎` fronts each name in the bulk header's file list.
const FILE_ICON: &str = "\u{100f4e}";
/// `􀆿` sits left of the name in the badge.
const LOGO_ICON: &str = "\u{1001bf}";
/// `􀇿` fronts an alert on a file's page: a rename that would land on
/// another file, or a staged value the write will leave out.
const ALERT_ICON: &str = "\u{1001ff}";

/// An SF Symbol and the text it fronts. Every symbol here is followed by the
/// same gap, and the gap is two spaces, not one: the symbol draws wider than
/// the one cell it is counted as, so a single space is swallowed by the
/// overhang and the glyph touches the word.
fn iconed(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}
/// Names the bulk header lists before it gives up and states the count.
const LISTED_FILES: usize = 5;
/// Files the write-queue panel lists before it gives up and states the
/// count. One more than the selection list: a queue is worth a row more.
const LISTED_QUEUE: usize = 6;
/// The column a stage name is right-aligned into in the queue, so every bar
/// starts at the same column: wide enough for the longest stage `write.rs`
/// reports, "rewriting the container".
const STAGE_COLS: usize = 23;
/// Columns left clear at the right of the queue, matching where the badge
/// bar's own text stops.
const RIGHT_MARGIN: usize = 2;

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
    // The view line belongs to the band: it says which file the band is
    // showing, so it goes where the band goes and not where it does not.
    let view_h = u16::from(header_h > 0);
    let chunks = Layout::vertical([
        Constraint::Length(1),        // badge bar: the name and this mode's keys
        Constraint::Length(1),        // breathing room under it
        Constraint::Length(view_h),   // "file 1/6" or "6 files", over the band
        Constraint::Length(header_h), // thumbnail + file facts
        Constraint::Min(3),           // the form
        Constraint::Length(1),        // the mode, and faststart
        Constraint::Length(1),        // status / validation
    ])
    .split(area);

    draw_badge_bar(f, chunks[0], app, view_h == 0);

    // A dialog takes everything below the header: it is the whole message.
    // A running write is not one: the form stays live while the queue drains,
    // with its bar on the status line and the queue in the band.
    if app.help || app.pending.is_some() || app.results.is_some() {
        let top = chunks[2].y;
        // The last row is not the dialog's: a write already draining keeps
        // its bar there. `w` over a running queue raises the confirmation
        // for the next batch while the writer works, and a bar that vanishes
        // under that dialog is hidden at the one moment it is most wanted.
        let body = Rect {
            x: area.x,
            y: top,
            width: area.width,
            height: area
                .height
                .saturating_sub(top.saturating_sub(area.y))
                .saturating_sub(1),
        };
        if app.help {
            draw_help(f, body, app);
        } else if let Some(plans) = &app.pending {
            draw_confirm(f, body, app, plans);
        } else if let Some(r) = &app.results {
            draw_results(f, body, r);
        }
        write_line(f, chunks[6], app);
        return;
    }

    if header_h > 0 {
        draw_view_line(f, chunks[2], app);
        if let Some(locate) = &app.locate {
            draw_locate(f, chunks[3], app, locate);
        } else if app.import_menu {
            draw_import(f, chunks[3], app);
        } else if app.inspector {
            draw_inspector(f, chunks[3], app);
        } else {
            draw_header(f, chunks[3], app, proto);
        }
    }
    draw_fields(f, chunks[4], app);
    draw_mode_bar(f, chunks[5], app);
    draw_status(f, chunks[6], app);
}

/// Which file the band below is about, or that it is about all of them, and
/// where the edits stand: the line that used to sit beside the logo, moved to
/// the thing it describes. The state is said only when there is one -- a
/// clean selection reads "9 files" and nothing more.
///
/// Bulk counts files in each state (`9 files · 1 writing · 3 queued · 2
/// staged`); a single file says its own (`file 1 of 9 · queued, 2 ahead`).
fn view_spans(app: &App) -> Vec<Span<'static>> {
    let n = app.files.len();
    let mut spans = Vec::new();
    let mut part = |text: String, fg: ratatui::style::Color| {
        spans.push(Span::styled(
            if spans.is_empty() {
                text
            } else {
                format!(" · {text}")
            },
            Style::default().fg(fg),
        ));
    };
    match app.view {
        Some(i) => {
            part(format!("file {} of {n}", i + 1), t::label());
            match app.queue_place(i) {
                Some(QueuePlace::Busy) => part("writing".into(), t::accent()),
                Some(QueuePlace::Waiting(0)) => part("queued, next".into(), t::star()),
                Some(QueuePlace::Waiting(k)) => part(format!("queued, {k} ahead"), t::star()),
                None if app.staged.get(&i).is_some_and(|e| !e.is_empty()) => {
                    part("staged changes".into(), t::staged())
                }
                None => {}
            }
            if app.rename_after.contains(&i) {
                part("renamed after the write".into(), t::muted());
            }
        }
        None => {
            part(format!("{n} file{}", plural(n)), t::label());
            let p = app.pending();
            if p.writing > 0 {
                part(format!("{} writing", p.writing), t::accent());
            }
            if p.queued > 0 {
                part(format!("{} queued", p.queued), t::star());
            }
            if p.staged > 0 {
                part(format!("{} staged", p.staged), t::staged());
            }
            if p.rename > 0 {
                part(format!("{} to rename", p.rename), t::muted());
            }
        }
    }
    spans
}

fn draw_view_line(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(view_spans(app));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The name sits in a filled badge and the bar carries its own background the
/// full width, so the header reads as a title rather than as one more row of
/// text competing with the form. At its right, the one key that finds every
/// other key: `?`, kept apart from the mode's list so it is never the hint a
/// narrow terminal drops.
///
/// `view` puts the view line here too, for a terminal too short to have the
/// band that normally carries it.
fn draw_badge_bar(f: &mut Frame, area: Rect, app: &App, view: bool) {
    let bar = Style::default().bg(t::header_bg());
    let mut right: Vec<Span> = if view {
        view_spans(app)
            .into_iter()
            .map(|s| s.patch_style(bar))
            .collect()
    } else {
        Vec::new()
    };
    let help = shortcut_pairs(app).contains(&HELP);
    if help {
        if !right.is_empty() {
            right.push(Span::raw("   "));
        }
        right.extend(hint_spans(&[HELP], usize::MAX).0);
    }
    let right_w: usize = right.iter().map(|s| s.content.width()).sum();
    let badge = format!(" {} ", iconed(LOGO_ICON, "tagform"));
    // The hint carries two trailing spaces of its own, which is the margin
    // everything else on the right keeps; without it, the plain two.
    let tail = if help { "" } else { "  " };
    let gap = (area.width as usize).saturating_sub(badge.width() + right_w + tail.width());

    let mut spans = vec![
        Span::styled(
            badge,
            Style::default()
                .bg(t::badge_bg())
                .fg(t::badge_fg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(right);
    spans.push(Span::raw(tail));
    f.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, proto: Option<&mut StatefulProtocol>) {
    if app.view.is_none() && app.files.len() > 1 {
        return draw_file_list(f, area, app);
    }
    let idx = app.current_file();
    let Some(file) = app.files.get(idx) else {
        return;
    };

    let want = app
        .thumb_aspect
        .map(|a| thumb_cols(area.height, a))
        .unwrap_or(0);
    // The column is reserved as soon as the aspect is known, picture or not:
    // the facts beside it must not slide left and back while ffmpeg seeks.
    let has_thumb = want > 0 && area.width > want + 21;
    // One blank column before the picture: the column the form's caret takes,
    // so the picture's left edge is the labels' left edge, and the file list's.
    let cols = Layout::horizontal([
        Constraint::Length(u16::from(has_thumb)),
        Constraint::Length(if has_thumb { want } else { 0 }),
        Constraint::Min(10),
    ])
    .split(area);
    let (pic, cols) = (cols[1], &cols[1..]);

    if has_thumb {
        if let Some(p) = proto {
            f.render_stateful_widget(StatefulImage::default(), pic, p);
        }
    }

    let name = file_label(&file.path);
    let dir = file
        .path
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    let summary = app.media.get(idx).map(|m| m.summary()).unwrap_or_default();
    // The indent is a block padding, not a prefix on the string: a long
    // filename wraps, and a wrapped line has to keep the indent the first one
    // had or the header loses its left edge.
    let pad = if has_thumb { 2 } else { 1 };

    // The same mark the bulk list gives this file, so the name reads the
    // same in either view: the file icon, or the queue mark while it waits.
    let (icon, icon_fg) = match app.queue_place(idx) {
        Some(QueuePlace::Busy) => (QUEUE_ICON, t::accent()),
        Some(_) => (QUEUE_ICON, t::staged()),
        None => (FILE_ICON, t::accent()),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(iconed(icon, ""), Style::default().fg(icon_fg)),
            Span::styled(
                name,
                Style::default()
                    .fg(t::header_fg())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            if summary.is_empty() {
                "probing…".into()
            } else {
                summary
            },
            Style::default().fg(t::muted()),
        )),
        Line::from(Span::styled(dir, Style::default().fg(t::path()))),
    ];
    // What is wrong with this file, on its own page, in the error colour:
    // one line, so a second alert cannot push the write's bar out of a band
    // six rows tall. The status line has the room for the full reason.
    let alerts = app.file_alerts(idx);
    if !alerts.is_empty() {
        let w = (cols[1].width as usize).saturating_sub(pad as usize + 1);
        let text = iconed(ALERT_ICON, &alerts.join(" · "));
        lines.push(Line::from(Span::styled(
            t::fit(&text, text.width().min(w)),
            Style::default().fg(t::error()).add_modifier(Modifier::BOLD),
        )));
    }
    // The file in front of you is the file being written: its own bar goes
    // under its facts, wide, because this panel has the room the status line
    // does not. The bar bottom-right is the batch; this one is this file.
    if let Some(place) = app.queue_place(idx) {
        let w = (cols[1].width as usize).saturating_sub(pad as usize + 1);
        let bar_w = w.saturating_sub(STAGE_COLS + 8).clamp(4, 48);
        let (stage, frac) = match (place, &app.progress) {
            (QueuePlace::Busy, Some(p)) => (p.label.to_string(), Some(p.frac)),
            (QueuePlace::Busy, None) => ("writing".into(), Some(0.0)),
            (QueuePlace::Waiting(0), _) => ("next".into(), None),
            (QueuePlace::Waiting(n), _) => (format!("{n} ahead"), None),
        };
        lines.push(Line::from(""));
        // Bar first, stage after: the bar holds still at the left edge of the
        // facts while the stage beside it changes length from "preparing" to
        // "replacing the original".
        let mut row = bar(bar_w, frac.unwrap_or(0.0)).spans;
        if let Some(fr) = frac {
            row.push(Span::styled(
                format!(" {:>3}%", (fr * 100.0).round() as u32),
                Style::default().fg(t::accent()),
            ));
        }
        row.push(Span::styled(
            format!("  {stage}"),
            Style::default().fg(if frac.is_some() {
                t::accent()
            } else {
                t::muted()
            }),
        ));
        lines.push(Line::from(row));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::new().padding(Padding::left(pad)))
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// Bulk view's header: the selection itself, where a single file shows its
/// picture. One file's thumbnail over a form that edits forty is a claim
/// about the wrong file. Five names and then the count -- the band is six
/// rows, and a list that has to scroll is a list nobody reads; what the
/// sixth row can usefully say is how many there are. A file waiting in the
/// write queue carries the queue mark, so the list also says what `w` has
/// already taken.
fn draw_file_list(f: &mut Frame, area: Rect, app: &App) {
    // A write running replaces the selection with the queue: while `w` is
    // draining, what the panel is for is "which file, and how far", and the
    // selection is the thing you already know.
    let listed = (area.height as usize).saturating_sub(1).min(LISTED_QUEUE);
    let (queue, queued) = app.queue_rows(listed);
    if queued > 0 {
        return draw_queue(f, area, app, &queue, queued);
    }
    let n = app.files.len();
    let width = (area.width as usize).saturating_sub(5);
    let mut lines: Vec<Line> = app
        .files
        .iter()
        .take(LISTED_FILES)
        .enumerate()
        .map(|(i, file)| {
            // The queue mark takes the icon's column when the file is
            // waiting: one glyph per row, and the row's state is the glyph.
            let (icon, colour) = match app.queue_place(i) {
                Some(_) => (QUEUE_ICON, t::staged()),
                None => (FILE_ICON, t::accent()),
            };
            // A file with something wrong -- a rename that would land on
            // another file, a value the write will refuse -- is listed in
            // the error colour, icon and all, so the ones to open are found
            // without opening each.
            let bad = !app.file_alerts(i).is_empty();
            Line::from(vec![
                Span::styled(
                    format!(" {}", iconed(icon, "")),
                    Style::default().fg(if bad { t::error() } else { colour }),
                ),
                Span::styled(
                    t::fit(&file_label(&file.path), width),
                    Style::default().fg(if bad { t::error() } else { t::header_fg() }),
                ),
            ])
        })
        .collect();
    if n > LISTED_FILES {
        // The total is on the view line just above; what this row owes is
        // how many the list did not show -- and how many of those are in
        // trouble, since a red name past the fifth is a red name nobody sees.
        let hidden_bad = (LISTED_FILES..n)
            .filter(|&i| !app.file_alerts(i).is_empty())
            .count();
        let mut more = vec![Span::styled(
            format!("    … {} more", n - LISTED_FILES),
            Style::default().fg(t::muted()),
        )];
        if hidden_bad > 0 {
            more.push(Span::styled(
                format!(" · {hidden_bad} with problems"),
                Style::default().fg(t::error()),
            ));
        }
        lines.push(Line::from(more));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// The write queue, in the order it will be taken: the file under the writer
/// on top with a live bar, the ones behind it with the same geometry and an
/// empty one. Six rows and then the count -- the band cannot grow, and a
/// queue of forty that stops at six without saying so is a lie about how
/// much is left.
fn draw_queue(f: &mut Frame, area: Rect, app: &App, rows: &[QueueRow], total: usize) {
    let width = area.width as usize;
    // Stage, bar and percentage sit against the right edge, the same two
    // columns in from it the badge bar's own text stops at; the name takes
    // everything to their left. Anchored there, the bars form one column
    // whatever the names are, and a long name costs its own tail rather than
    // pushing the one thing that moves off into the middle of the row.
    let bar_w = width.saturating_sub(STAGE_COLS + 40).clamp(6, 24);
    let stage_w = STAGE_COLS.min(width / 4);
    let right_w = stage_w + 1 + bar_w + 5 + RIGHT_MARGIN;
    let name_w = width.saturating_sub(4 + 1 + right_w).max(4);
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|r| {
            let (stage, frac) = match (r.busy, &app.progress) {
                (true, Some(p)) => (p.label.to_string(), Some(p.frac)),
                (true, None) => ("writing".to_string(), Some(0.0)),
                (false, _) => ("waiting".to_string(), None),
            };
            let mut spans = vec![
                // One glyph for the whole column: every row here is in the
                // queue, and which one is moving is said by the bar and the
                // stage beside it, not by a second vocabulary of icons.
                Span::styled(
                    format!(" {}", iconed(QUEUE_ICON, "")),
                    Style::default().fg(if r.busy { t::accent() } else { t::staged() }),
                ),
                Span::styled(
                    format!("{} ", t::fit(&r.name, name_w)),
                    Style::default().fg(if r.busy { t::header_fg() } else { t::muted() }),
                ),
            ];
            spans.extend(progress_row(&stage, stage_w, frac, bar_w));
            Line::from(spans)
        })
        .collect();
    if total > rows.len() {
        lines.push(Line::from(Span::styled(
            format!("    … {} more waiting", total - rows.len()),
            Style::default().fg(t::muted()),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// One progress row: the stage, a bar, and a percentage -- or, for a file
/// still waiting its turn, the same geometry with the bar left empty. The
/// geometry is shared so a column of them reads as one queue rather than as
/// six unrelated rows. The stage is right-aligned in its `stage_w` columns so
/// it sits against its bar however long it is.
fn progress_row(
    stage: &str,
    stage_w: usize,
    frac: Option<f64>,
    bar_w: usize,
) -> Vec<Span<'static>> {
    let running = frac.is_some();
    let stage = t::fit(stage, stage.width().min(stage_w));
    let mut spans = vec![Span::styled(
        format!(
            "{}{stage} ",
            " ".repeat(stage_w.saturating_sub(stage.width()))
        ),
        Style::default().fg(if running { t::accent() } else { t::muted() }),
    )];
    spans.extend(bar(bar_w, frac.unwrap_or(0.0)).spans);
    spans.push(match frac {
        Some(fr) => Span::styled(
            format!(" {:>3}%", (fr * 100.0).round() as u32),
            Style::default().fg(t::accent()),
        ),
        None => Span::raw("     "),
    });
    spans
}

/// The import menu, in the band the inspector uses: each source with what it
/// would bring, so the choice is made looking at the answer rather than at a
/// key name. The filename line is the honest one -- it is computed, not
/// promised -- and it says which fields it would fill and which the file
/// already holds, because the import never overwrites.
///
/// A caret marks the source j/k are sitting on. The letter chips stay beside
/// it: the cursor is for choosing while reading the preview, the letters for
/// when the choice was made before the menu opened.
fn draw_import(f: &mut Frame, area: Rect, app: &App) {
    let p = app.import_preview();
    let key = |k: &str| {
        Span::styled(
            format!(" {k} "),
            Style::default()
                .bg(t::rule())
                .fg(t::accent())
                .add_modifier(Modifier::BOLD),
        )
    };
    let name = |s: &str, on: bool| {
        let style = Style::default().fg(t::label_focus());
        Span::styled(
            format!(" {s:<9}"),
            if on {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            },
        )
    };
    // One column, always painted, so the two lines do not shift sideways as
    // the cursor moves between them.
    let caret =
        |on: bool| Span::styled(if on { "▸" } else { " " }, Style::default().fg(t::accent()));
    let on_url = app.import_pick == ImportSource::Url;
    let on_name = app.import_pick == ImportSource::Filename;
    let on_place = app.import_pick == ImportSource::Location;
    let muted = Style::default().fg(t::muted());
    let width = area.width as usize;
    // Truncate without padding: these are values set side by side, not a
    // column to line up.
    let clip = |s: &str, max: usize| t::fit(s, s.width().min(max));
    let fit = |s: &str, used: usize| clip(s, width.saturating_sub(used).max(8));

    let scope_note = if p.files > 1 {
        format!("  {} files, each from its own", p.files)
    } else {
        String::new()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            " import ",
            Style::default().bg(t::rule()).fg(t::label_focus()),
        ),
        Span::styled(
            format!("  from where?  fills the empty fields, keeps the rest{scope_note}"),
            muted,
        ),
    ])];

    // The URL line: what yt-dlp would be asked.
    let url_text = match &p.url {
        Some(u) => fit(u, 16),
        None => "no URL on this file".into(),
    };
    lines.push(Line::from(vec![
        caret(on_url),
        key("u"),
        name("url", on_url),
        Span::styled(
            url_text,
            Style::default().fg(if p.url.is_some() {
                t::value()
            } else {
                t::value_empty()
            }),
        ),
    ]));

    // The filename line, and under it what the parse found.
    lines.push(Line::from(vec![
        caret(on_name),
        key("f"),
        name("filename", on_name),
        Span::styled(fit(&p.stem, 16), Style::default().fg(t::value())),
    ]));
    let mut found: Vec<Span> = vec![Span::raw("               ")];
    if p.fills.is_empty() && p.keeps.is_empty() {
        found.push(Span::styled(
            "nothing recognised in the name",
            Style::default().fg(t::value_empty()),
        ));
    } else {
        for (n, (label, value)) in p.fills.iter().enumerate() {
            if n > 0 {
                found.push(Span::styled(" · ", muted));
            }
            found.push(Span::styled(
                format!("{label} "),
                Style::default().fg(t::staged()),
            ));
            let shown = match value {
                Value::Text(s) if label == "Rating" => stars_glyphs(s.parse().unwrap_or(0)),
                Value::Text(s) => s.clone(),
                Value::List(l) => l.join(", "),
            };
            found.push(Span::styled(
                clip(&shown, 30),
                Style::default().fg(t::value()),
            ));
        }
        if !p.keeps.is_empty() {
            if !p.fills.is_empty() {
                found.push(Span::styled("  ·  ", muted));
            }
            found.push(Span::styled(format!("keeps {}", p.keeps.join(", ")), muted));
        }
    }
    lines.push(Line::from(found));

    // The place line: what the lookup would start from, or that it would
    // name the camera's coordinates, or that there is nothing yet to go on.
    let (place_text, place_fg) = match (&p.place, p.coords) {
        (Some(place), _) => (fit(place, 16), t::value()),
        (None, Some((lat, lon))) => (
            format!("name the place at {}", crate::geocode::iso6709(lat, lon)),
            t::value(),
        ),
        (None, None) => ("type a place to look up".to_string(), t::value_empty()),
    };
    lines.push(Line::from(vec![
        caret(on_place),
        key("l"),
        name("location", on_place),
        Span::styled(place_text, Style::default().fg(place_fg)),
    ]));
    lines.push(Line::from(vec![
        Span::raw(" "),
        key("j/k"),
        Span::styled(" choose  ", muted),
        key("⏎"),
        Span::styled(" import  ", muted),
        key("esc"),
        Span::styled(" cancel", muted),
    ]));
    f.render_widget(Paragraph::new(lines), area);
}

/// The place lookup, in the import band's place (§5.5): the line being typed,
/// then the wait, then the hits to choose from. One hit never reaches here --
/// it is staged on arrival -- so a list on screen always means a choice.
fn draw_locate(f: &mut Frame, area: Rect, app: &App, locate: &Locate) {
    let muted = Style::default().fg(t::muted());
    let key = |k: &str| {
        Span::styled(
            format!(" {k} "),
            Style::default()
                .bg(t::rule())
                .fg(t::accent())
                .add_modifier(Modifier::BOLD),
        )
    };
    let chip = Span::styled(
        " locate ",
        Style::default().bg(t::rule()).fg(t::label_focus()),
    );
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    match locate {
        Locate::Ask(ed) => {
            let p = app.import_preview();
            let hint = match p.coords {
                Some(_) => "  a place, or empty to name the camera's coordinates",
                None => "  a place: a venue, a street, a city",
            };
            lines.push(Line::from(vec![chip, Span::styled(hint, muted)]));
            let (text, cur) = ed.display();
            const LEAD: u16 = 3;
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    t::fit(&text, width.saturating_sub(LEAD as usize + 1)),
                    Style::default().fg(t::value()),
                ),
            ]));
            if let Some(c) = cur {
                let x = area.x + LEAD + (c as u16).min(area.width.saturating_sub(LEAD + 1));
                f.set_cursor_position((x, area.y + 1));
            }
            lines.push(Line::from(vec![
                Span::raw(" "),
                key("⏎"),
                Span::styled(" look up  ", muted),
                key("esc"),
                Span::styled(" cancel", muted),
            ]));
        }
        Locate::Looking => {
            lines.push(Line::from(vec![
                chip,
                Span::styled("  asking MapKit…", muted),
            ]));
            lines.push(Line::from(vec![
                Span::raw(" "),
                key("esc"),
                Span::styled(" cancel", muted),
            ]));
        }
        Locate::Pick { hits, at, .. } => {
            lines.push(Line::from(vec![
                chip,
                Span::styled(
                    format!("  {} places match -- which one?", hits.len()),
                    muted,
                ),
            ]));
            // The band is short; keep the cursor's row on screen.
            let room = (area.height as usize).saturating_sub(2).max(1);
            let first = at.saturating_sub(room - 1);
            for (i, h) in hits.iter().enumerate().skip(first).take(room) {
                let on = i == *at;
                let caret = Span::styled(
                    if on { "▸ " } else { "  " },
                    Style::default().fg(t::accent()),
                );
                let style = Style::default().fg(t::value());
                lines.push(Line::from(vec![
                    caret,
                    Span::styled(
                        t::fit(&h.summary(), width.saturating_sub(3)),
                        if on {
                            style.add_modifier(Modifier::BOLD)
                        } else {
                            style
                        },
                    ),
                ]));
            }
            lines.push(Line::from(vec![
                Span::raw(" "),
                key("j/k"),
                Span::styled(" choose  ", muted),
                key("⏎"),
                Span::styled(" take it  ", muted),
                key("esc"),
                Span::styled(" cancel", muted),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// The answer to "what does ‹multiple› actually contain" -- the thing the old
/// fzf-based tagger could only show in a preview pane.
fn draw_inspector(f: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.rows.get(app.focus) else {
        return;
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {} ", row.label),
            Style::default().bg(t::rule()).fg(t::label_focus()),
        ),
        Span::styled("  per file", Style::default().fg(t::muted())),
    ])];

    // The effective values, so a per-file edit is visible here as the thing
    // that will be written to that file.
    let scope = app.scope();
    match &row.eff {
        Agg::Mixed { values } => {
            for (i, v) in values.iter().enumerate() {
                let file = scope.get(i).copied().unwrap_or(i);
                let edited = app.file_is_staged(file, &row.key);
                let style = if edited {
                    Style::default().fg(t::staged())
                } else if v.is_some() {
                    Style::default().fg(t::value())
                } else {
                    Style::default().fg(t::value_empty())
                };
                // Chips here for the same reason as on the row, but the
                // payoff is larger: this pane is a column of one field across
                // every file, so a tag missing from one of them is a gap in a
                // colour rather than a word to find twice.
                let mut spans = vec![Span::raw(" ")];
                match v {
                    Some(Value::List(l))
                        if !l.is_empty()
                            && matches!(row.control, Control::List | Control::HashTags) =>
                    {
                        let sigil = if edited { t::staged() } else { t::muted() };
                        spans.extend(tag_spans(
                            l,
                            row.control == Control::HashTags,
                            34,
                            ratatui::style::Color::Reset,
                            sigil,
                        ));
                    }
                    _ => {
                        let shown = match v {
                            Some(Value::Text(s)) => s.clone(),
                            Some(Value::List(l)) => l.join(" · "),
                            None => "—".into(),
                        };
                        spans.push(Span::styled(t::fit(&shown, 34), style));
                    }
                }
                spans.push(Span::styled(
                    app.files
                        .get(file)
                        .map(|f| file_label(&f.path))
                        .unwrap_or_default(),
                    Style::default().fg(t::muted()),
                ));
                lines.push(Line::from(spans));
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
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t::rule()));
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
    // Bulk is the aggregate view over more than one file: every edit made
    // here lands on all of them, and the form says so -- on the rule, and
    // beside every value.
    let n_files = format!("{} files", app.files.len());
    let bulk = (app.view.is_none() && app.files.len() > 1)
        .then(|| iconed(BULK_ICON, &format!("bulk edit mode - {n_files}")));
    // In single-file view the rule says instead where this file stands in the
    // write queue, if it is in it: how many are written before it, or that it
    // is under the writer now -- which is the one moment an edit here does
    // not fold into its write.
    let queued = app
        .view
        .and_then(|i| app.queue_place(i))
        .map(|place| match place {
            QueuePlace::Busy => iconed(QUEUE_ICON, "writing now"),
            QueuePlace::Waiting(0) => iconed(QUEUE_ICON, "queued for write - next up"),
            QueuePlace::Waiting(k) => iconed(
                QUEUE_ICON,
                &format!("queued for write - {k} file{} left", plural(k)),
            ),
        });
    let heading = match (&bulk, &queued) {
        (Some(h), _) => Some((h.as_str(), t::muted())),
        (None, Some(h)) => Some((h.as_str(), t::staged())),
        (None, None) => None,
    };
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
        // The focused row is filled edge to edge, not just in its input box.
        // The caret and the lit box said "here" only in the two columns you
        // were already looking at; a band across the label as well is what you
        // find without looking, which on a twenty-row form is the whole job of
        // a cursor. Deliberately the *focus* tint and not the edit one, so an
        // open field's box still stands out from the row carrying it.
        let row_bg = focused.then(t::input_bg_focus);
        let banded = |st: Style| match row_bg {
            Some(b) => st.bg(b),
            None => st,
        };
        let label_style = if focused {
            banded(Style::default().fg(label_fg).add_modifier(Modifier::BOLD))
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
        } else if bulk.is_some() && row.is_mixed() {
            // The files disagree: there is no one value to draw, and the count
            // says how many answers the edit is about to replace. In the
            // aggregate colour, which is what it is.
            (format!("multiple values ({})", n_files), t::mixed())
        } else {
            // One rule for every row, chips included: the colour says where
            // the value stands, never what it says. On the file and sound in
            // the value colour, about to be written in staged green, refused
            // by the write in red, an aggregate of disagreeing files subdued.
            match display_row(app, row).filter(|v| !v.is_empty()) {
                Some(v) if app.row_error(row).is_some() => (v, t::error()),
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
        // Beside an agreed value in bulk view: how many files it stands for,
        // which is also how many a save will land on. Gone while the field is
        // open, so the text being typed has the whole box.
        let count_hint = match (&bulk, editing, &row.eff) {
            (Some(_), false, Agg::Same { .. }) => Some(n_files.clone()),
            _ => None,
        };
        // Star colour belongs to stars on the file. An empty rating draws the
        // same "—" as every other empty field and must look like one, and a
        // rating about to be written is green like every other staged value.
        let has_value = app.shown_value(row).is_some();
        let value_fg = if row.control == Control::Stars && !editing && has_value && !staged {
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
        // Chips replace the flat string only where the flat string was all
        // there was to say: an open row is showing the editor's text, and a
        // mixed row in bulk is showing a count of disagreements, not a list.
        let chips = (!editing && !(bulk.is_some() && row.is_mixed()))
            .then(|| tag_list(row))
            .flatten();
        let value_spans = match set {
            Some((labels, sel, counts)) => set_spans(
                &labels,
                sel,
                &counts,
                text_w + PAD as usize - lead,
                bg,
                focused,
                staged,
            ),
            None => {
                // The count is dropped, not the value, when the box is too
                // narrow for both.
                let hint = count_hint.filter(|h| text_w > h.width() + 6);
                let body_w = match &hint {
                    Some(h) => text_w - h.width() - 2,
                    None => text_w,
                };
                let mut spans = match chips {
                    Some((items, hash)) => tag_spans(&items, hash, body_w, bg, fg),
                    None => vec![Span::styled(
                        t::fit(&raw, body_w),
                        Style::default().bg(bg).fg(value_fg),
                    )],
                };
                if let Some(h) = hint {
                    spans.push(Span::styled(
                        format!("  {h}"),
                        Style::default().bg(bg).fg(t::muted()),
                    ));
                }
                spans
            }
        };

        let mut spans = vec![
            Span::styled(marker, banded(Style::default().fg(marker_fg))),
            Span::styled(
                t::fit(
                    &if custom {
                        t::short_key(&row.label)
                    } else {
                        row.label.clone()
                    },
                    LABEL_COLS as usize,
                ),
                label_style,
            ),
            Span::styled(" ".repeat(GUTTER as usize), banded(Style::default())),
            Span::styled(" ".repeat(lead), Style::default().bg(bg)),
        ];
        spans.extend(value_spans);
        spans.push(Span::styled(
            " ".repeat(PAD as usize),
            Style::default().bg(bg),
        ));
        // The column the value box does not reach. Painted only on the focused
        // row, so the band closes rather than stopping one column short.
        let drawn = 1 + LABEL_COLS + GUTTER + value_w as u16;
        if let Some(tail) = inner.width.checked_sub(drawn).filter(|_| focused) {
            spans.push(Span::styled(
                " ".repeat(tail as usize),
                banded(Style::default()),
            ));
        }
        lines.push(Line::from(spans));
        if group_break_after(row) {
            lines.push(group_rule(inner.width as usize, heading));
        }
    }

    let start = if focus_line >= height {
        focus_line + 1 - height
    } else {
        0
    };
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if let Some((x, line)) = cursor_line {
        if let Some(y) = line.checked_sub(start).filter(|y| *y < height) {
            f.set_cursor_position((x, inner.y + y as u16));
        }
    }
}

/// Category and Variant are drawn together above a rule. They are not fields
/// among the others: they say what the file *is* and which version of it this
/// is, and which of the fields below are worth showing at all (DESIGN §3.5,
/// §16). A form where that choice sits eighth, indistinguishable from Tags,
/// hides the one answer everything else follows.
fn group_break_after(row: &Row) -> bool {
    row.def.is_some_and(|d| d.id == "variant")
}

/// The rule under Category and Variant. In bulk view it carries the heading
/// that says every edit below it is about to land on the whole selection; in
/// single-file view, the file's place in the write queue. The heading is
/// drawn inverted, a tab set into the rule: a state that changes what ⏎
/// does has to be read without being looked for, and a bold grey word on a
/// grey line was not.
fn group_rule(width: usize, heading: Option<(&str, ratatui::style::Color)>) -> Line<'static> {
    let rule = |n: usize| Span::styled("\u{2500}".repeat(n), Style::default().fg(t::rule()));
    match heading {
        Some((h, colour)) if h.width() + 4 <= width => {
            let text = format!(" {h} ");
            let left = (width - text.width()) / 2;
            let right = width - text.width() - left;
            Line::from(vec![
                rule(left),
                Span::styled(
                    text,
                    Style::default()
                        .fg(colour)
                        .add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ),
                rule(right),
            ])
        }
        _ => Line::from(rule(width)),
    }
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
///
/// The third part is the mixed row's answer instead of a selection: how many
/// files in the selection hold each option, zero for the ones none do, empty
/// when the row agrees. `Original 4  Clip 2` says what the files disagree
/// about, where a row with nothing lit only said that they do.
fn closed_set(app: &App, row: &Row) -> Option<(Vec<String>, Option<usize>, Vec<usize>)> {
    if row.control != Control::Enum {
        return None;
    }
    let mut opts = app.options_for(row);
    if opts.is_empty() {
        return None;
    }
    let mut counts = Vec::new();
    if let Agg::Mixed { values } = &row.eff {
        for v in values {
            let Some(Value::Text(code)) = v else { continue };
            if code.trim().is_empty() {
                continue;
            }
            let i = match opts.iter().position(|o| &o.code == code) {
                Some(i) => i,
                None => {
                    opts.push(Opt {
                        code: code.clone(),
                        label: code.clone(),
                    });
                    opts.len() - 1
                }
            };
            counts.resize(opts.len(), 0);
            counts[i] += 1;
        }
        if !counts.is_empty() {
            counts.resize(opts.len(), 0);
        }
    }
    let sel = match app.shown_value(row) {
        Some(Value::Text(s)) if !s.trim().is_empty() && (!row.is_mixed() || row.staged) => {
            match opts.iter().position(|o| o.code == s) {
                Some(i) => Some(i),
                None => {
                    opts.push(Opt {
                        code: s.clone(),
                        label: s,
                    });
                    Some(opts.len() - 1)
                }
            }
        }
        _ => None,
    };
    Some((opts.into_iter().map(|o| o.label).collect(), sel, counts))
}

/// The set, laid out along the value box with the current one lit.
///
/// Scrolls to keep the selection in view rather than eliding the tail: with
/// seven kinds and "Podcast" chosen, a naive trim showed every option except
/// the one you were on.
/// `lit` is whether the row is the one you are on: an open or focused set lights
/// its selection in accent, a set sitting quietly further down the form marks
/// it without competing with the caret for attention. `staged` outranks both:
/// a selection that is about to be written is drawn the way a staged text
/// value is -- staged green on the box's own ground, bold -- so the edit reads
/// on the value as well as on the label, focused or not.
///
/// `counts`, when the row is mixed, puts each held option's file count after
/// its name -- `Original 4` -- the name in the value colour and the number in
/// the lighter mixed one, so the options the files actually hold stand out
/// from the ones none do without any of them reading as chosen.
fn set_spans(
    labels: &[String],
    sel: Option<usize>,
    counts: &[usize],
    width: usize,
    bg: ratatui::style::Color,
    lit: bool,
    staged: bool,
) -> Vec<Span<'static>> {
    let held = |i: usize| counts.get(i).copied().unwrap_or(0);
    let tail = |i: usize| match held(i) {
        0 => " ".to_string(),
        n => format!(" {n} "),
    };
    let cell_w = |i: usize| 1 + labels[i].width() + tail(i).width();
    // Scroll to keep the selection in view -- or, on a mixed row, the first
    // option anyone holds, since that is what the row is there to say.
    let anchor = sel
        .or_else(|| (0..labels.len()).find(|&i| held(i) > 0))
        .unwrap_or(0);
    let mut first = 0usize;
    loop {
        let used: usize = (first..labels.len()).map(cell_w).sum();
        if used <= width || first >= anchor {
            break;
        }
        first += 1;
    }

    let mut spans = Vec::new();
    let mut used = 0usize;
    for (i, label) in labels.iter().enumerate().skip(first) {
        if used + cell_w(i) > width {
            break;
        }
        used += cell_w(i);
        if held(i) > 0 {
            spans.push(Span::styled(
                format!(" {label}"),
                Style::default()
                    .bg(bg)
                    .fg(t::value())
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                tail(i),
                Style::default().bg(bg).fg(t::mixed()),
            ));
            continue;
        }
        spans.push(Span::styled(
            format!(" {label} "),
            match (Some(i) == sel, staged, lit) {
                (true, true, _) => Style::default()
                    .bg(bg)
                    .fg(t::staged())
                    .add_modifier(Modifier::BOLD),
                (true, false, true) => Style::default()
                    .bg(t::accent())
                    .fg(t::badge_fg())
                    .add_modifier(Modifier::BOLD),
                (true, false, false) => Style::default()
                    .bg(t::rule())
                    .fg(t::value())
                    .add_modifier(Modifier::BOLD),
                (false, _, _) => Style::default().bg(bg).fg(t::muted()),
            },
        ));
    }
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    spans
}

/// A list value drawn as chips: whole entries, never one cut mid-word, and a
/// count of the ones that did not fit.
///
/// The entries take the row's colour, the one every other row's value takes
/// (§7): the colour says whether the list is on the file, about to be
/// written, or refused -- not which tag it is. A colour per tag, hashed from
/// its text, was tried first; beside rows that coloured by state it read as
/// state, and meant nothing. The `·` between names stays subdued so a list
/// still counts at a glance; a hashtag's `#` is part of the tag and takes its
/// colour.
fn tag_spans(
    items: &[String],
    hash: bool,
    width: usize,
    bg: ratatui::style::Color,
    fg: ratatui::style::Color,
) -> Vec<Span<'static>> {
    let sigil = |first: bool| match (hash, first) {
        (true, _) => "#".to_string(),
        (false, true) => String::new(),
        (false, false) => " · ".to_string(),
    };
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (i, item) in items.iter().enumerate() {
        let lead = if hash && i > 0 { " " } else { "" };
        let sig = format!("{lead}{}", sigil(i == 0));
        let cost = sig.width() + item.width();
        // No room for the next chip: say how many are hidden rather than
        // truncating one mid-word, which reads as a corrupted tag.
        if used + cost > width {
            let rest = format!(" +{}", items.len() - i);
            if used + rest.width() <= width {
                spans.push(Span::styled(
                    rest.clone(),
                    Style::default().bg(bg).fg(t::muted()),
                ));
                used += rest.width();
            }
            break;
        }
        used += cost;
        // A tag that cannot be repaired into a filename token is drawn in the
        // error colour wherever tags are drawn, so the one the write is about
        // to leave out is the one that looks wrong (§5.4) -- and underlined,
        // so it is still the one that looks wrong on a row already red.
        let style = if hash && tag::is_hostile(item) {
            Style::default()
                .bg(bg)
                .fg(t::error())
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().bg(bg).fg(fg)
        };
        if !sig.is_empty() {
            let sig_style = if hash {
                style
            } else {
                Style::default().bg(bg).fg(t::muted())
            };
            spans.push(Span::styled(sig, sig_style));
        }
        spans.push(Span::styled(item.clone(), style));
    }
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    spans
}

/// The chips to draw for a row, or `None` for anything that is not a list --
/// including an empty one, which falls back to the ordinary em dash rather
/// than to a blank box.
fn tag_list(row: &Row) -> Option<(Vec<String>, bool)> {
    let hash = match row.control {
        Control::HashTags => true,
        Control::List => false,
        _ => return None,
    };
    match row.shown()? {
        Value::List(l) if !l.is_empty() => Some((l.clone(), hash)),
        _ => None,
    }
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
        (Value::List(l), Control::HashTags) => l
            .iter()
            .map(|x| format!("#{x}"))
            .collect::<Vec<_>>()
            .join(" "),
        (Value::List(l), _) => l.join(" · "),
        (Value::Text(s), Control::Enum) => app.enum_label(row, s).unwrap_or_else(|| s.clone()),
        (Value::Text(s), _) => s.replace('\n', " "),
    })
}

/// The mode bar: a vim-style mode indicator, then the keys that mode takes,
/// on a ground that is always painted -- lit while a field is open or a
/// one-key menu is armed. The keys sit beside the mode that governs them, so
/// the list changing and the mode changing are one thing to notice, not two;
/// typing into a field you thought was closed is the mistake worth pricing a
/// colour against. `?` is not among them: it lives at the right of the badge
/// bar, where it cannot be truncated away.
fn draw_mode_bar(f: &mut Frame, area: Rect, app: &App) {
    let (mode_name, mode_fg, bar_bg) = mode_of(app);
    let badge = format!(" {mode_name} ");
    // Faststart is a standing setting of the writer, not a fact about the
    // selection, so it lives with the other standing state -- the mode --
    // rather than in the title.
    let fast = format!("faststart {}  ", if app.faststart { "on" } else { "off" });
    let pairs: Vec<(&str, &str)> = shortcut_pairs(app)
        .iter()
        .copied()
        .filter(|p| *p != HELP)
        .collect();
    let room = (area.width as usize).saturating_sub(badge.width() + 1 + fast.width() + 1);
    let (hints, hints_w) = hint_spans(&pairs, room);
    let gap = (area.width as usize).saturating_sub(badge.width() + 1 + hints_w + fast.width());
    let mut spans = vec![
        Span::styled(
            badge,
            Style::default()
                .bg(mode_fg)
                .fg(t::badge_fg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    spans.extend(hints);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(fast, Style::default().fg(t::muted())));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bar_bg)),
        area,
    );
}

/// The help key, which every Normal-mode list starts with and only the badge
/// bar draws.
const HELP: (&str, &str) = ("?", "help");

/// The mode the keys are in: its name, its badge colour, and the ground the
/// mode bar takes. The ground is always painted -- dark in Normal, lit while
/// a field is open or a one-key menu is armed -- because the keys changing
/// under you is easy to miss, and a permanent ground is what makes the lit
/// states read as a change rather than as the bar simply appearing.
fn mode_of(app: &App) -> (&'static str, ratatui::style::Color, ratatui::style::Color) {
    // There is no third mode any more: a fixed set is stepped from Normal
    // with h/l and never opens, so there are only the two states the app
    // actually has.
    let (mode_name, mode_fg, bar_bg) = if app.mode == Mode::Edit {
        ("EDIT", t::staged(), t::input_bg_edit())
    } else {
        ("NORMAL", t::accent(), t::bar_bg())
    };
    // The case menu is modal for exactly one keystroke, and the mode bar is
    // the only place that says so -- so it names the menu outright.
    if app.format_pending {
        ("FORMAT", t::star(), t::input_bg_focus())
    } else if app.locate.is_some() {
        ("LOCATE", t::star(), t::input_bg_focus())
    } else if app.import_menu {
        ("IMPORT", t::star(), t::input_bg_focus())
    } else {
        (mode_name, mode_fg, bar_bg)
    }
}

/// The keys that matter right now, and only those, in the order the badge bar
/// lists them. Which keys are live depends on the mode, so a fixed list would
/// be wrong half the time.
fn shortcut_pairs(app: &App) -> &'static [(&'static str, &'static str)] {
    if let Some(locate) = &app.locate {
        match locate {
            Locate::Ask(_) => &[("(type)", "a place"), ("⏎", "look up"), ("esc", "cancel")],
            Locate::Looking => &[("esc", "cancel")],
            Locate::Pick { .. } => &[("jk", "choose"), ("⏎", "take it"), ("esc", "cancel")],
        }
    } else if app.import_menu {
        &[
            ("jk", "choose"),
            ("⏎", "import"),
            ("u", "from the URL"),
            ("f", "from the filename"),
            ("l", "from a place"),
            ("esc", "cancel"),
        ]
    } else if app.format_pending {
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
            ("esc", "cancel"),
            ("^c", "quit"),
        ]
    } else {
        &[
            // Help is listed with Normal's keys, but drawn apart from them at
            // the right of the badge bar: the list below is truncated to fit
            // the terminal, and the one key that can find every other key
            // must not be what gets cut.
            HELP,
            // h and l move along a set rather than between rows, but they are
            // the same hand's movement keys and a strip that named only two of
            // the four read as though the other two did nothing.
            ("hjkl", "move"),
            ("⏎", "edit"),
            ("w", "write"),
            ("r", "rename"),
            ("i", "import"),
            ("m", "merge"),
            ("I", "inspect"),
            ("][", "file"),
            ("a", "all files"),
            ("o", "open"),
            ("O", "overwrite"),
            ("b", "backfill"),
            ("u", "undo"),
            // The glyph is one column wide by the width tables and wider than
            // that in most terminals, so it carries its own trailing space
            // rather than letting the next label collide with it.
            ("⌫ ", "clear"),
            ("f ~", "format"),
            ("y", "yank"),
            ("p", "paste"),
            ("t", "theme"),
            ("F", "fast"),
            ("q", "quit"),
        ]
    }
}

/// Key chips and their descriptions, fitted to `width`. Hints that do not fit
/// are dropped rather than let run off the edge -- a half-rendered key name is
/// worse than one fewer hint -- and an ellipsis says some were. Returns the
/// spans and the columns they take. The descriptions take the ground of the
/// bar they are drawn on.
fn hint_spans(pairs: &[(&str, &str)], width: usize) -> (Vec<Span<'static>>, usize) {
    let mut spans = Vec::new();
    let mut used = 0usize;
    let mut dropped = false;
    for (k, d) in pairs {
        let key = format!(" {k} ");
        let desc = format!(" {d}  ");
        let w = key.width() + desc.width();
        if used + w + 1 > width {
            dropped = true;
            continue;
        }
        used += w;
        spans.push(Span::styled(
            key,
            Style::default()
                .bg(t::rule())
                .fg(t::accent())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(desc, Style::default().fg(t::muted())));
    }
    if dropped {
        spans.push(Span::styled("…", Style::default().fg(t::muted())));
        used += 1;
    }
    (spans, used)
}

/// One section of the key map as painted rows, each with the width it
/// occupies, so two of them can be set side by side without measuring styled
/// spans twice.
fn help_section(
    s: &crate::ui::keymap::Section,
    keyw: usize,
    descw: usize,
    colw: usize,
) -> Vec<(Vec<Span<'static>>, usize)> {
    let mut out: Vec<(Vec<Span<'static>>, usize)> = vec![(
        vec![Span::styled(
            format!(" {} ", s.title),
            Style::default()
                .bg(t::accent())
                .fg(t::badge_fg())
                .add_modifier(Modifier::BOLD),
        )],
        s.title.width() + 2,
    )];
    // Indented one column so the note sits under the badge's text rather than
    // under its left edge, and measured against the whole column: it is prose,
    // and truncating it to the description column would cut it mid-clause.
    out.push((
        vec![Span::styled(
            format!(" {}", t::fit(s.note, colw.saturating_sub(1))),
            Style::default().fg(t::muted()),
        )],
        colw,
    ));
    out.push((vec![], 0));
    for k in s.binds {
        let pad = " ".repeat(keyw - key_width(k.keys));
        out.push((
            vec![
                Span::styled(
                    format!(" {}{pad} ", k.keys),
                    Style::default()
                        .bg(t::rule())
                        .fg(t::accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", t::fit(k.what, descw)),
                    Style::default().fg(t::value()),
                ),
            ],
            keyw + 3 + descw,
        ));
    }
    out.push((vec![], 0));
    out
}

/// The whole map, in two columns where the terminal is wide enough for them
/// and one where it is not, scrolled by `help_scroll` so a short terminal can
/// still reach the end of it. Sections are never split across the two columns:
/// a heading in one column and its keys in the other is worse than an uneven
/// pair of columns.
fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t::accent()))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let keyw = KEYMAP
        .iter()
        .flat_map(|s| s.binds.iter())
        .map(|k| key_width(k.keys))
        .max()
        .unwrap_or(0);
    let total = inner.width as usize;
    // Two columns need the key legend twice, a readable description twice, and
    // a gutter between them; below that the one-column form reads better than
    // two squeezed ones.
    let gutter = 4usize;
    let two = total >= (keyw + 3 + 34) * 2 + gutter;
    let colw = if two { (total - gutter) / 2 } else { total };
    let descw = colw.saturating_sub(keyw + 3).max(8);

    let sections: Vec<Vec<(Vec<Span<'static>>, usize)>> = KEYMAP
        .iter()
        .map(|s| help_section(s, keyw, descw, colw))
        .collect();

    let mut lines: Vec<Line> = Vec::new();
    if two {
        // Fill the left column until it holds about half the rows, so the two
        // are balanced whatever the table grows into.
        let all: usize = sections.iter().map(Vec::len).sum();
        let mut left: Vec<(Vec<Span<'static>>, usize)> = Vec::new();
        let mut right: Vec<(Vec<Span<'static>>, usize)> = Vec::new();
        for sec in sections {
            if left.len() + sec.len() / 2 <= all / 2 && right.is_empty() {
                left.extend(sec);
            } else {
                right.extend(sec);
            }
        }
        for i in 0..left.len().max(right.len()) {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let used = match left.get(i) {
                Some((s, w)) => {
                    spans.extend(s.clone());
                    *w
                }
                None => 0,
            };
            if let Some((s, _)) = right.get(i) {
                spans.push(Span::raw(
                    " ".repeat(colw + gutter - used.min(colw + gutter)),
                ));
                spans.extend(s.clone());
            }
            lines.push(Line::from(spans));
        }
    } else {
        for sec in sections {
            for (spans, _) in sec {
                lines.push(Line::from(spans));
            }
        }
    }

    // Clamp the scroll to what is left below, so pressing j at the bottom does
    // not scroll the map off the top of its own box.
    let body_h = inner.height.saturating_sub(1);
    let max_scroll = (lines.len() as u16).saturating_sub(body_h);
    app.help_max.set(max_scroll);
    let scroll = app.help_scroll.min(max_scroll);
    let foot = if max_scroll > 0 {
        "  j k ↑ ↓  scroll      any other key  back to the form"
    } else {
        "  any key  back to the form"
    };

    let body = Rect {
        height: body_h,
        ..inner
    };
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            foot,
            Style::default().fg(t::value()).add_modifier(Modifier::BOLD),
        ))),
        Rect {
            y: inner.y + body_h,
            height: 1,
            ..inner
        },
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    // Validation is about the field under the cursor, so it outranks the
    // transient status line -- but only while a field is actually open.
    // A field at rest can still be unwritable -- a staged tag set with a slash
    // in it will be skipped -- and the reason has to be readable without
    // opening the field again, since nothing else says why it never saves.
    let live = match app.mode {
        Mode::Edit => app.validation(),
        _ => app
            .rows
            .get(app.focus)
            .and_then(|r| app.row_error(r))
            .map_or(Validation::Ok, Validation::Error),
    };
    let (text, fg) = match live {
        Validation::Error(m) => (m, t::error()),
        Validation::Warn(m) => (m, t::warn()),
        Validation::Ok if app.status_error => (app.status.clone(), t::error()),
        Validation::Ok if !app.status.is_empty() => (app.status.clone(), t::muted()),
        Validation::Ok => (String::new(), t::muted()),
    };
    // The running write lives at the far right of this line -- under the
    // mode bar, the last thing on the screen (§7). It is the global
    // view: which file of how many, what stage, and the batch's own bar.
    // The per-file detail is in the header panel; this line is for the
    // glance that asks "is it still going, and how far".
    let right_w = write_line(f, area, app);
    // The status text yields to it rather than overrunning it: a truncated
    // message is readable, two messages overlapping are not.
    let room = (area.width as usize).saturating_sub(right_w + 1);
    let text = t::fit(&text, text.width().min(room));
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {text}"),
            Style::default().fg(fg),
        ))),
        Rect {
            width: (room + 1) as u16,
            ..area
        },
    );
}

/// The global write status, painted hard against the right edge of `area`.
/// Returns the columns it took, so the caller knows what is left of the row.
fn write_line(f: &mut Frame, area: Rect, app: &App) -> usize {
    let spans = write_status(app);
    let w: usize = spans.iter().map(|s| s.content.width()).sum();
    if w == 0 {
        return 0;
    }
    let x = area.width.saturating_sub(w as u16);
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect {
            x: area.x + x,
            width: area.width - x,
            ..area
        },
    );
    w
}

/// The global write status: the spans that sit bottom-right while a write
/// runs, and nothing at all when none does.
///
/// Just the position and the bar, with the percentage inside it. The file's
/// name and stage are in the panel at the top, which has the room for them;
/// said here as well, the line read "writing" three times over.
fn write_status(app: &App) -> Vec<Span<'static>> {
    let Some(p) = &app.progress else {
        return Vec::new();
    };
    let overall = p.overall();
    let mut v = vec![Span::styled(
        format!("{}/{} ", p.file + 1, p.total),
        Style::default().fg(t::accent()),
    )];
    v.extend(labelled_bar(
        16,
        overall,
        &format!("{}%", (overall * 100.0).round() as u32),
    ));
    // The same two columns in from the edge the mode bar's faststart stops at.
    v.push(Span::raw("  "));
    v
}

/// A bar drawn in background colour rather than in glyphs, so a label can sit
/// inside it: the label is centred, and each of its characters takes the
/// colour of the half of the bar it lands on -- dark on the filled part,
/// light on the rest -- so it reads the whole way across.
fn labelled_bar(width: usize, frac: f64, label: &str) -> Vec<Span<'static>> {
    let filled = ((width as f64) * frac.clamp(0.0, 1.0)).round() as usize;
    let chars: Vec<char> = label.chars().collect();
    let start = width.saturating_sub(chars.len()) / 2;
    (0..width)
        .map(|i| {
            let ch = i
                .checked_sub(start)
                .and_then(|j| chars.get(j))
                .copied()
                .unwrap_or(' ');
            let style = if i < filled {
                Style::default()
                    .bg(t::accent())
                    .fg(t::badge_fg())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(t::rule()).fg(t::value())
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

/// The plan, in the terms the user thinks in: which field, to what, and by
/// which route. The route matters because it is the difference between an
/// in-place update and a full rewrite of a multi-gigabyte file.
fn draw_confirm(f: &mut Frame, area: Rect, app: &App, plans: &[FilePlan]) {
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" Write {} file{} ", plans.len(), plural(plans.len())),
            Style::default()
                .bg(t::accent())
                .fg(t::badge_fg())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for edit in app.staged_summary() {
        // A refused field is still listed -- it is staged, and hiding it would
        // make the dialog agree with the write for the wrong reason -- but it
        // is listed as what it is: red, and named as not going.
        let value_fg = if edit.refused.is_some() {
            t::error()
        } else {
            t::staged()
        };
        let mut spans = vec![
            Span::styled(
                format!("  {}", t::fit(&edit.label, 14)),
                Style::default().fg(t::label()),
            ),
            Span::styled("→ ", Style::default().fg(t::muted())),
            Span::styled(edit.shown, Style::default().fg(value_fg)),
        ];
        if let Some(why) = &edit.refused {
            spans.push(Span::styled(
                format!("   not written · {why}"),
                Style::default().fg(t::error()),
            ));
        }
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

    // One line per file: its name, elided, and the fields it is about to
    // have changed. The route used to repeat under every name; it is the
    // same for most of a batch, so it is said once per route below, and a
    // name carries its own only when the batch is split between routes.
    let mut routes: Vec<(&str, &str, usize)> = Vec::new();
    for p in plans {
        match routes
            .iter_mut()
            .find(|(w, why, _)| *w == p.writer.label() && *why == p.why)
        {
            Some((_, _, n)) => *n += 1,
            None => routes.push((p.writer.label(), p.why, 1)),
        }
    }
    let split = routes.len() > 1;
    let width = (area.width as usize).saturating_sub(2);
    let route_w = if split {
        plans
            .iter()
            .map(|p| p.writer.label().width())
            .max()
            .unwrap_or(0)
            + 3
    } else {
        0
    };
    let names: Vec<String> = plans.iter().map(|p| file_label(&p.path)).collect();
    // The name identifies; the fields are the news. The name gets two fifths
    // at most, so a long one does not squeeze the changes down to "+8".
    let name_w = names
        .iter()
        .map(|n| n.width())
        .max()
        .unwrap_or(0)
        .min(width * 2 / 5)
        .max(8);
    let fields_w = width.saturating_sub(2 + name_w + 3 + route_w);

    // The dialog cannot scroll, so a batch longer than the room left is
    // listed as far as it fits and then counted.
    let renames_n = plans
        .iter()
        .filter(|p| {
            app.files
                .iter()
                .position(|f| f.path == p.path)
                .is_some_and(|i| app.rename_after.contains(&i))
        })
        .count();
    let fixed = lines.len() + 1 + routes.len() + if renames_n > 0 { 2 } else { 0 } + 4;
    let room = (area.height as usize).saturating_sub(2 + fixed).max(1);
    let shown = if plans.len() > room {
        room.saturating_sub(1).max(1)
    } else {
        plans.len()
    };

    for (p, name) in plans.iter().zip(&names).take(shown) {
        let mut spans = vec![Span::styled(
            format!("  {}   ", t::fit(name, name_w)),
            Style::default().fg(t::header_fg()),
        )];
        // The container's layout only when it is not the usual one: a
        // moov-at-end file is about to be restructured, and that is worth a
        // word; nine lines of "FastStart" were not.
        let layout = match p.layout {
            Container::FastStart => "",
            Container::MoovAtEnd => "moov at end",
            Container::Fragmented => "fragmented",
            Container::Inconclusive => "layout unknown",
        };
        let note = if layout.is_empty() {
            0
        } else {
            layout.width() + 3
        };
        spans.extend(field_chips(
            &app.file_edits(&p.path),
            fields_w.saturating_sub(note),
        ));
        if !layout.is_empty() {
            spans.push(Span::styled(
                format!(" · {layout}"),
                Style::default().fg(t::muted()),
            ));
        }
        if split {
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let pad = width.saturating_sub(used + route_w - 3 + 1);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(
                p.writer.label(),
                Style::default().fg(t::accent()),
            ));
        }
        lines.push(Line::from(spans));
    }
    if shown < plans.len() {
        lines.push(Line::from(Span::styled(
            format!("  … {} more", plans.len() - shown),
            Style::default().fg(t::muted()),
        )));
    }
    lines.push(Line::from(""));
    for (writer, why, n) in &routes {
        let count = if split || plans.len() > 1 {
            format!(" · {n} file{}", plural(*n))
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {writer}"), Style::default().fg(t::accent())),
            Span::styled(format!("{count}  "), Style::default().fg(t::muted())),
            Span::styled(*why, Style::default().fg(t::muted())),
        ]));
    }

    // A rename that waited for this write is part of it, and the plan is
    // where a write says everything it is about to do.
    let renames: Vec<String> = plans
        .iter()
        .filter(|p| {
            app.files
                .iter()
                .position(|f| f.path == p.path)
                .is_some_and(|i| app.rename_after.contains(&i))
        })
        .map(|p| file_label(&p.path))
        .collect();
    if !renames.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  then renamed from their tags: ",
                Style::default().fg(t::staged()),
            ),
            Span::styled(
                t::fit(
                    &renames.join(", "),
                    (area.width as usize).saturating_sub(36).max(10),
                ),
                Style::default().fg(t::header_fg()),
            ),
        ]));
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
        Span::styled(
            "  ⏎ ",
            Style::default()
                .bg(t::rule())
                .fg(t::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" write   ", Style::default().fg(t::value())),
        Span::styled(
            " esc ",
            Style::default()
                .bg(t::rule())
                .fg(t::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cancel", Style::default().fg(t::value())),
    ]));

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t::accent())),
        ),
        area,
    );
}

/// A file's changed fields as the write dialog lists them: the names in the
/// staged colour, a cleared field in the warning one and a refused field in
/// the error one, and as many as fit before a `+N` counts the rest. The names
/// are the point -- "+4" alone says a file changes without saying how.
fn field_chips(edits: &[FileEdit], width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (i, e) in edits.iter().enumerate() {
        let sep = if i == 0 { "" } else { ", " };
        let rest = edits.len() - i - 1;
        // Leave room for the "+N" that would have to follow this one.
        let tail = if rest > 0 {
            format!(" +{rest}").width()
        } else {
            0
        };
        let w = sep.width() + e.label.width();
        if used + w + tail > width {
            spans.push(Span::styled(
                format!(" +{}", edits.len() - i),
                Style::default().fg(t::mixed()),
            ));
            break;
        }
        used += w;
        let fg = if e.refused {
            t::error()
        } else if e.removed {
            t::warn()
        } else {
            t::staged()
        };
        spans.push(Span::styled(sep, Style::default().fg(t::muted())));
        spans.push(Span::styled(e.label.clone(), Style::default().fg(fg)));
    }
    spans
}

/// The bar, drawn by hand rather than with `Gauge` so the filled and unfilled
/// halves take their colours from the theme like everything else.
fn bar(width: usize, frac: f64) -> Line<'static> {
    let filled = ((width as f64) * frac.clamp(0.0, 1.0)).round() as usize;
    Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(t::accent())),
        Span::styled(
            "░".repeat(width.saturating_sub(filled)),
            Style::default().fg(t::rule()),
        ),
    ])
}

/// What actually happened, per file. A one-line status is fine for one file and
/// useless for forty: a batch needs to say which ones failed and why, without
/// the successes scrolling them away.
fn draw_results(f: &mut Frame, area: Rect, r: &WriteResults) {
    let total = r.ok.len() + r.failed.len();
    let ok = r.failed.is_empty();
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" {} {} of {} ", r.verb, r.ok.len(), total),
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
    // Written but not renamed is its own kind of outcome: the tags are on
    // the file, so it is not a failure, but the name the user asked for is
    // not there either, and the reason is the same paragraph a refusal is.
    for (p, why) in &r.not_renamed {
        lines.push(Line::from(vec![
            Span::styled("  ↷ ", Style::default().fg(t::warn())),
            Span::styled(
                file_label(p),
                Style::default().fg(t::warn()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  written, not renamed", Style::default().fg(t::muted())),
        ]));
        for l in wrap(why, text_width) {
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
    if n == 1 {
        ""
    } else {
        "s"
    }
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
            let candidate = if line.is_empty() {
                indent.chars().count() + word.chars().count()
            } else {
                line.chars().count() + 1 + word.chars().count()
            };
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
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    fn category_and_variant_are_drawn_first_and_the_rule_follows_them() {
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
        let line =
            |y: u16| -> String { (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // y=0 is the block's top border; the form starts at y=1.
        assert!(line(1).contains("Category"), "{:?}", line(1));
        assert!(line(2).contains("Variant"), "{:?}", line(2));
        assert!(
            line(3).trim_end().chars().all(|c| c == '\u{2500}'),
            "{:?}",
            line(3)
        );
        assert!(line(4).contains("Title"), "{:?}", line(4));
    }

    /// Chips take the row's colour, every one of them: the colour is the
    /// value's standing, not the tag's identity. A `#` goes with its tag; the
    /// `·` between names stays subdued so the list still counts at a glance.
    #[test]
    fn every_chip_takes_the_rows_colour() {
        let items: Vec<String> = vec!["pov".into(), "solo".into()];
        let fg_of = |v: &[Span<'static>], text: &str| -> Vec<_> {
            v.iter()
                .filter(|s| s.content.trim() == text)
                .map(|s| s.style.fg)
                .collect()
        };
        for colour in [t::value(), t::staged(), t::mixed()] {
            let spans = tag_spans(&items, true, 40, t::input_bg(), colour);
            assert_eq!(fg_of(&spans, "pov"), vec![Some(colour)]);
            assert_eq!(fg_of(&spans, "solo"), vec![Some(colour)]);
            assert_eq!(
                fg_of(&spans, "#"),
                vec![Some(colour); 2],
                "the # is the tag's"
            );
        }
        let names = tag_spans(&items, false, 40, t::input_bg(), t::staged());
        assert_eq!(fg_of(&names, "solo"), vec![Some(t::staged())]);
        assert_eq!(
            fg_of(&names, "·"),
            vec![Some(t::muted())],
            "the separator is not a name"
        );
    }

    /// A tag the write cannot store is red on any row, and underlined, so it
    /// is still the one that stands out on a row already red.
    #[test]
    fn a_hostile_tag_is_red_on_any_row() {
        let items: Vec<String> = vec!["pov".into(), "a/b".into()];
        let spans = tag_spans(&items, true, 40, t::input_bg(), t::value());
        let bad = spans.iter().find(|s| s.content == "a/b").unwrap();
        assert_eq!(bad.style.fg, Some(t::error()));
        assert!(bad.style.add_modifier.contains(Modifier::UNDERLINED));
        let good = spans.iter().find(|s| s.content == "pov").unwrap();
        assert_eq!(good.style.fg, Some(t::value()));
    }

    /// The form end to end: a list on the file draws in the value colour, the
    /// same list staged draws staged, and a staged list the write will refuse
    /// draws red.
    #[test]
    fn a_rows_colour_says_where_its_value_stands() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        let f = FileTags {
            path: std::path::PathBuf::from("/tmp/tagform-render-colours.mp4"),
            atoms: [("actors".to_string(), Value::text("Ann"))]
                .into_iter()
                .collect(),
            xmp: BTreeMap::new(),
        };
        let mut app = crate::ui::app::App::new(vec![f], BTreeMap::new(), false);
        let colour_of = |app: &crate::ui::app::App, word: &str| {
            let mut term = Terminal::new(TestBackend::new(80, 30)).unwrap();
            term.draw(|fr| draw_fields(fr, fr.area(), app)).unwrap();
            let buf = term.backend().buffer().clone();
            for y in 0..30 {
                let line: String = (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect();
                if let Some(at) = line.find(word) {
                    let x = line[..at].chars().count() as u16;
                    return buf[(x, y)].fg;
                }
            }
            panic!("{word} not drawn");
        };
        assert_eq!(colour_of(&app, "Ann"), t::value(), "on the file");
        app.set_staged(0, "actors", Value::List(vec!["Bo".into()]));
        assert_eq!(colour_of(&app, "Bo"), t::staged(), "about to be written");
        app.set_staged(0, "tags", Value::List(vec!["pov".into(), ".bad".into()]));
        assert_eq!(colour_of(&app, "pov"), t::error(), "refused by the write");
    }

    /// A chip is never cut mid-word: half a tag is a different tag. The
    /// overflow is a count instead, and the box still fills its exact width.
    #[test]
    fn an_overlong_tag_set_counts_the_rest_instead_of_truncating_one() {
        let items: Vec<String> = ["pov", "solo", "outdoor", "handheld"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let spans = tag_spans(&items, true, 16, t::input_bg(), t::muted());
        let drawn: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(drawn.width(), 16, "{drawn:?}");
        assert!(drawn.contains("+2"), "{drawn:?}");
        assert!(
            !drawn.contains("outdo "),
            "a tag was cut mid-word: {drawn:?}"
        );
    }

    #[test]
    fn a_list_that_fits_is_padded_to_the_box_and_not_counted() {
        let items: Vec<String> = vec!["pov".into()];
        let spans = tag_spans(&items, true, 20, t::input_bg(), t::muted());
        let drawn: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(drawn.width(), 20);
        assert!(!drawn.contains('+'), "{drawn:?}");
    }

    fn two_files(a: &[(&str, &str)], b: &[(&str, &str)]) -> crate::ui::app::App {
        use crate::tags::probe::FileTags;
        use std::collections::BTreeMap;
        let mk = |name: &str, kv: &[(&str, &str)]| FileTags {
            path: std::path::PathBuf::from(format!("/tmp/tagform-render-{name}.mp4")),
            atoms: kv
                .iter()
                .map(|(k, v)| (k.to_string(), Value::text(*v)))
                .collect(),
            xmp: BTreeMap::new(),
        };
        crate::ui::app::App::new(vec![mk("a", a), mk("b", b)], BTreeMap::new(), false)
    }

    fn screen(app: &crate::ui::app::App, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| draw_fields(fr, fr.area(), app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    /// Bulk view says what it is and how far an edit reaches: the heading on
    /// the rule, the count beside an agreed value, and "multiple values"
    /// where the files disagree. Opening the field takes the note away so the
    /// typing has the box.
    #[test]
    fn bulk_view_labels_the_rule_and_every_value_with_the_count() {
        let mut app = two_files(
            &[("title", "Same"), ("channel", "One")],
            &[("title", "Same"), ("channel", "Two")],
        );
        let lines = screen(&app, 80, 20);
        let rule = &lines[3];
        assert!(
            rule.contains(&iconed(BULK_ICON, "bulk edit mode - 2 files")),
            "{rule:?}"
        );
        assert!(
            rule.starts_with('\u{2500}') && rule.trim_end().ends_with('\u{2500}'),
            "{rule:?}"
        );
        let title = lines.iter().find(|l| l.contains("Title")).unwrap();
        assert!(
            title.contains("Same") && title.contains("2 files"),
            "{title:?}"
        );
        let channel = lines.iter().find(|l| l.contains("Channel")).unwrap();
        assert!(channel.contains("multiple values (2 files)"), "{channel:?}");

        // Open Title: the count goes, the value stays.
        app.focus = app.rows.iter().position(|r| r.key == "title").unwrap();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let lines = screen(&app, 80, 20);
        let title = lines.iter().find(|l| l.contains("Title")).unwrap();
        assert!(
            title.contains("Same") && !title.contains("2 files"),
            "{title:?}"
        );

        // A single file, or one file of the selection, is not bulk.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        let lines = screen(&app, 80, 20);
        assert!(!lines.iter().any(|l| l.contains("files")), "{lines:?}");
    }

    fn n_files(n: usize) -> crate::ui::app::App {
        use crate::tags::probe::FileTags;
        use std::collections::BTreeMap;
        let files = (0..n)
            .map(|i| FileTags {
                path: std::path::PathBuf::from(format!("/nonexistent/clip-{i}.mp4")),
                atoms: BTreeMap::new(),
                xmp: BTreeMap::new(),
            })
            .collect();
        crate::ui::app::App::new(files, BTreeMap::new(), false)
    }

    /// `]`: step to the next file, as the user would.
    fn next(app: &mut crate::ui::app::App) {
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
    }

    fn header(app: &crate::ui::app::App, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| draw_header(fr, fr.area(), app, None))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    /// A file with a rename that would land on another file is red in the
    /// selection list, and its own page says what is wrong, in red.
    #[test]
    fn a_file_in_trouble_is_red_in_the_list_and_says_why_on_its_page() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = n_files(3);
        app.conflicts
            .insert(1, std::path::PathBuf::from("/nonexistent/taken.mp4"));
        let mut term = Terminal::new(TestBackend::new(80, 6)).unwrap();
        term.draw(|fr| draw_header(fr, fr.area(), &app, None))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let name_fg = |y: u16| {
            let line: String = (0..80).map(|x| buf[(x, y)].symbol().to_string()).collect();
            let at = line.find("clip-").unwrap();
            buf[(line[..at].chars().count() as u16, y)].fg
        };
        assert_eq!(name_fg(0), t::header_fg());
        assert_eq!(name_fg(1), t::error(), "the file in trouble");
        assert_eq!(name_fg(2), t::header_fg());

        next(&mut app);
        let lines = header(&app, 80, 6);
        assert!(
            !lines.iter().any(|l| l.contains("rename blocked")),
            "file 0 is fine: {lines:?}"
        );
        next(&mut app);
        let lines = header(&app, 80, 6);
        let alert = lines
            .iter()
            .find(|l| l.contains("rename blocked"))
            .expect("its page says so");
        assert!(
            alert.contains(&iconed(
                ALERT_ICON,
                "rename blocked: another file is already named taken.mp4"
            )),
            "{alert:?}"
        );
    }

    /// Past the fifth name the list stops, and a file in trouble among the
    /// ones it did not show is counted rather than lost.
    #[test]
    fn a_file_in_trouble_past_the_list_is_counted() {
        let mut app = n_files(8);
        app.conflicts
            .insert(6, std::path::PathBuf::from("/nonexistent/taken.mp4"));
        let lines = header(&app, 80, 6);
        assert!(lines[5].contains("3 more · 1 with problems"), "{lines:?}");
    }

    /// Bulk view's header lists the selection instead of one file's picture,
    /// five names and then the count. Five files fit whole; a sixth is where
    /// the list stops and says how many.
    #[test]
    fn bulk_header_lists_the_files_and_elides_at_the_sixth() {
        let app = n_files(7);
        let lines = header(&app, 60, 6);
        for i in 0..5 {
            assert!(
                lines[i].contains(&iconed(FILE_ICON, &format!("clip-{i}.mp4"))),
                "{lines:?}"
            );
        }
        assert!(lines[5].contains("… 2 more"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("clip-5")), "{lines:?}");

        let app = n_files(5);
        let lines = header(&app, 60, 6);
        assert!(
            lines[4].contains("clip-4.mp4") && !lines[5].contains("more"),
            "{lines:?}"
        );

        // One file in view is that file's header, not a list.
        let mut app = n_files(7);
        next(&mut app);
        let lines = header(&app, 60, 6);
        assert!(
            lines[0].contains("clip-0.mp4") && !lines[1].contains("clip-1"),
            "{lines:?}"
        );
    }

    /// In single-file view the rule says where the file stands in the write
    /// queue -- how many go before it -- and nothing when it is not queued.
    #[test]
    fn the_rule_says_where_a_queued_file_stands() {
        let mut app = n_files(3);
        for i in 0..3 {
            app.set_staged(i, "title", Value::text("t"));
            app.enqueue_for_test(i, false);
        }
        for _ in 0..3 {
            next(&mut app); // file 2
        }
        let lines = screen(&app, 80, 20);
        let rule = &lines[3];
        assert!(
            rule.contains(&iconed(QUEUE_ICON, "queued for write - 2 files left")),
            "{rule:?}"
        );

        let mut app = n_files(2);
        next(&mut app);
        let lines = screen(&app, 80, 20);
        assert!(!lines[3].contains("queued"), "{:?}", lines[3]);
    }

    /// A set the files disagree about lights nothing: highlighting one file's
    /// answer would claim an agreement that is not there.
    #[test]
    fn a_mixed_set_lights_no_option() {
        let app = two_files(&[("category", "Adult")], &[("category", "Meme")]);
        let row = app.rows.iter().find(|r| r.key == "category").unwrap();
        let (_, sel, _) = closed_set(&app, row).unwrap();
        assert_eq!(sel, None);
        let agreed = two_files(&[("category", "Meme")], &[("category", "Meme")]);
        let row = agreed.rows.iter().find(|r| r.key == "category").unwrap();
        assert!(closed_set(&agreed, row).unwrap().1.is_some());
    }

    /// A mixed set says what the files hold: each held option followed by
    /// its count, the count in the lighter mixed colour, and the options no
    /// file holds left without one.
    #[test]
    fn a_mixed_set_counts_each_answer() {
        let app = two_files(&[("variant", "Clip")], &[("variant", "Original")]);
        let row = app.rows.iter().find(|r| r.key == "variant").unwrap();
        let (labels, sel, counts) = closed_set(&app, row).unwrap();
        assert_eq!(sel, None);
        let held = |l: &str| counts[labels.iter().position(|x| x == l).unwrap()];
        assert_eq!(
            (held("Original"), held("Enhanced"), held("Clip")),
            (1, 0, 1)
        );

        let lines = screen(&app, 100, 20);
        let at = lines.iter().position(|l| l.contains("Variant")).unwrap();
        let line = &lines[at];
        assert!(line.contains("Original 1"), "{line:?}");
        assert!(line.contains("Clip 1"), "{line:?}");
        assert!(!line.contains("Enhanced 0"), "{line:?}");

        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|fr| draw_fields(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let col = line.find("Original 1").unwrap() + "Original ".len();
        let col = line[..col].chars().count() as u16;
        assert_eq!(
            buf[(col, at as u16)].style().fg,
            Some(t::mixed()),
            "{line:?}"
        );
    }

    /// An unknown value in a mixed set is counted too, not dropped.
    #[test]
    fn a_mixed_set_counts_a_value_it_does_not_know() {
        let app = two_files(&[("variant", "Bootleg")], &[("variant", "Clip")]);
        let row = app.rows.iter().find(|r| r.key == "variant").unwrap();
        let (labels, _, counts) = closed_set(&app, row).unwrap();
        let i = labels.iter().position(|l| l == "Bootleg").expect("dropped");
        assert_eq!(counts[i], 1);
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
        assert!(
            buf[(at, 1)].style().add_modifier.contains(Modifier::BOLD),
            "{row:?}"
        );
        assert!(!buf[(2 + LABEL_COLS, 1)]
            .style()
            .add_modifier
            .contains(Modifier::BOLD));
        // It is a staged edit, so the chosen cell is drawn in the staged
        // colour the way a staged text value is -- on the label *and* on the
        // value, not the label alone.
        assert_eq!(buf[(at, 1)].style().fg, Some(t::staged()), "{row:?}");
        assert_eq!(buf[(1, 1)].style().fg, Some(t::staged()), "label {row:?}");
    }

    /// The focused row is a band, not a caret: the label side is filled too,
    /// so the row is findable without hunting for a one-column marker. The
    /// rows either side of it keep the terminal's own ground -- a form where
    /// every row is tinted has no cursor at all.
    #[test]
    fn the_focused_row_is_filled_across_its_label_as_well() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        let f = FileTags {
            path: std::path::PathBuf::from("/tmp/tagform-band-test.mp4"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let mut app = crate::ui::app::App::new(vec![f], BTreeMap::new(), false);
        app.focus = 1;
        let w = 80;
        let mut term = Terminal::new(TestBackend::new(w, 12)).unwrap();
        term.draw(|fr| draw_fields(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();

        // y = 1 is the first field row, so the focused one is the next line
        // down. Every column of it, marker and label included, carries the
        // focus ground -- including the last, which the value box stops short
        // of.
        let band = t::input_bg_focus();
        for x in 0..w {
            assert_eq!(
                buf[(x, 2)].style().bg,
                Some(band),
                "column {x} of the focused row"
            );
        }
        // The row above it is untinted where the label is: the terminal's own
        // background, which is what keeps a translucent terminal translucent.
        for x in 0..LABEL_COLS {
            assert_ne!(
                buf[(x, 1)].style().bg,
                Some(band),
                "column {x} bled onto row 1"
            );
        }
    }

    /// A set whose selection came from disk keeps the focus accent, so the
    /// staged colour means "will be written" and nothing else.
    #[test]
    fn an_unstaged_enum_selection_is_not_drawn_staged() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        // The set's members live on the app, so one is built empty to read
        // them, and the real one with the choice already on disk.
        let app = crate::ui::app::App::new(
            vec![FileTags {
                path: std::path::PathBuf::from("/tmp/tagform-render-test.mp4"),
                atoms: BTreeMap::new(),
                xmp: BTreeMap::new(),
            }],
            BTreeMap::new(),
            false,
        );
        let chosen = app.enums.category[1].clone();
        let mut atoms = BTreeMap::new();
        atoms.insert("category".to_string(), Value::Text(chosen.clone()));
        let app = crate::ui::app::App::new(
            vec![FileTags {
                path: std::path::PathBuf::from("/tmp/tagform-render-test.mp4"),
                atoms,
                xmp: BTreeMap::new(),
            }],
            BTreeMap::new(),
            false,
        );

        let w = 140;
        let mut term = Terminal::new(TestBackend::new(w, 12)).unwrap();
        term.draw(|fr| draw_fields(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let row: String = (0..w).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        let at = row.find(&chosen).unwrap() as u16;
        assert_ne!(buf[(at, 1)].style().fg, Some(t::staged()), "{row:?}");
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
        // Wide enough for the whole vocabulary including the help key, which
        // leads the strip and so is never the hint that gets dropped.
        // Faststart shares the mode bar, so the vocabulary needs its width too.
        let w = 280;
        let mut term = Terminal::new(TestBackend::new(w, 2)).unwrap();
        term.draw(|fr| {
            let a = fr.area();
            draw_badge_bar(fr, Rect { height: 1, ..a }, &app, false);
            draw_mode_bar(
                fr,
                Rect {
                    y: 1,
                    height: 1,
                    ..a
                },
                &app,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let (strip, mode) = (row(0), row(1));

        // Two spaces of padding plus the one the key carries, so the gap
        // still reads as one column once the terminal draws the glyph wide.
        assert!(mode.contains(" ⌫   clear"), "{mode:?}");
        assert!(
            !mode.contains("…"),
            "the whole list should fit at {w} cols: {mode:?}"
        );
        // The keys follow the mode, and help is not among them.
        assert!(mode.starts_with(" NORMAL   hjkl  move "), "{mode:?}");
        assert!(!mode.contains("help"), "{mode:?}");
        for key in ["o", "b", "f ~", "F", "t"] {
            assert!(
                mode.contains(&format!(" {key}  ")),
                "{key} crowded: {mode:?}"
            );
        }
        assert!(mode.trim_end().ends_with("faststart on"), "{mode:?}");
        // Help sits alone at the right of the badge bar.
        assert!(strip.trim_end().ends_with("?  help"), "{strip:?}");
        assert!(!strip.contains("hjkl"), "{strip:?}");
        assert!(!strip.contains("faststart"), "{strip:?}");
    }

    /// The import band names both sources and previews the filename's
    /// answer, marking what the file already holds -- at a width a laptop
    /// terminal actually has.
    #[test]
    fn the_import_band_previews_the_filename() {
        use crate::tags::probe::FileTags;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::collections::BTreeMap;

        let f = FileTags {
            path: std::path::PathBuf::from("/x/Ann (Ch) - A Title #pov ★★★☆☆.mp4"),
            atoms: [("title".to_string(), Value::text("Kept"))]
                .into_iter()
                .collect(),
            xmp: BTreeMap::new(),
        };
        let mut app = crate::ui::app::App::new(vec![f], BTreeMap::new(), false);
        app.import_menu = true;
        // Where the menu opens on a file with no URL of its own.
        app.import_pick = ImportSource::Filename;
        let (w, h) = (100, 6);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| draw_import(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: Vec<String> = (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect();
        assert!(text[1].contains(" u  url"), "{:?}", text[1]);
        assert!(text[1].contains("no URL on this file"), "{:?}", text[1]);
        assert!(text[2].contains(" f  filename"), "{:?}", text[2]);
        assert!(text[3].contains("Actors Ann"), "{:?}", text[3]);
        assert!(text[3].contains("Rating ★★★☆☆"), "{:?}", text[3]);
        assert!(text[3].contains("keeps Title"), "{:?}", text[3]);
        assert!(text[4].contains(" l  location"), "{:?}", text[4]);
        assert!(text[4].contains("type a place to look up"), "{:?}", text[4]);
        assert!(text[5].contains("esc"), "{:?}", text[5]);
        assert!(
            text[5].contains("j/k"),
            "the band must say how to choose: {:?}",
            text[5]
        );

        // The caret sits on the source the cursor is on, and only on that one.
        assert!(text[2].starts_with("▸"), "{:?}", text[2]);
        assert!(!text[1].starts_with("▸"), "{:?}", text[1]);
        app.import_pick = ImportSource::Url;
        term.draw(|fr| draw_import(fr, fr.area(), &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let moved: Vec<String> = (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect();
        assert!(moved[1].starts_with("▸"), "{:?}", moved[1]);
        assert!(!moved[2].starts_with("▸"), "{:?}", moved[2]);
        // Moving the caret must not shift the lines it moves between.
        let tail = |s: &str| s.chars().skip(1).collect::<String>();
        assert_eq!(tail(&moved[2]), tail(&text[2]));
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
            verb: "Wrote",
            ok: vec![],
            failed: vec![(
                std::path::PathBuf::from("/x/IMG_4855.MOV"),
                "the remux did not reproduce this file's tracks\nlost:    data/mebx x3".into(),
            )],
            ..Default::default()
        };
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|fr| draw_results(fr, fr.area(), &r)).unwrap();
        let buf = term.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let rows: Vec<String> = (0..12).map(row).collect();
        let at = rows
            .iter()
            .position(|l| l.contains("IMG_4855.MOV"))
            .unwrap();
        // The name owns its line; the reason follows, indented, unwrapped.
        assert!(!rows[at].contains("remux"), "{:?}", rows[at]);
        assert!(
            rows[at + 1].contains("the remux did not reproduce"),
            "{:?}",
            rows[at + 1]
        );
        assert!(
            rows[at + 2].contains("lost:    data/mebx x3"),
            "{:?}",
            rows[at + 2]
        );
    }
}

#[cfg(test)]
mod progress_panel_tests {
    use super::*;
    use crate::model::value::Value;
    use crate::tags::probe::FileTags;
    use crate::ui::app::WriteProgress;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeMap;

    fn app(n: usize) -> App {
        let files = (0..n)
            .map(|i| FileTags {
                path: std::path::PathBuf::from(format!("/tmp/clip-{i:03}.mov")),
                atoms: BTreeMap::new(),
                xmp: BTreeMap::new(),
            })
            .collect();
        App::new(files, BTreeMap::new(), false)
    }

    /// Every file the same size, so the byte-weighted bar reads exactly like
    /// the old per-file count would have.
    fn progress(file: usize, total: usize, label: &'static str, frac: f64) -> WriteProgress {
        WriteProgress {
            file,
            total,
            label,
            frac,
            file_bytes: 1,
            done_bytes: file as u64,
            total_bytes: total as u64,
        }
    }

    /// The whole screen, as rows of text.
    fn paint(a: &App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| draw(fr, a, None)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    /// Where the status went: the last row, under the mode bar -- not the
    /// badge bar it used to share with the title.
    #[test]
    fn the_global_bar_reads_bottom_right_and_not_in_the_badge_bar() {
        let mut a = app(3);
        a.fake_queue(Some(0), &[1, 2]);
        a.progress = Some(progress(0, 3, "remuxing", 0.5));
        let rows = paint(&a, 100, 30);
        let last = rows.last().unwrap();
        assert!(last.contains("1/3"), "{last:?}");
        // Said once, by the panel: not "writing", not the stage, not the name.
        assert!(
            !last.contains("writing") && !last.contains("remuxing"),
            "{last:?}"
        );
        assert!(!last.contains("clip-000"), "{last:?}");
        // Right-aligned: the left third of the line is untouched, and the
        // percentage is the last thing on the screen.
        assert!(last[..40].trim().is_empty(), "not right-aligned: {last:?}");
        // The batch, not this file: half of one file of three is 17% -- and
        // it is inside the bar, which runs on past it to the edge.
        let at = last.find("17%").expect("the bar is not the whole batch");
        assert!(
            last[at + 3..].trim_end().is_empty() && last[at + 3..].len() > 4,
            "{last:?}"
        );
        assert!(
            !rows[0].contains("writing"),
            "still in the badge bar: {:?}",
            rows[0]
        );
    }

    /// One file in the panel, and it is the one under the writer: its own
    /// bar goes under its facts, wider than the one on the status line.
    #[test]
    fn the_focused_file_gets_its_own_bar_under_its_facts() {
        let mut a = app(1);
        a.fake_queue(Some(0), &[]);
        a.progress = Some(progress(0, 1, "remuxing", 0.5));
        let rows = paint(&a, 100, 30);
        let band = rows[2..8].join("\n");
        assert!(band.contains("clip-000.mov"), "{band}");
        assert!(
            band.contains("remuxing"),
            "the stage is not in the panel: {band}"
        );
        let wide = band.matches('\u{2588}').count();
        let narrow = rows.last().unwrap().matches('\u{2588}').count();
        assert!(
            wide > narrow,
            "panel bar {wide} is not bigger than the status bar {narrow}"
        );
    }

    /// A file still in the queue says where it stands rather than pretending
    /// to be running: an empty bar, and how many go first.
    #[test]
    fn a_file_waiting_its_turn_shows_a_waiting_bar() {
        let mut a = app(3);
        a.view = Some(2);
        a.fake_queue(Some(0), &[1, 2]);
        a.progress = Some(progress(0, 3, "remuxing", 0.5));
        let band = paint(&a, 100, 30)[2..8].join("\n");
        assert!(band.contains("2 ahead"), "{band}");
        assert!(
            !band.contains('\u{2588}'),
            "a waiting file must not show a filled bar: {band}"
        );
        assert!(band.contains('\u{2591}'), "no empty bar: {band}");
    }

    /// Bulk view: the queue itself, in the order it will be taken, with the
    /// live bar on the file under the writer and empty ones behind it.
    #[test]
    fn bulk_view_lists_the_queue_with_the_running_file_on_top() {
        let mut a = app(9);
        a.fake_queue(Some(3), &[4, 5, 6, 7, 8]);
        a.progress = Some(progress(0, 6, "verifying", 0.75));
        let rows = paint(&a, 120, 30);
        let band = rows[2..8].join("\n");
        assert!(
            band.contains("clip-003.mov"),
            "the busy file is not on top: {band}"
        );
        assert!(band.contains("verifying"), "{band}");
        assert!(
            band.contains("waiting"),
            "the files behind it are not listed: {band}"
        );
        // One live bar, on the busy row alone.
        let filled: Vec<usize> = rows[2..8]
            .iter()
            .filter(|r| r.contains('\u{2588}'))
            .map(|r| r.len())
            .collect();
        assert_eq!(filled.len(), 1, "more than one row is running: {band}");
    }

    /// Six rows of queue and then the count: a panel that stops at six
    /// without saying so is a lie about how much is left.
    #[test]
    fn a_queue_longer_than_the_band_says_how_many_it_did_not_list() {
        let mut a = app(40);
        let waiting: Vec<usize> = (1..40).collect();
        a.fake_queue(Some(0), &waiting);
        a.progress = Some(progress(0, 40, "remuxing", 0.1));
        let band = paint(&a, 120, 30)[2..9].join("\n");
        assert!(band.contains("more waiting"), "{band}");
        assert!(
            band.contains("35 more") || band.contains("34 more"),
            "{band}"
        );
    }

    /// `w` over a draining queue raises the confirmation for the next batch
    /// while the writer works: the dialog takes the screen, but not the row
    /// the running write reports on.
    #[test]
    fn a_dialog_does_not_cover_the_running_bar() {
        let mut a = app(3);
        a.fake_queue(Some(0), &[1]);
        a.progress = Some(progress(0, 3, "remuxing", 0.5));
        a.results = Some(WriteResults {
            verb: "Wrote",
            ok: vec![a.files[0].path.clone()],
            failed: Vec::new(),
            not_renamed: Vec::new(),
        });
        let rows = paint(&a, 100, 30);
        assert!(
            rows.iter().any(|r| r.contains("Wrote 1 of 1")),
            "no dialog: {rows:?}"
        );
        let last = rows.last().unwrap();
        assert!(
            last.contains("1/3") && last.contains('%'),
            "the bar went under the dialog: {last:?}"
        );
    }

    /// A long status message is cut rather than run under the bar: two
    /// messages sharing a row is worse than one short one.
    #[test]
    fn a_long_status_message_yields_to_the_bar() {
        let mut a = app(2);
        a.fake_queue(Some(0), &[1]);
        a.progress = Some(progress(0, 2, "remuxing", 0.5));
        a.status = "x".repeat(200);
        let rows = paint(&a, 100, 30);
        let last = rows.last().unwrap();
        assert!(
            last.contains("1/2") && last.contains("25%"),
            "the message overran the bar: {last:?}"
        );
        assert!(last.contains('…'), "the message was not cut: {last:?}");
    }

    /// The view line sits over the band, not beside the logo; the band keeps
    /// its six rows under it, and the badge bar carries neither the count nor
    /// the custom-key tally.
    #[test]
    fn the_view_line_sits_over_the_band() {
        let mut a = app(6);
        let rows = paint(&a, 100, 30);
        assert!(
            !rows[0].contains("6 files") && !rows[0].contains("custom"),
            "{:?}",
            rows[0]
        );
        assert!(rows[2].trim_start().starts_with("6 files"), "{:?}", rows[2]);
        assert!(
            rows[3].contains("clip-000.mov"),
            "the list is not under it: {:?}",
            rows[3]
        );
        a.view = Some(0);
        let rows = paint(&a, 100, 30);
        assert!(
            rows[2].trim_start().starts_with("file 1 of 6"),
            "{:?}",
            rows[2]
        );
        assert!(rows[3].contains("clip-000.mov"), "{:?}", rows[3]);
        // Too short for the band: the count goes back to the badge bar
        // rather than vanishing.
        let rows = paint(&a, 100, 16);
        assert!(rows[0].contains("file 1 of 6"), "{:?}", rows[0]);
    }

    fn confirm(n: usize, fields: &[(&str, &str)], h: u16) -> Vec<String> {
        let files = (0..n)
            .map(|i| FileTags {
                path: std::path::PathBuf::from(format!(
                    "/nonexistent/Anna Cherry, Milalzt - A Very Long Title That Will Not Fit {i:02}.mp4"
                )),
                atoms: BTreeMap::new(),
                xmp: BTreeMap::new(),
            })
            .collect();
        let mut a = App::new(files, BTreeMap::new(), false);
        for i in 0..n {
            for (k, v) in fields {
                a.set_staged(i, k, Value::text(*v));
            }
        }
        a.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char('w'),
        ));
        assert!(
            a.pending.is_some(),
            "w did not raise the dialog: {}",
            a.status
        );
        paint(&a, 100, h)
    }

    /// One line per file: the name cut short, then the fields it changes --
    /// and the route said once for the batch, not under every name.
    #[test]
    fn the_write_dialog_lists_each_file_once_with_its_fields() {
        let rows = confirm(
            3,
            &[
                ("title", "T"),
                ("channel", "C"),
                ("url", "https://x.test/v"),
            ],
            40,
        );
        let named: Vec<&String> = rows.iter().filter(|r| r.contains("A Very Long")).collect();
        assert_eq!(named.len(), 3, "{rows:#?}");
        for r in &named {
            assert!(r.contains('…'), "the name was not elided: {r:?}");
            assert!(
                r.contains("Title") && r.contains("Channel") && r.contains("URL"),
                "{r:?}"
            );
        }
        let routes = rows.iter().filter(|r| r.contains("· 3 files")).count();
        assert_eq!(routes, 1, "the route should be said once: {rows:#?}");
        assert!(!rows.iter().any(|r| r.contains("FastStart")), "{rows:#?}");
    }

    /// More fields than the line holds: the names that fit, then a count.
    #[test]
    fn a_long_field_list_ends_in_a_count() {
        let edits: Vec<FileEdit> = [
            "Actors",
            "Category",
            "Channel",
            "Orientation",
            "Tags",
            "Title",
        ]
        .iter()
        .map(|l| FileEdit {
            label: l.to_string(),
            removed: false,
            refused: false,
        })
        .collect();
        let text: String = field_chips(&edits, 26)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.starts_with("Actors, Category"), "{text:?}");
        assert!(text.ends_with(" +4") || text.ends_with(" +3"), "{text:?}");
        assert!(text.width() <= 26, "{text:?}");
    }

    /// A batch too long for the dialog lists what fits and counts the rest,
    /// so the keys at the bottom are never pushed off it.
    #[test]
    fn a_long_batch_is_counted_rather_than_cut_off() {
        let rows = confirm(40, &[("title", "T")], 30);
        assert!(rows.iter().any(|r| r.contains("more")), "{rows:#?}");
        assert!(
            rows.iter()
                .any(|r| r.contains("esc") && r.contains("cancel")),
            "{rows:#?}"
        );
    }

    /// The percentage sits inside the bar, and each of its characters takes
    /// the colour of the half it lands on.
    #[test]
    fn the_percentage_is_drawn_inside_the_bar() {
        let spans = labelled_bar(10, 0.5, "50%");
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "   50%    ");
        assert_eq!(
            spans[3].style.bg,
            Some(t::accent()),
            "5 is on the filled half"
        );
        assert_eq!(spans[5].style.bg, Some(t::rule()), "% is on the empty half");
        assert_eq!(spans[3].style.fg, Some(t::badge_fg()));
        assert_eq!(spans[5].style.fg, Some(t::value()));
    }

    /// The view line says where the edits stand, in files, and only when
    /// there is something to say; the badge bar no longer says it at all.
    #[test]
    fn the_view_line_counts_files_by_state() {
        let mut a = app(9);
        let line = |a: &App| paint(a, 120, 30)[2].trim().to_string();
        assert_eq!(line(&a), "9 files");
        for i in 0..6 {
            a.set_staged(i, "title", Value::text("T"));
            a.set_staged(i, "channel", Value::text("C"));
        }
        assert_eq!(line(&a), "9 files · 6 staged", "files, not fields");
        a.fake_queue(Some(0), &[1, 2]);
        assert_eq!(line(&a), "9 files · 1 writing · 2 queued · 3 staged");
        assert!(
            !paint(&a, 120, 30)[0].contains("staged"),
            "still in the badge bar"
        );

        a.view = Some(0);
        assert_eq!(line(&a), "file 1 of 9 · writing");
        a.view = Some(2);
        assert_eq!(line(&a), "file 3 of 9 · queued, 2 ahead");
        a.view = Some(4);
        assert_eq!(line(&a), "file 5 of 9 · staged changes");
        a.view = Some(8);
        assert_eq!(line(&a), "file 9 of 9");
    }

    /// Nothing running: the panel is the selection again, and the status
    /// line is the status line.
    #[test]
    fn an_idle_screen_carries_no_bar_at_all() {
        let a = app(4);
        let rows = paint(&a, 100, 30);
        let all = rows.join("\n");
        assert!(!all.contains("writing"), "{all}");
        assert!(
            !all.contains('\u{2588}'),
            "a bar with nothing to report: {all}"
        );
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use crate::tags::probe::FileTags;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeMap;

    fn app() -> App {
        App::new(
            vec![FileTags {
                path: std::path::PathBuf::from("/tmp/tagform-help.mp4"),
                atoms: BTreeMap::new(),
                xmp: BTreeMap::new(),
            }],
            BTreeMap::new(),
            false,
        )
    }

    fn paint(w: u16, h: u16, scroll: u16) -> Vec<String> {
        let mut a = app();
        a.help = true;
        a.help_scroll = scroll;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|fr| draw_help(fr, fr.area(), &a)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    /// Every binding in the table has to reach the screen, or the screen is
    /// not the key map -- it is a subset that looks like one.
    #[test]
    fn every_binding_is_painted_somewhere() {
        let wide = paint(160, 40, 0).join("\n");
        for k in KEYMAP.iter().flat_map(|s| s.binds.iter()) {
            assert!(
                wide.contains(k.keys),
                "{:?} missing from the map: {wide}",
                k.keys
            );
        }
        for s in KEYMAP {
            assert!(wide.contains(s.title), "{:?} heading missing", s.title);
        }
    }

    /// A terminal too short for the map must still be able to reach the end of
    /// it, and must say so -- an overlay that silently cuts off at `f` looks
    /// like a map with no undo in it.
    #[test]
    fn a_short_terminal_scrolls_and_says_so() {
        let top = paint(72, 24, 0).join("\n");
        assert!(top.contains("scroll"), "{top}");
        assert!(
            !top.contains("clear the line"),
            "the whole map cannot fit in 24 rows"
        );
        let bottom = paint(72, 24, 200).join("\n");
        assert!(
            bottom.contains("clear the line"),
            "scrolled to the end: {bottom}"
        );
    }

    /// Where the map fits whole there is nothing to scroll, and offering the
    /// keys anyway is a lie about what the screen does.
    #[test]
    fn a_tall_terminal_offers_no_scroll() {
        let all = paint(160, 40, 0).join("\n");
        assert!(all.contains("any key  back to the form"), "{all}");
        assert!(!all.contains("scroll"), "{all}");
    }

    /// `?` opens it, the next key closes it, and nothing leaks into the form
    /// behind it -- `w` over the map must not stage a write.
    #[test]
    fn the_question_mark_opens_and_the_next_key_closes() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut a = app();
        a.on_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(a.help);
        a.on_key(KeyEvent::from(KeyCode::Char('w')));
        assert!(!a.help);
        assert!(
            a.pending.is_none(),
            "w over the map reached the form behind it"
        );
    }
}

//! Entity list screen rendering and extraction worker thread.
//!
//! `Screen::Main` displays the entity list. The file diff was `Screen::Main`;
//! it is now `Screen::FileDiff { file_idx }`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::semantic::{ChangeAnnotation, ChangeType, EntityKind, EntitySummary};
use crate::tui::app::App;
use crate::tui::ReviewSurfaceExt;

// ── Extraction worker events ──────────────────────────────────────────────────

/// Events produced by the extraction worker thread and consumed by the
/// TUI main loop on each render tick.
pub enum ExtractionEvent {
    /// Counter update for the loading overlay.
    Progress {
        files_done: usize,
        files_total: usize,
        files_failed: usize,
    },
    /// One file's extraction finished — carries its entity summaries.
    FileExtracted {
        file_path: String,
        entities: Vec<EntitySummary>,
    },
    /// All files processed without cancellation.
    Complete,
    /// Cancellation flag was set; remaining files unprocessed.
    Cancelled,
    /// Fatal error (e.g., entire commit-list fetch failed).
    Error(String),
}

// ── Extraction state ──────────────────────────────────────────────────────────

/// In-progress extraction: channel receiver + cancellation flag.
pub struct ExtractionInProgress {
    pub rx: std::sync::mpsc::Receiver<ExtractionEvent>,
    pub cancel: Arc<AtomicBool>,
    pub files_done: usize,
    pub files_total: usize,
    pub files_failed: usize,
}

/// Cancel in-progress extraction and discard the receiver.
pub fn cancel_extraction(progress: &ExtractionInProgress) {
    progress.cancel.store(true, Ordering::Relaxed);
}

// ── Entity list rendering ─────────────────────────────────────────────────────

const SIGIL_PAD: usize = 5; // "  Δ  " — 2-char indent + 2-char sigil + space
const NAME_MAX: usize = 28;
const FILE_MAX: usize = 15;
const ANNOT_MAX: usize = 20;

fn entity_sigil(e: &EntitySummary) -> (&'static str, Color) {
    match e.change {
        ChangeType::Added => ("⊕ ", Color::Green),
        ChangeType::Deleted => ("⊖ ", Color::Red),
        ChangeType::Modified => ("Δ ", Color::Yellow),
        ChangeType::Moved => ("≈ ", Color::DarkGray),
    }
}

fn kind_label(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Function => "fn",
        EntityKind::Method => "method",
        EntityKind::Class => "class",
        EntityKind::Struct => "struct",
        EntityKind::Enum => "enum",
        EntityKind::Trait => "trait",
        EntityKind::Interface => "iface",
        EntityKind::Module => "mod",
        EntityKind::Type => "type",
        EntityKind::Constant => "const",
        EntityKind::Table => "table",
        EntityKind::View => "view",
        EntityKind::Index => "index",
        EntityKind::Trigger => "trigger",
        EntityKind::Policy => "policy",
        EntityKind::Schema => "schema",
        EntityKind::Extension => "ext",
        EntityKind::ConfigProperty => "prop",
        EntityKind::AnonymousBlock => "block",
        EntityKind::Section => "§",
        EntityKind::TestSuite => "suite",
        EntityKind::TestCase => "test",
        EntityKind::Other => "",
    }
}

fn annotation_text(e: &EntitySummary) -> String {
    let kind = kind_label(e.kind);
    let change = match e.change {
        ChangeType::Added => "added".to_owned(),
        ChangeType::Deleted => "deleted".to_owned(),
        ChangeType::Moved => {
            let src = e
                .source_file
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            format!("moved from {src}")
        }
        ChangeType::Modified => match e.annotation {
            ChangeAnnotation::SigChanged => "sig changed".to_owned(),
            ChangeAnnotation::BodyOnly => "body".to_owned(),
            ChangeAnnotation::SigAndBody => "sig+body".to_owned(),
            ChangeAnnotation::None => String::new(),
        },
    };
    if kind.is_empty() || change.is_empty() {
        format!("{kind}{change}")
    } else {
        format!("{kind} · {change}")
    }
}

fn truncate_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn is_cosmetic(e: &EntitySummary) -> bool {
    !e.structural_change
}

struct EntityRowCtx<'a> {
    entity: &'a EntitySummary,
    focused: bool,
    cosmetic_hidden: bool,
    comment_indicator: bool,
}

/// Render one entity row at `y` into the given area.
/// Build the text line for one entity row, suitable for Paragraph rendering.
fn entity_row_line(
    entity: &EntitySummary,
    focused: bool,
    comment_indicator: bool,
    width: usize,
) -> TuiLine<'static> {
    let (sigil_str, base_color) = entity_sigil(entity);
    let (fg, dim) = if is_cosmetic(entity) {
        (Color::DarkGray, true)
    } else {
        (base_color, false)
    };
    let style = if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let name = truncate_to(&entity.display_name, NAME_MAX);
    let file = {
        let p = entity.file_path.to_string_lossy();
        let filename = p.rsplit('/').next().unwrap_or_else(|| p.as_ref());
        truncate_to(filename, FILE_MAX)
    };
    let annot_raw = annotation_text(entity);
    let annot = truncate_to(
        &if is_cosmetic(entity) {
            format!("{annot_raw} [cosmetic]")
        } else {
            annot_raw
        },
        ANNOT_MAX,
    );
    let name_col = width.min(SIGIL_PAD + NAME_MAX + 2);
    let file_col = name_col + FILE_MAX + 2;
    let annot_end = file_col + ANNOT_MAX;
    let pad_name = " ".repeat(
        name_col
            .saturating_sub(SIGIL_PAD)
            .saturating_sub(name.chars().count()),
    );
    let pad_file = " ".repeat(
        file_col
            .saturating_sub(name_col + 2)
            .saturating_sub(file.chars().count()),
    );
    let trailing = if annot_end < width {
        " ".repeat(width - annot_end)
    } else {
        String::new()
    };
    let dot = if comment_indicator { " ●" } else { "" };
    TuiLine::from(vec![
        Span::raw("  "),
        Span::styled(
            sigil_str,
            if dim {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(fg)
            },
        ),
        Span::styled(format!("{name}{pad_name}  "), style),
        Span::styled(
            format!("{file}{pad_file}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("{annot}{trailing}{dot}"), style.fg(Color::DarkGray)),
    ])
}

fn render_entity_row(frame: &mut Frame<'_>, area: Rect, ctx: &EntityRowCtx<'_>) {
    let EntityRowCtx {
        entity,
        focused,
        cosmetic_hidden,
        comment_indicator,
    } = ctx;
    let (focused, cosmetic_hidden, comment_indicator) =
        (*focused, *cosmetic_hidden, *comment_indicator);
    if cosmetic_hidden && is_cosmetic(entity) {
        return;
    }
    let line = entity_row_line(entity, focused, comment_indicator, usize::from(area.width));
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            y: area.y,
            height: 1,
            ..area
        },
    );
}

/// Render the description row (≡ sigil, commit subject, comment dot).
pub fn render_description_row(
    frame: &mut Frame<'_>,
    area: Rect,
    subject: &str,
    comment_count: usize,
    focused: bool,
) {
    let style = if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let dot = if comment_count > 0 {
        format!(" ● {comment_count}")
    } else {
        String::new()
    };
    let width = usize::from(area.width);
    let dot_chars = dot.chars().count();
    let subj = truncate_to(subject, width.saturating_sub(SIGIL_PAD + dot_chars));
    let pad = " ".repeat(width.saturating_sub(SIGIL_PAD + subj.chars().count() + dot_chars));
    let line = TuiLine::from(vec![
        Span::raw("  "),
        Span::styled("≡ ", Style::default()),
        Span::styled(format!("{subj}{pad}{dot}"), style),
    ]);
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            y: area.y,
            height: 1,
            ..area
        },
    );
}

/// Render the divider line between description row and entity list.
pub fn render_divider(frame: &mut Frame<'_>, area: Rect) {
    let line = "─".repeat(usize::from(area.width));
    frame.render_widget(
        Paragraph::new(TuiLine::from(Span::styled(
            line,
            Style::default().fg(Color::DarkGray),
        ))),
        Rect {
            y: area.y,
            height: 1,
            ..area
        },
    );
}

/// Render the loading overlay (1s+ case).
pub fn render_loading_overlay(
    frame: &mut Frame<'_>,
    files_done: usize,
    files_total: usize,
    files_failed: usize,
    tick: u64,
) {
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    #[expect(
        clippy::as_conversions,
        reason = "spinner index: u64 mod 10 then to usize is bounded"
    )]
    let spinner = spinner_frames[(tick % 10) as usize];
    let fail_str = if files_failed > 0 {
        format!("  ·  {files_failed} failed")
    } else {
        String::new()
    };
    let text =
        format!(" {spinner} Extracting entities  ·  {files_done} / {files_total} files{fail_str}");
    let hint = "   Esc to cancel";

    let area = frame.area();
    let height = 4u16;
    let width = u16::try_from(text.chars().count() + 4)
        .unwrap_or(60)
        .min(area.width.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay = Rect {
        x,
        y,
        width,
        height,
    };

    let block = Block::bordered();
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);
    frame.render_widget(
        Paragraph::new(vec![
            TuiLine::raw(text),
            TuiLine::raw(""),
            TuiLine::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        ]),
        inner,
    );
}

/// Render the status-bar spinner (300ms–1s case).
pub fn spinner_glyph(tick: u64) -> &'static str {
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    #[expect(clippy::as_conversions, reason = "spinner index: bounded by mod 10")]
    frames[(tick % 10) as usize]
}

// ── Full entity list screen ───────────────────────────────────────────────────

/// Render only the entity rows inside the scrollable body area.
///
/// The description row is rendered separately by the outer screen function at
/// `layout[1]`, so this function starts at `entity_scroll` into `app.entities`
/// and renders each entity. `entity_index` is 0-based over the full list
/// (0 = description row, 1+ = entities); a row is focused when
/// `app.entity_index - 1 == eidx`.
pub fn render_entity_list_body<S: ReviewSurfaceExt>(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App<S>,
) {
    let total_rows = usize::from(area.height);
    let entities = &app.entities;
    let scroll = app.entity_scroll;

    // Pre-compute visible entity indices so cosmetic-filtered entities don't
    // produce blank rows (they were previously reached via `continue` while
    // still incrementing the row_offset position counter).
    let visible: Vec<usize> = (scroll..)
        .take_while(|&i| i < entities.len())
        .filter(|&i| !app.cosmetic_filter_on || entities.get(i).is_none_or(|e| !is_cosmetic(e)))
        .take(total_rows)
        .collect();

    for (row_offset, eidx) in visible.into_iter().enumerate() {
        let Some(entity) = entities.get(eidx) else {
            break;
        };
        let y = area.y + u16::try_from(row_offset).unwrap_or(u16::MAX);
        let row_area = Rect {
            y,
            height: 1,
            ..area
        };
        // entity_index 0 = description, 1 = entity[0], 2 = entity[1], ...
        let focused = app.entity_index.saturating_sub(1) == eidx && app.entity_index > 0;
        render_entity_row(
            frame,
            row_area,
            &EntityRowCtx {
                entity,
                focused,
                cosmetic_hidden: app.cosmetic_filter_on,
                comment_indicator: entity.comment_count > 0,
            },
        );
    }
}

// ── Row count (accounting for cosmetic filter) ────────────────────────────────

/// Total navigable rows: description row (index 0) + visible entities.
pub fn entity_list_len<S: ReviewSurfaceExt>(app: &App<S>) -> usize {
    let entity_count = if app.cosmetic_filter_on {
        app.entities.iter().filter(|e| !is_cosmetic(e)).count()
    } else {
        app.entities.len()
    };
    1 + entity_count // 1 for the description row
}

/// Move the entity list cursor by `delta`, clamping to valid rows.
///
/// `entity_index` 0 = description row (fixed, never scrolled into body).
/// `entity_index` 1..N = entity rows; the body scrolls to keep them visible.
pub fn move_entity_cursor<S: ReviewSurfaceExt>(app: &mut App<S>, delta: isize) {
    let len = entity_list_len(app);
    if len == 0 {
        return;
    }
    let new_idx = if delta >= 0 {
        (app.entity_index + delta.unsigned_abs()).min(len - 1)
    } else {
        app.entity_index.saturating_sub(delta.unsigned_abs())
    };
    app.entity_index = new_idx;

    // The body area shows entities (indices 1..N). entity_scroll is an offset
    // into app.entities (so entity at index I is at entity_scroll position when
    // entity_index - 1 == entity_scroll + row_offset). Adjust scroll so the
    // focused entity is visible. Description row (index 0) is fixed.
    if new_idx == 0 {
        return; // description row — no body scroll needed
    }
    let eidx = new_idx - 1; // index into app.entities
    let viewport = usize::from(app.viewport_rows).saturating_sub(4); // approx body rows
    if eidx < app.entity_scroll {
        app.entity_scroll = eidx;
    } else if eidx >= app.entity_scroll + viewport {
        app.entity_scroll = eidx.saturating_sub(viewport.saturating_sub(1));
    }
}

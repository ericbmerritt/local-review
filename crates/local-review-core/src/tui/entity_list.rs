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

/// Trait for an extraction task that can run on a background thread.
///
/// Implementations clone whatever surface data they need (paths, change
/// ids, registry handles, etc.) and run the extraction work without
/// referencing the surface, so the main thread is free to render the
/// loading overlay while extraction proceeds.
///
/// The implementation should:
/// - Poll `cancel.load(Relaxed)` between files and exit early via
///   `ExtractionEvent::Cancelled` if it returns `true`.
/// - Send `Progress { files_done, files_total, files_failed }` events as
///   counts change so the overlay updates.
/// - Send `FileExtracted { file_path, entities }` as each file completes
///   so the entity list can accumulate.
/// - Finish with `Complete` on success, `Cancelled` on cancel, or
///   `Error(String)` on fatal failure.
pub trait ExtractionRunner: Send + 'static {
    fn run(self: Box<Self>, tx: std::sync::mpsc::Sender<ExtractionEvent>, cancel: Arc<AtomicBool>);
}

// ── Entity list rendering ─────────────────────────────────────────────────────

const SIGIL_WIDTH: usize = 5; // "  Δ  " — 2-char indent + 2-char sigil + space
const GAP: usize = 2;
// Minimum column widths (used when the terminal is narrow).
const NAME_MIN: usize = 20;
const FILE_MIN: usize = 12;
const ANNOT_MIN: usize = 14;
// Maximum column widths (the column stops expanding past these even if space allows).
const NAME_MAX: usize = 52;
const FILE_MAX: usize = 40;
const ANNOT_MAX: usize = 22;
// Widths of the optional extras.
const RANGE_WIDTH: usize = 10; // ":NNN-NNN  " e.g. ":42-78  "

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

/// Column widths resolved for one row at a given terminal width.
struct RowLayout {
    name_width: usize,
    file_width: usize,
    annot_width: usize,
    show_range: bool,
}

impl RowLayout {
    fn new(width: usize, suffix_chars: usize) -> Self {
        let fixed = SIGIL_WIDTH + GAP + GAP + ANNOT_MIN + suffix_chars;
        let budget = width.saturating_sub(fixed);

        let name_width = (budget / 2).clamp(NAME_MIN, NAME_MAX);
        let mut remaining = budget.saturating_sub(name_width + GAP);

        let file_width = (remaining / 3 * 2).clamp(FILE_MIN, FILE_MAX);
        remaining = remaining.saturating_sub(file_width + GAP);

        let show_range = remaining >= RANGE_WIDTH;
        if show_range {
            remaining = remaining.saturating_sub(RANGE_WIDTH);
        }

        let annot_width = (ANNOT_MIN + remaining).min(ANNOT_MAX);
        Self {
            name_width,
            file_width,
            annot_width,
            show_range,
        }
    }
}

/// Build the text line for one entity row.
///
/// Columns expand proportionally to fill the terminal width:
/// - Name grows first (up to `NAME_MAX`).
/// - File path shows more directory context as width increases.
/// - Line range (`:42-78`) appears when at least `RANGE_WIDTH` chars are free.
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
    let base_style = if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    // Suffix glyphs that appear at the far right.
    let dot = if comment_indicator { " ●" } else { "" };
    let check = if entity.reviewed { " ✓" } else { "" };
    let suffix_chars = dot.chars().count() + check.chars().count();

    // Build the annotation string (may include "[cosmetic]" suffix).
    let annot_raw = annotation_text(entity);
    let annot_full = if is_cosmetic(entity) {
        format!("{annot_raw} [cosmetic]")
    } else {
        annot_raw
    };

    let layout = RowLayout::new(width, suffix_chars);
    let (name_cell, file_cell, annot_cell, range_cell) = build_cells(entity, &annot_full, &layout);
    let dim_style = if focused {
        base_style
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let sigil_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(fg)
    };

    TuiLine::from(vec![
        Span::raw("  "),
        Span::styled(sigil_str, sigil_style),
        Span::styled(name_cell, base_style),
        Span::styled(file_cell, dim_style),
        Span::styled(range_cell, dim_style),
        Span::styled(annot_cell, dim_style),
        Span::styled(dot, base_style),
        Span::styled(check, Style::default().fg(Color::Green)),
    ])
}

/// Build padded cell strings for each column.
fn build_cells(
    entity: &EntitySummary,
    annot_full: &str,
    layout: &RowLayout,
) -> (String, String, String, String) {
    let RowLayout {
        name_width,
        file_width,
        annot_width,
        show_range,
    } = *layout;

    let name = truncate_to(&entity.display_name, name_width);
    let name_cell = format!(
        "{name}{pad}  ",
        pad = " ".repeat(name_width.saturating_sub(name.chars().count()))
    );

    let file = fit_path(&entity.file_path.to_string_lossy(), file_width);
    let file_cell = format!(
        "{file}{pad}  ",
        pad = " ".repeat(file_width.saturating_sub(file.chars().count()))
    );

    let annot = truncate_to(annot_full, annot_width);
    let annot_cell = format!(
        "{annot}{pad}",
        pad = " ".repeat(annot_width.saturating_sub(annot.chars().count()))
    );

    let range_cell = if show_range {
        let raw = format!(":{}−{}", entity.line_range.0, entity.line_range.1);
        format!(
            "{:<width$}  ",
            truncate_to(&raw, RANGE_WIDTH - 2),
            width = RANGE_WIDTH - 2
        )
    } else {
        String::new()
    };

    (name_cell, file_cell, annot_cell, range_cell)
}

/// Return the deepest path segments that fit within `max_chars`, falling back
/// to a truncated filename when even the last segment is too long.
fn fit_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_owned();
    }
    let segs: Vec<&str> = path.split('/').collect();
    let mut s = segs.last().copied().unwrap_or("").to_owned();
    for seg in segs.iter().rev().skip(1) {
        let candidate = format!("{seg}/{s}");
        if candidate.chars().count() > max_chars {
            break;
        }
        s = candidate;
    }
    if s.chars().count() > max_chars {
        truncate_to(&s, max_chars)
    } else {
        s
    }
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
    let subj = truncate_to(subject, width.saturating_sub(SIGIL_WIDTH + dot_chars));
    let pad = " ".repeat(width.saturating_sub(SIGIL_WIDTH + subj.chars().count() + dot_chars));
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

/// Render the orientation header's body-peek row: the first body line of
/// the description, dimmed, indented under the `≡` sigil.
pub fn render_body_peek_row(frame: &mut Frame<'_>, area: Rect, peek: &str) {
    let width = usize::from(area.width);
    let text = truncate_to(peek, width.saturating_sub(SIGIL_WIDTH + 2));
    let line = TuiLine::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("\u{201c}{text}\u{201d}"),
            Style::default().fg(Color::DarkGray),
        ),
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

/// Compose the orientation header's scope line:
/// `Σ N entities · M files · ~L LOC · K sig changes`.
///
/// LOC counts added + removed diff rows across the per-file views (view 0,
/// the description view, is skipped). Files are distinct entity file paths.
pub fn stats_line(entities: &[EntitySummary], views: &[crate::tui::DiffView]) -> String {
    let n = entities.len();
    let files: std::collections::HashSet<&std::path::Path> =
        entities.iter().map(|e| e.file_path.as_path()).collect();
    let loc: usize = views
        .iter()
        .skip(1)
        .flat_map(|v| v.lines.iter())
        .filter(|l| {
            matches!(
                l.kind,
                crate::tui::RenderedLineKind::Added | crate::tui::RenderedLineKind::Removed
            )
        })
        .count();
    let sig = entities
        .iter()
        .filter(|e| {
            matches!(
                e.annotation,
                ChangeAnnotation::SigChanged | ChangeAnnotation::SigAndBody
            )
        })
        .count();
    let entities_part = if n == 1 {
        "1 entity".to_owned()
    } else {
        format!("{n} entities")
    };
    let files_part = if files.len() == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", files.len())
    };
    let sig_part = if sig == 1 {
        "1 sig change".to_owned()
    } else {
        format!("{sig} sig changes")
    };
    format!("\u{3a3} {entities_part} · {files_part} · ~{loc} LOC · {sig_part}")
}

/// Render the orientation header's scope row, dimmed.
pub fn render_stats_row(frame: &mut Frame<'_>, area: Rect, text: &str) {
    let width = usize::from(area.width);
    let body = truncate_to(text, width.saturating_sub(2));
    let line = TuiLine::from(vec![
        Span::raw("  "),
        Span::styled(body, Style::default().fg(Color::DarkGray)),
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
    // Hide the file counter when there is no real progress data
    // (`files_total == 0` is the initial synchronous-load case where no
    // background worker is sending Progress events). A static `0 / 0`
    // counter reads as broken; an ellipsis reads as in-progress.
    let text = if files_total == 0 {
        format!(" {spinner} Extracting entities…")
    } else {
        format!(" {spinner} Extracting entities  ·  {files_done} / {files_total} files{fail_str}")
    };
    let hint = if files_total == 0 {
        "   (this can take a few seconds for large changes)"
    } else {
        "   Esc to cancel"
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::DiffFile;
    use crate::semantic::fallback_summary_for_file;
    use crate::tui::diff_view::RenderedLine;
    use crate::tui::{DiffView, RenderedLineKind};

    fn entity(path: &str, annotation: ChangeAnnotation) -> EntitySummary {
        let mut e = fallback_summary_for_file(&DiffFile::Modified {
            path: std::path::PathBuf::from(path),
            hunks: vec![],
        });
        e.annotation = annotation;
        e
    }

    fn line(kind: RenderedLineKind) -> RenderedLine {
        RenderedLine {
            kind,
            text: String::new(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        }
    }

    fn view(kinds: &[RenderedLineKind]) -> DiffView {
        DiffView {
            title: String::new(),
            lines: kinds.iter().map(|k| line(*k)).collect(),
            paired_rows: vec![],
            token_spans: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn stats_line_counts_entities_files_loc_and_sig_changes() {
        let entities = vec![
            entity("a.rs", ChangeAnnotation::SigAndBody),
            entity("a.rs", ChangeAnnotation::BodyOnly),
            entity("b.rs", ChangeAnnotation::None),
        ];
        let views = vec![
            view(&[RenderedLineKind::Notice]), // view 0: description, skipped
            view(&[
                RenderedLineKind::HunkHeader,
                RenderedLineKind::Added,
                RenderedLineKind::Removed,
                RenderedLineKind::Context,
            ]),
            view(&[RenderedLineKind::Added]),
        ];
        assert_eq!(
            stats_line(&entities, &views),
            "Σ 3 entities · 2 files · ~3 LOC · 1 sig change"
        );
    }

    #[test]
    fn stats_line_singulars() {
        let entities = vec![entity("a.rs", ChangeAnnotation::SigChanged)];
        let views = vec![view(&[])];
        assert_eq!(
            stats_line(&entities, &views),
            "Σ 1 entity · 1 file · ~0 LOC · 1 sig change"
        );
    }

    #[test]
    fn stats_line_zero_sig_changes_plural() {
        let entities = vec![
            entity("a.rs", ChangeAnnotation::BodyOnly),
            entity("b.rs", ChangeAnnotation::BodyOnly),
        ];
        let views = vec![view(&[])];
        assert_eq!(
            stats_line(&entities, &views),
            "Σ 2 entities · 2 files · ~0 LOC · 0 sig changes"
        );
    }
}

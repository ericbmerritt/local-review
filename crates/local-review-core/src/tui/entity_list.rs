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
    use crate::semantic::RefactorKind;
    let kind = kind_label(e.kind);
    // Refactor tags take over the change column: the tag says more than the
    // generic change word ("extracted ← validate()" vs "added").
    let change = match (&e.refactor, e.change) {
        (Some(RefactorKind::Renamed { from, .. }), _) => {
            if e.is_behavior_preserving() {
                format!("renamed ← {from}")
            } else {
                format!("renamed ← {from} +body")
            }
        }
        (Some(RefactorKind::Extracted { from }), _) => {
            format!("extracted ← {}", from.display_name())
        }
        (_, ChangeType::Added) => "added".to_owned(),
        (_, ChangeType::Deleted) => "deleted".to_owned(),
        (_, ChangeType::Moved) => {
            let src = e
                .source_file
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            // A move whose content also changed is not behavior-preserving;
            // say so rather than letting the tag imply a pure move.
            if e.is_behavior_preserving() {
                format!("moved from {src}")
            } else {
                format!("moved from {src} +edits")
            }
        }
        (_, ChangeType::Modified) => match e.annotation {
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

/// Rows the `;` filter hides and the renderer dims: cosmetic changes plus
/// behavior-preserving refactors (rename / identical move / extract). The
/// classification is a parser heuristic — demoted, never hidden by default.
fn is_demoted(e: &EntitySummary) -> bool {
    is_cosmetic(e) || e.is_behavior_preserving()
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
    let (fg, dim) = if is_demoted(entity) {
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

    // Build the annotation string (may include a "[cosmetic]" suffix; the
    // refactor tag itself is part of annotation_text).
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

    // High-risk badge occupies the 2-char indent so row columns stay aligned.
    let high_risk = entity
        .risk
        .as_ref()
        .is_some_and(|r| r.tier == crate::semantic::RiskTier::High);
    let badge = if high_risk {
        Span::styled(
            "! ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };

    TuiLine::from(vec![
        badge,
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
    if cosmetic_hidden && is_demoted(entity) {
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

/// Recompute each entity's `comment_count` from already-fetched inline
/// comments. Pure: no IO, no surface access — `views` and `comments_per_view`
/// are parallel slices (index `i` of each belongs to the same rendered view)
/// gathered by the caller via `ReviewSurface::inline_comments_for_view`.
/// Drives the entity-list `●` indicator (`comment_indicator` in
/// [`render_entity_row`]).
///
/// A comment matches an entity when its view's file matches the entity's
/// file and its line (preferring `target_line`, falling back to
/// `source_line` for comments anchored to a since-removed line) falls
/// within the entity's `line_range`. Heuristic, not stored identity — same
/// line-containment approach `render_entity_diff_screen` already uses to
/// jump to an entity's first changed line.
pub(crate) fn recompute_comment_counts(
    entities: &mut [EntitySummary],
    views: &[crate::tui::DiffView],
    comments_per_view: &[Vec<crate::tui::InlineComment>],
) {
    for entity in &mut *entities {
        entity.comment_count = 0;
    }
    for (view, comments) in views.iter().zip(comments_per_view) {
        if comments.is_empty() {
            continue;
        }
        for entity in &mut *entities {
            let path = entity.file_path.to_string_lossy();
            if !crate::tui::diff_view::view_title_matches_path(&view.title, &path) {
                continue;
            }
            let (start, end) = entity.line_range;
            entity.comment_count += comments
                .iter()
                .filter(|c| {
                    c.target_line.is_some_and(|l| l >= start && l <= end)
                        || c.source_line.is_some_and(|l| l >= start && l <= end)
                })
                .count();
        }
    }
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
/// One rendered row of the entity-list body: a group header (display
/// only) or a navigable entity row.
enum BodyRow {
    Header(usize),
    Entity(usize),
}

pub fn render_entity_list_body<S: ReviewSurfaceExt>(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App<S>,
) {
    let total_rows = usize::from(area.height);
    let entities = &app.entities;
    let scroll = app.entity_scroll;
    let visible_pred =
        |i: usize| !app.cosmetic_filter_on || entities.get(i).is_none_or(|e| !is_demoted(e));

    // Concern-group headers render before their first *visible* member —
    // a group whose members are all filtered out contributes no header,
    // so the `;` filter composes with grouping without empty sections.
    // Inverted (entity index -> group index) for O(1) lookup per row below.
    let header_before: std::collections::HashMap<usize, usize> = app
        .group_spans
        .iter()
        .enumerate()
        .filter_map(|(gidx, g)| {
            (g.start..g.start + g.len)
                .find(|&i| visible_pred(i))
                .map(|first| (first, gidx))
        })
        .collect();

    // Interleave header and entity rows from the scroll offset. Headers
    // are display-only: they consume a body row but are never focusable,
    // so `entity_index`/`entity_scroll` stay in entity space. Scrolling
    // into the middle of a group drops its header (accepted v1 tradeoff).
    let mut rows: Vec<BodyRow> = Vec::new();
    let mut i = scroll;
    while rows.len() < total_rows && i < entities.len() {
        if !visible_pred(i) {
            i += 1;
            continue;
        }
        if let Some(&gidx) = header_before.get(&i) {
            rows.push(BodyRow::Header(gidx));
            if rows.len() == total_rows {
                break;
            }
        }
        rows.push(BodyRow::Entity(i));
        i += 1;
    }

    for (row_offset, row) in rows.into_iter().enumerate() {
        let y = area.y + u16::try_from(row_offset).unwrap_or(u16::MAX);
        let row_area = Rect {
            y,
            height: 1,
            ..area
        };
        match row {
            BodyRow::Header(gidx) => {
                if let Some(span) = app.group_spans.get(gidx) {
                    render_group_header(frame, row_area, span);
                }
            }
            BodyRow::Entity(eidx) => {
                let Some(entity) = entities.get(eidx) else {
                    break;
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
    }
}

/// Render one concern-group header: `── <label> ───── <tier> ──`, dimmed.
fn render_group_header(frame: &mut Frame<'_>, area: Rect, span: &crate::semantic::GroupSpan) {
    let width = usize::from(area.width);
    let tier_part = format!(" {} ──", span.max_tier.label());
    let label_budget = width.saturating_sub(tier_part.chars().count() + 6);
    let label = truncate_to(&span.label, label_budget);
    let prefix = format!("── {label} ");
    let fill = width.saturating_sub(prefix.chars().count() + tier_part.chars().count());
    let line = TuiLine::from(Span::styled(
        format!("{prefix}{}{tier_part}", "─".repeat(fill)),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            y: area.y,
            height: 1,
            ..area
        },
    );
}

// ── Row count (accounting for cosmetic filter) ────────────────────────────────

/// Total navigable rows: description row (index 0) + visible entities.
pub fn entity_list_len<S: ReviewSurfaceExt>(app: &App<S>) -> usize {
    let entity_count = if app.cosmetic_filter_on {
        app.entities.iter().filter(|e| !is_demoted(e)).count()
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
    let eidx = new_idx - 1;
    // Approx body rows: chrome takes ~4; group headers consume one row each.
    // Reserve the full group count (never capped) — a window can never hold
    // more headers than exist, so this is always a safe upper bound. Capping
    // it would undercount on changes with many small concern groups, which
    // could let the render loop hit `total_rows` on a header row and drop
    // the focused entity's row for that frame. `.max(1)` is the only floor,
    // for the pathological case where header count alone exceeds the
    // viewport — degraded scrolling (one row at a time), never a hidden
    // cursor.
    let headers = app.group_spans.len();
    let viewport = usize::from(app.viewport_rows)
        .saturating_sub(4)
        .saturating_sub(headers)
        .max(1);
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

    fn entity_with_range(path: &str, start: u32, end: u32) -> EntitySummary {
        let mut e = entity(path, ChangeAnnotation::None);
        e.line_range = (start, end);
        e
    }

    fn comment_on(line: u32) -> crate::tui::InlineComment {
        crate::tui::InlineComment {
            source_line: None,
            target_line: Some(line),
            severity: crate::severity::Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["nit".to_owned()],
            comment_index: crate::tui::CommentIndex::Local(0),
        }
    }

    #[test]
    fn recompute_comment_counts_matches_by_file_and_line_range() {
        let views = vec![DiffView {
            title: "src/lib.rs".to_owned(),
            ..view(&[])
        }];
        let mut entities = vec![
            entity_with_range("src/lib.rs", 1, 9),
            entity_with_range("src/lib.rs", 11, 19),
        ];
        let comments_per_view = vec![vec![comment_on(5)]];

        recompute_comment_counts(&mut entities, &views, &comments_per_view);

        assert_eq!(
            entities[0].comment_count, 1,
            "line 5 falls in entity 0's (1,9) range"
        );
        assert_eq!(
            entities[1].comment_count, 0,
            "entity 1's (11,19) range excludes line 5"
        );
    }

    #[test]
    fn recompute_comment_counts_matches_neither_entity_when_line_falls_in_gap() {
        let views = vec![DiffView {
            title: "src/lib.rs".to_owned(),
            ..view(&[])
        }];
        let mut entities = vec![
            entity_with_range("src/lib.rs", 1, 9),
            entity_with_range("src/lib.rs", 11, 19),
        ];
        let comments_per_view = vec![vec![comment_on(10)]]; // between the two ranges

        recompute_comment_counts(&mut entities, &views, &comments_per_view);

        assert_eq!(entities[0].comment_count, 0);
        assert_eq!(entities[1].comment_count, 0);
    }

    #[test]
    fn recompute_comment_counts_resets_stale_counts_with_no_current_match() {
        let views = vec![DiffView {
            title: "src/lib.rs".to_owned(),
            ..view(&[])
        }];
        let mut entities = vec![entity_with_range("src/lib.rs", 1, 9)];
        entities[0].comment_count = 3; // stale count from a prior comment set
        let comments_per_view = vec![Vec::new()];

        recompute_comment_counts(&mut entities, &views, &comments_per_view);

        assert_eq!(
            entities[0].comment_count, 0,
            "no current comments — must reset, not accumulate"
        );
    }

    #[test]
    fn recompute_comment_counts_matches_status_suffixed_title_and_source_line_fallback() {
        let views = vec![DiffView {
            title: "src/lib.rs (removed)".to_owned(),
            ..view(&[])
        }];
        let mut entities = vec![entity_with_range("src/lib.rs", 1, 9)];
        let comments_per_view = vec![vec![crate::tui::InlineComment {
            source_line: Some(5),
            target_line: None, // deleted file: no after-state line
            severity: crate::severity::Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["nit".to_owned()],
            comment_index: crate::tui::CommentIndex::Local(0),
        }]];

        recompute_comment_counts(&mut entities, &views, &comments_per_view);

        assert_eq!(
            entities[0].comment_count, 1,
            "\" (removed)\" suffix must strip for title matching; source_line must fall back"
        );
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

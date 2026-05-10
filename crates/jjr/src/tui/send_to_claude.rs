use std::path::PathBuf;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::comment::{Anchor, Comment, Severity, Status};
use crate::packet::Packet;

use super::composer::ScopeTag;
use super::composer_overlay::centered_rect;
use super::{severity_color, severity_label};

const MODAL_WIDTH: u16 = 67;

const MODAL_HEIGHT: u16 = 25;

pub(super) const CONFIRM_FOOTER: &str = "  v view full prompt    Enter send    Esc cancel";

pub(super) const PROMPT_VIEW_FOOTER: &str = "  \u{2191}\u{2193} scroll    q back    Esc cancel";

pub(super) struct ConfirmData {
    pub(super) change_id: crate::change_id::ChangeId,
    pub(super) change_description: String,
    pub(super) scope_severity_grid: Vec<ScopeSeverityRow>,
    pub(super) files_affected: Vec<FileCountRow>,
    pub(super) stale_count: usize,
    pub(super) packet: Packet,
}

pub(super) struct ScopeSeverityRow {
    pub(super) scope: ScopeTag,
    pub(super) severity: Severity,
    pub(super) count: usize,
}

pub(super) struct FileCountRow {
    pub(super) file: PathBuf,
    pub(super) count: usize,
}

pub(super) enum SendToClaudeState {
    Confirm(ConfirmData),
    PromptView {
        confirm: ConfirmData,
        prompt: String,
        scroll_offset: u16,
    },
}

impl SendToClaudeState {
    /// Transition to `PromptView`, consuming `self`.
    ///
    /// If called on `Confirm`, moves the data directly. If called on
    /// `PromptView`, extracts the inner `confirm` data and rebuilds the view.
    ///
    /// Callers invoke this only from the `Confirm` state; the
    /// `PromptView` → `PromptView` path is exercised in tests for completeness
    /// and is not reachable from the key handler.
    pub(super) fn into_prompt_view(self) -> Self {
        let data = match self {
            Self::Confirm(d) => d,
            Self::PromptView { confirm, .. } => confirm,
        };
        let prompt = crate::packet::render_prompt_with_mode(
            &data.packet,
            crate::packet::PromptMode::JsonlPaths,
        );
        Self::PromptView {
            confirm: data,
            prompt,
            scroll_offset: 0,
        }
    }

    /// Transition to `Confirm`, consuming `self`.
    ///
    /// If called on `PromptView`, extracts the inner `confirm` data. If called
    /// on `Confirm`, the data is preserved unchanged.
    pub(super) fn into_confirm(self) -> Self {
        let data = match self {
            Self::Confirm(d) => d,
            Self::PromptView { confirm, .. } => confirm,
        };
        Self::Confirm(data)
    }
}

/// Row order: stack first (Required → Suggestion → Note), then change, then
/// line. Empty (scope, severity) pairs are omitted.
pub(super) fn compute_scope_severity_grid(packet: &Packet) -> Vec<ScopeSeverityRow> {
    let mut rows: Vec<ScopeSeverityRow> = Vec::new();

    for severity in [Severity::Required, Severity::Suggestion, Severity::Note] {
        let count = pending_count(&packet.stack_comments, severity);
        if count > 0 {
            rows.push(ScopeSeverityRow {
                scope: ScopeTag::Stack,
                severity,
                count,
            });
        }
    }

    for cp in &packet.changes {
        for severity in [Severity::Required, Severity::Suggestion, Severity::Note] {
            let count = pending_count(&cp.change_comments, severity);
            if count > 0 {
                rows.push(ScopeSeverityRow {
                    scope: ScopeTag::Change,
                    severity,
                    count,
                });
            }
        }
    }

    for cp in &packet.changes {
        for severity in [Severity::Required, Severity::Suggestion, Severity::Note] {
            let count = pending_count(&cp.line_comments, severity);
            if count > 0 {
                rows.push(ScopeSeverityRow {
                    scope: ScopeTag::Line,
                    severity,
                    count,
                });
            }
        }
    }

    rows
}

fn pending_count(comments: &[Comment], severity: Severity) -> usize {
    comments
        .iter()
        .filter(|c| {
            c.severity == severity && !matches!(c.status, Some(Status::Stale | Status::Orphaned))
        })
        .count()
}

/// Results are sorted by file path. Change-scoped and stack-scoped comments
/// are excluded — only line-anchored comments contribute. Stale and orphaned
/// comments are excluded so the count matches what's actually being sent.
pub(super) fn compute_files_affected(packet: &Packet) -> Vec<FileCountRow> {
    let mut counts: std::collections::BTreeMap<PathBuf, usize> = std::collections::BTreeMap::new();

    for cp in &packet.changes {
        for comment in &cp.line_comments {
            if matches!(comment.status, Some(Status::Stale | Status::Orphaned)) {
                continue;
            }
            if let Anchor::Line { location, .. } = &comment.anchor {
                *counts.entry(location.file.clone()).or_insert(0) += 1;
            }
        }
    }

    counts
        .into_iter()
        .map(|(file, count)| FileCountRow { file, count })
        .collect()
}

fn scope_label(scope: ScopeTag) -> &'static str {
    match scope {
        ScopeTag::Stack => "stack",
        ScopeTag::Change => "change",
        ScopeTag::Line => "line",
        ScopeTag::Description => "description",
    }
}

pub(super) fn render(frame: &mut Frame<'_>, state: &SendToClaudeState) {
    match state {
        SendToClaudeState::Confirm(data) => render_confirm(frame, data),
        SendToClaudeState::PromptView {
            prompt,
            scroll_offset,
            ..
        } => render_prompt_view(frame, prompt, *scroll_offset),
    }
}

/// Width budget (chars) for the file path column. Paths longer than this are
/// truncated so the count column doesn't shift.
const FILES_PATH_WIDTH: usize = 40;

fn render_confirm(frame: &mut Frame<'_>, data: &ConfirmData) {
    let area = frame.area();
    let modal = centered_rect(area, MODAL_WIDTH, MODAL_HEIGHT);

    frame.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Double)
        .title(" Send to Claude ");
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let [body_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    let [sep_row, text_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(footer_area);

    let mut lines: Vec<TuiLine<'_>> = Vec::new();

    let change_short: String = data.change_id.as_str().chars().take(8).collect();
    let desc_budget =
        usize::from(body_area.width).saturating_sub(12 + change_short.chars().count());
    let desc_preview = crate::util::truncate(&data.change_description, desc_budget);
    lines.push(TuiLine::from(format!(
        "  Change      {change_short} \u{2014} {desc_preview}"
    )));
    lines.push(TuiLine::from("  Scope       current change"));
    lines.push(TuiLine::default());

    let sep_width = usize::from(body_area.width).saturating_sub(4);
    lines.push(TuiLine::from(format!("  {}", "\u{2500}".repeat(sep_width))));
    lines.push(TuiLine::default());

    if data.scope_severity_grid.is_empty() {
        lines.push(TuiLine::from("  Comments to send   (none)"));
    } else {
        lines.push(TuiLine::from("  Comments to send"));
        lines.push(TuiLine::from(Span::styled(
            "      scope    severity     count",
            Style::default().add_modifier(Modifier::DIM),
        )));
        for row in &data.scope_severity_grid {
            lines.push(TuiLine::from(vec![
                Span::raw(format!("      {:<7}  ", scope_label(row.scope))),
                Span::styled(
                    format!("{:<12}", severity_label(row.severity)),
                    Style::default().fg(severity_color(row.severity)),
                ),
                Span::raw(format!(" {:>3}", row.count)),
            ]));
        }
    }

    lines.push(TuiLine::default());

    if !data.files_affected.is_empty() {
        lines.push(TuiLine::from("  Files affected"));
        for row in &data.files_affected {
            let path = row.file.display().to_string();
            let path_display = crate::util::truncate(&path, FILES_PATH_WIDTH);
            let count = row.count;
            lines.push(TuiLine::from(format!(
                "      {path_display:<FILES_PATH_WIDTH$} {count:>3}",
            )));
        }
        lines.push(TuiLine::default());
    }

    if data.stale_count > 0 {
        lines.push(TuiLine::from(format!(
            "  Stale comments    {}  excluded",
            data.stale_count
        )));
        lines.push(TuiLine::default());
    }

    let body_widget = Paragraph::new(lines);
    frame.render_widget(body_widget, body_area);

    let sep_line = "\u{2550}".repeat(usize::from(inner.width));
    frame.render_widget(Paragraph::new(sep_line), sep_row);
    frame.render_widget(Paragraph::new(CONFIRM_FOOTER), text_row);
}

fn render_prompt_view(frame: &mut Frame<'_>, prompt: &str, scroll_offset: u16) {
    let area = frame.area();

    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Full Prompt ");
    let inner = block.inner(layout[0]);
    frame.render_widget(block, layout[0]);

    let widget = Paragraph::new(prompt.to_owned()).scroll((scroll_offset, 0));
    frame.render_widget(widget, inner);

    frame.render_widget(Paragraph::new(PROMPT_VIEW_FOOTER), layout[1]);
}

pub(super) fn stale_count_for_change(
    repo_root: &std::path::Path,
    change_id: &crate::change_id::ChangeId,
    revset_hash: Option<crate::stack::RevsetHash>,
) -> usize {
    let mut count = 0;
    if let Ok(comments) = crate::store::load_change_comments(repo_root, change_id) {
        count += comments
            .iter()
            .filter(|c| c.status == Some(Status::Stale))
            .count();
    }
    if let Some(hash) = revset_hash {
        if let Ok(stack_comments) = crate::store::load_stack_comments(repo_root, &hash) {
            count += stack_comments
                .iter()
                .filter(|c| c.status == Some(Status::Stale))
                .count();
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::comment::{Anchor, Comment, LineAnchor, SchemaVersion, Severity, Side, Status};
    use crate::packet::{ChangePacket, Packet};
    use crate::stack::RevsetHash;

    fn cid() -> ChangeId {
        ChangeId::parse("abc12345").unwrap()
    }

    fn commit_id() -> CommitId {
        CommitId::parse(&"b".repeat(40)).unwrap()
    }

    fn make_line_comment(file: &str, severity: Severity) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: cid(),
                location: LineAnchor {
                    file: PathBuf::from(file),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
                    target_text: "text".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "body".to_owned(),
            severity,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    fn make_change_comment(severity: Severity) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change { change_id: cid() },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "body".to_owned(),
            severity,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    fn make_stack_comment(severity: Severity) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: RevsetHash::from_revset("@"),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "body".to_owned(),
            severity,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    fn empty_packet() -> Packet {
        Packet {
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![],
        }
    }

    fn packet_with_all_scopes() -> Packet {
        Packet {
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment(Severity::Suggestion)],
            changes: vec![ChangePacket {
                change_id: cid(),
                commit_id: commit_id(),
                description: "desc".to_owned(),
                change_comments: vec![make_change_comment(Severity::Required)],
                description_comments: vec![],
                line_comments: vec![
                    make_line_comment("src/client.rs", Severity::Required),
                    make_line_comment("src/client.rs", Severity::Required),
                    make_line_comment("src/retry.rs", Severity::Suggestion),
                    make_line_comment("src/client.rs", Severity::Note),
                ],
                diff: None,
            }],
        }
    }

    #[test]
    fn scope_severity_grid_empty_packet_returns_empty() {
        let packet = empty_packet();
        let rows = compute_scope_severity_grid(&packet);
        assert!(rows.is_empty());
    }

    #[test]
    fn scope_severity_grid_all_scopes_in_spec_order() {
        let packet = packet_with_all_scopes();
        let rows = compute_scope_severity_grid(&packet);

        assert_eq!(rows.len(), 5, "expected 5 rows; got {}", rows.len());

        assert_eq!(rows[0].scope, ScopeTag::Stack);
        assert_eq!(rows[0].severity, Severity::Suggestion);
        assert_eq!(rows[0].count, 1);

        assert_eq!(rows[1].scope, ScopeTag::Change);
        assert_eq!(rows[1].severity, Severity::Required);
        assert_eq!(rows[1].count, 1);

        assert_eq!(rows[2].scope, ScopeTag::Line);
        assert_eq!(rows[2].severity, Severity::Required);
        assert_eq!(rows[2].count, 2);

        assert_eq!(rows[3].scope, ScopeTag::Line);
        assert_eq!(rows[3].severity, Severity::Suggestion);
        assert_eq!(rows[3].count, 1);

        assert_eq!(rows[4].scope, ScopeTag::Line);
        assert_eq!(rows[4].severity, Severity::Note);
        assert_eq!(rows[4].count, 1);
    }

    #[test]
    fn scope_severity_grid_stale_comments_excluded() {
        let mut comment = make_stack_comment(Severity::Required);
        comment.status = Some(Status::Stale);
        let packet = Packet {
            stack_comments: vec![comment],
            ..empty_packet()
        };
        let rows = compute_scope_severity_grid(&packet);
        assert!(rows.is_empty(), "stale comment must be excluded");
    }

    #[test]
    fn scope_severity_grid_orphaned_comments_excluded() {
        let mut comment = make_stack_comment(Severity::Required);
        comment.status = Some(Status::Orphaned);
        let packet = Packet {
            stack_comments: vec![comment],
            ..empty_packet()
        };
        let rows = compute_scope_severity_grid(&packet);
        assert!(rows.is_empty(), "orphaned comment must be excluded");
    }

    #[test]
    fn files_affected_counts_only_line_scoped_comments() {
        let packet = packet_with_all_scopes();
        let rows = compute_files_affected(&packet);

        assert_eq!(rows.len(), 2);

        let client = rows
            .iter()
            .find(|r| r.file == std::path::Path::new("src/client.rs"));
        assert!(client.is_some(), "src/client.rs missing");
        assert_eq!(client.unwrap().count, 3);

        let retry = rows
            .iter()
            .find(|r| r.file == std::path::Path::new("src/retry.rs"));
        assert!(retry.is_some(), "src/retry.rs missing");
        assert_eq!(retry.unwrap().count, 1);
    }

    #[test]
    fn files_affected_sorted_by_path() {
        let packet = Packet {
            changes: vec![ChangePacket {
                change_id: cid(),
                commit_id: commit_id(),
                description: "d".to_owned(),
                change_comments: vec![],
                description_comments: vec![],
                line_comments: vec![
                    make_line_comment("z/last.rs", Severity::Note),
                    make_line_comment("a/first.rs", Severity::Note),
                    make_line_comment("m/middle.rs", Severity::Note),
                ],
                diff: None,
            }],
            ..empty_packet()
        };
        let rows = compute_files_affected(&packet);
        let paths: Vec<String> = rows.iter().map(|r| r.file.display().to_string()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "rows must be sorted by path");
    }

    #[test]
    fn files_affected_excludes_stale_comments() {
        let mut stale = make_line_comment("src/stale.rs", Severity::Required);
        stale.status = Some(Status::Stale);
        let pending = make_line_comment("src/pending.rs", Severity::Required);
        let packet = Packet {
            changes: vec![ChangePacket {
                change_id: cid(),
                commit_id: commit_id(),
                description: "d".to_owned(),
                change_comments: vec![],
                description_comments: vec![],
                line_comments: vec![stale, pending],
                diff: None,
            }],
            ..empty_packet()
        };
        let rows = compute_files_affected(&packet);
        assert_eq!(rows.len(), 1, "only pending file should appear");
        assert_eq!(rows[0].file, std::path::Path::new("src/pending.rs"));
    }

    #[test]
    fn files_affected_excludes_orphaned_comments() {
        let mut orphaned = make_line_comment("src/orphaned.rs", Severity::Required);
        orphaned.status = Some(Status::Orphaned);
        let pending = make_line_comment("src/pending.rs", Severity::Required);
        let packet = Packet {
            changes: vec![ChangePacket {
                change_id: cid(),
                commit_id: commit_id(),
                description: "d".to_owned(),
                change_comments: vec![],
                description_comments: vec![],
                line_comments: vec![orphaned, pending],
                diff: None,
            }],
            ..empty_packet()
        };
        let rows = compute_files_affected(&packet);
        assert_eq!(rows.len(), 1, "only pending file should appear");
        assert_eq!(rows[0].file, std::path::Path::new("src/pending.rs"));
    }

    #[test]
    fn files_affected_excludes_change_scoped_and_stack_scoped() {
        let packet = Packet {
            stack_comments: vec![make_stack_comment(Severity::Required)],
            changes: vec![ChangePacket {
                change_id: cid(),
                commit_id: commit_id(),
                description: "d".to_owned(),
                change_comments: vec![make_change_comment(Severity::Suggestion)],
                description_comments: vec![],
                line_comments: vec![],
                diff: None,
            }],
            ..empty_packet()
        };
        let rows = compute_files_affected(&packet);
        assert!(
            rows.is_empty(),
            "no line-scoped comments → empty files_affected"
        );
    }

    fn make_confirm_data() -> ConfirmData {
        ConfirmData {
            change_id: cid(),
            change_description: "test change".to_owned(),
            scope_severity_grid: vec![],
            files_affected: vec![],
            stale_count: 0,
            packet: empty_packet(),
        }
    }

    #[test]
    fn into_prompt_view_from_confirm_produces_prompt_view() {
        let state = SendToClaudeState::Confirm(make_confirm_data());
        let result = state.into_prompt_view();
        assert!(
            matches!(result, SendToClaudeState::PromptView { .. }),
            "Confirm.into_prompt_view() must produce PromptView"
        );
    }

    #[test]
    fn into_prompt_view_from_prompt_view_resets_scroll() {
        let state = SendToClaudeState::PromptView {
            confirm: make_confirm_data(),
            prompt: "already a prompt".to_owned(),
            scroll_offset: 42,
        };
        let result = state.into_prompt_view();
        match result {
            SendToClaudeState::PromptView { scroll_offset, .. } => {
                assert_eq!(
                    scroll_offset, 0,
                    "scroll must reset when rebuilding PromptView"
                );
            }
            SendToClaudeState::Confirm(_) => {
                panic!("PromptView.into_prompt_view() must stay PromptView");
            }
        }
    }

    #[test]
    fn into_confirm_from_prompt_view_produces_confirm() {
        let state = SendToClaudeState::PromptView {
            confirm: make_confirm_data(),
            prompt: "some prompt".to_owned(),
            scroll_offset: 5,
        };
        let result = state.into_confirm();
        assert!(
            matches!(result, SendToClaudeState::Confirm(_)),
            "PromptView.into_confirm() must produce Confirm"
        );
    }

    #[test]
    fn into_confirm_from_confirm_is_identity() {
        let state = SendToClaudeState::Confirm(make_confirm_data());
        let result = state.into_confirm();
        assert!(
            matches!(result, SendToClaudeState::Confirm(_)),
            "Confirm.into_confirm() must stay Confirm"
        );
    }

    #[test]
    fn confirm_footer_fits_within_80_cols() {
        let len = CONFIRM_FOOTER.chars().count();
        assert!(len <= 80, "CONFIRM_FOOTER ({len} chars) exceeds 80 cols");
    }

    #[test]
    fn prompt_view_footer_fits_within_80_cols() {
        let len = PROMPT_VIEW_FOOTER.chars().count();
        assert!(
            len <= 80,
            "PROMPT_VIEW_FOOTER ({len} chars) exceeds 80 cols"
        );
    }
}

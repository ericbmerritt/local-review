//! Terminal UI entry point for ggr: wires `PrDetails` into the shared
//! `App<GgrSurface>` review loop from `local-review-core`.

use std::io::{stdout, Stdout};

use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};

use local_review_core::tui::diff_view::InlineComment;
use local_review_core::tui::{
    file_picker, run_app as core_run_app, App, AppError, DeleteOutcome, DeleteRequest, DiffView,
    ExtraKeyAction, ExtraScreen, ExtraScreenAction, ExtraScreenContext, FilePickerEntry,
    MarkReviewedOutcome, ReviewSurface, ReviewSurfaceExt, ReviewedOutcome, SaveOutcome,
    SaveRequest, SeverityHistogram, TransitionMode, UpdateRequest, MIN_COLS, MIN_ROWS,
};
use local_review_core::util::{strip_controls, strip_controls_preserve_newlines};
use local_review_core::Severity;

use crate::error::{GgrError, Result};
use crate::gh;
use crate::pr::PrDetails;

// ── constants ─────────────────────────────────────────────────────────────────

const THREADS_EXPANDED_MSG: &str = "threads expanded";
const THREADS_COLLAPSED_MSG: &str = "threads collapsed";

// ── GgrSurface ────────────────────────────────────────────────────────────────

/// Loaded-entry state for [`GgrSurface`].
///
/// The invariant — "description has no diff; commit-diff entries always carry
/// a diff" — is structural rather than doc-only.
enum State {
    /// The PR description cover page (entry 0). No diff is loaded.
    Description,
    /// A per-commit diff entry. `index` is the 1-based entry index;
    /// `diff` is the fetched diff for that commit.
    CommitDiff {
        index: usize,
        diff: local_review_core::diff::Diff,
    },
}

/// ggr-specific TUI surface. Plugged into `local_review_core::tui::App<GgrSurface>`
/// via [`ReviewSurface`] and [`ReviewSurfaceExt`].
///
/// Entry 0 is the PR description page. Entries 1..=N correspond to PR commits
/// 0..N−1 (oldest-first). For each commit entry, `fetch_views` fetches the
/// per-commit diff on demand and returns the rendered view list.
pub(crate) struct GgrSurface {
    pr: PrDetails,
    state: State,
    threads_expanded: bool,
}

impl GgrSurface {
    pub(crate) fn new(pr: PrDetails) -> Self {
        Self {
            pr,
            state: State::Description,
            threads_expanded: true,
        }
    }

    fn pr_description_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&strip_controls(&self.pr.title));
        if !self.pr.body.is_empty() {
            out.push_str("\n\n");
            out.push_str(&strip_controls_preserve_newlines(&self.pr.body));
        }
        for c in &self.pr.comments {
            out.push_str("\n\n---\n");
            out.push_str(&strip_controls(&c.author));
            out.push_str(": ");
            out.push_str(&strip_controls_preserve_newlines(&c.body));
        }
        out
    }
}

// ── ReviewSurface impl ────────────────────────────────────────────────────────

impl ReviewSurface for GgrSurface {
    type Error = GgrError;

    fn entry_count(&self) -> usize {
        self.pr.commits.len() + 1
    }

    fn current_entry_index(&self) -> usize {
        match self.state {
            State::Description => 0,
            State::CommitDiff { index, .. } => index,
        }
    }

    fn entry_id_display(&self, idx: usize) -> String {
        if idx == 0 {
            return format!("#{}", self.pr.number);
        }
        self.pr
            .commits
            .get(idx - 1)
            .map(|c| c.short_sha.clone())
            .unwrap_or_default()
    }

    fn entry_description(&self, idx: usize) -> String {
        if idx == 0 {
            return strip_controls(&self.pr.title);
        }
        self.pr
            .commits
            .get(idx - 1)
            .map(|c| strip_controls(&c.title))
            .unwrap_or_default()
    }

    fn fetch_views(&mut self, idx: usize) -> std::result::Result<Vec<DiffView>, GgrError> {
        if idx == 0 {
            self.state = State::Description;
            let desc = self.pr_description_text();
            return Ok(vec![DiffView::from_description(&desc)]);
        }
        let commit_idx = idx - 1;
        let Some(commit) = self.pr.commits.get(commit_idx) else {
            return Err(GgrError::Io {
                source: std::io::Error::other(format!("commit index {commit_idx} out of range")),
            });
        };
        let sha = commit.sha.clone();
        let title = commit.title.clone();
        // Fetch the diff before updating state so that a failure leaves the
        // previous state intact (the struct never holds an index that
        // disagrees with the loaded diff).
        let diff = gh::fetch_commit_diff(&self.pr.repo_name, &sha, self.pr.hostname.as_deref())?;
        let mut views = Vec::with_capacity(diff.files.len() + 1);
        views.push(DiffView::from_description(&strip_controls(&title)));
        for file in &diff.files {
            views.push(DiffView::from_file(file));
        }
        self.state = State::CommitDiff { index: idx, diff };
        Ok(views)
    }

    fn inline_comments_for_view(
        &self,
        now: std::time::SystemTime,
        view_idx: usize,
        severity_filter: Option<Severity>,
    ) -> Vec<InlineComment> {
        if !self.threads_expanded {
            return Vec::new();
        }
        let diff = match &self.state {
            State::Description => return Vec::new(),
            State::CommitDiff { diff, .. } => diff,
        };
        if view_idx == 0 {
            return Vec::new();
        }
        let file_idx = view_idx - 1;
        let Some(file) = diff.files.get(file_idx) else {
            return Vec::new();
        };
        let file_path_str = file.display_path().to_string_lossy();
        self.pr
            .review_threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.path == file_path_str.as_ref() && !t.is_outdated())
            .filter_map(|(comment_index, thread)| {
                if let Some(f) = severity_filter {
                    if thread.severity != f {
                        return None;
                    }
                }
                Some(InlineComment {
                    source_line: thread.original_line,
                    target_line: thread.line,
                    severity: thread.severity,
                    age: local_review_core::util::format_age_from_iso_str(
                        now,
                        &thread.root.created_at,
                    ),
                    body_lines: strip_controls_preserve_newlines(&thread.root.body)
                        .lines()
                        .map(str::to_owned)
                        .collect(),
                    comment_index,
                })
            })
            .collect()
    }

    fn save_comment(
        &mut self,
        _req: SaveRequest<'_>,
    ) -> std::result::Result<SaveOutcome, GgrError> {
        Ok(SaveOutcome::Refused {
            reason: "ggr is read-only in this version".to_owned(),
        })
    }

    fn update_comment(
        &mut self,
        _req: UpdateRequest<'_>,
    ) -> std::result::Result<SaveOutcome, GgrError> {
        Ok(SaveOutcome::Refused {
            reason: "ggr is read-only in this version".to_owned(),
        })
    }

    fn delete_comment(
        &mut self,
        _req: DeleteRequest,
    ) -> std::result::Result<DeleteOutcome, GgrError> {
        Ok(DeleteOutcome::Refused {
            reason: "ggr is read-only; comments cannot be deleted".to_owned(),
        })
    }

    fn is_view_reviewed(&self, _view_idx: usize) -> bool {
        false
    }

    fn mark_view_reviewed(&mut self, _view_idx: usize) -> MarkReviewedOutcome {
        MarkReviewedOutcome::NotTracked
    }

    fn toggle_view_reviewed(&mut self, _view_idx: usize) -> ReviewedOutcome {
        ReviewedOutcome::NotTracked
    }

    fn severity_histogram(&self) -> SeverityHistogram {
        SeverityHistogram::default()
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "unhandled KeyCode variants are intentionally passed through as Ignored"
    )]
    fn handle_extra_key(&mut self, key: KeyEvent) -> std::result::Result<ExtraKeyAction, GgrError> {
        match key.code {
            KeyCode::Char('T') => {
                self.threads_expanded = !self.threads_expanded;
                let msg = if self.threads_expanded {
                    THREADS_EXPANDED_MSG
                } else {
                    THREADS_COLLAPSED_MSG
                };
                Ok(ExtraKeyAction::StatusMessage(msg.to_owned()))
            }
            _ => Ok(ExtraKeyAction::Ignored),
        }
    }

    fn render_extra_screen(&self, _frame: &mut Frame<'_>, _state: &mut dyn ExtraScreen) {}

    fn handle_extra_screen_key(
        &mut self,
        _state: &mut dyn ExtraScreen,
        _key: KeyEvent,
        _ctx: &mut ExtraScreenContext<'_>,
    ) -> std::result::Result<ExtraScreenAction, GgrError> {
        Ok(ExtraScreenAction::Close)
    }

    fn file_picker_entries(&self) -> Vec<FilePickerEntry> {
        match &self.state {
            State::Description => file_picker::build_entries(&[], &|_| 0, &|_| false, &|_| 0),
            State::CommitDiff { diff, .. } => file_picker::build_entries(
                &diff.files,
                &|_view_idx| 0,
                &|_view_idx| false,
                &|_view_idx| 0,
            ),
        }
    }

    fn help_screen_title(&self) -> &'static str {
        "ggr"
    }
}

// ── ReviewSurfaceExt impl ─────────────────────────────────────────────────────

impl ReviewSurfaceExt for GgrSurface {
    fn on_entry_loaded(&mut self, _idx: usize, _record_cursor: bool) {}

    fn severity_histogram_for_transition(&self) -> (Option<usize>, SeverityHistogram) {
        (Some(0), SeverityHistogram::default())
    }
}

// ── terminal setup / teardown ─────────────────────────────────────────────────

struct TuiGuard;

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

fn enter_tui() -> Result<(Terminal<CrosstermBackend<Stdout>>, TuiGuard)> {
    enable_raw_mode().map_err(|source| GgrError::Io { source })?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).map_err(|source| GgrError::Io { source })?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).map_err(|e| GgrError::Io {
        source: std::io::Error::other(e),
    })?;
    Ok((terminal, TuiGuard))
}

// ── entry point ───────────────────────────────────────────────────────────────

pub(crate) fn run(pr: PrDetails) -> Result<()> {
    let size = crossterm::terminal::size().map_err(|source| GgrError::Io { source })?;
    if size.0 < MIN_COLS {
        return Err(GgrError::TerminalTooNarrow { cols: size.0 });
    }
    if size.1 < MIN_ROWS {
        return Err(GgrError::TerminalTooShort { rows: size.1 });
    }
    let surface = GgrSurface::new(pr);
    let mut app = App::new(surface, vec![], TransitionMode::Auto);
    let (mut terminal, _guard) = enter_tui()?;
    core_run_app(&mut terminal, &mut app, |_| {}).map_err(|e| match e {
        AppError::Io(source) => GgrError::Io { source },
        AppError::Surface(e) => e,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::{CommitEntry, CommitSha, PrComment, RepoName, ReviewThread, ThreadComment};
    use crossterm::event::{KeyCode, KeyModifiers};
    use local_review_core::diff::{Diff, DiffFile};
    use local_review_core::tui::{composer::ComposerScope, CommentId};

    fn make_pr_zero_commits() -> PrDetails {
        let mut pr = make_pr();
        pr.commits.clear();
        pr
    }

    fn make_pr() -> PrDetails {
        PrDetails {
            number: 42,
            title: "PR title".to_owned(),
            body: "PR body".to_owned(),
            comments: vec![],
            repo_name: RepoName::try_from("owner/repo").unwrap(),
            hostname: None,
            commits: vec![
                CommitEntry {
                    sha: CommitSha::try_from("a3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b4").unwrap(),
                    short_sha: "a3b4c5d6".to_owned(),
                    title: "First commit".to_owned(),
                },
                CommitEntry {
                    sha: CommitSha::try_from("b3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b4").unwrap(),
                    short_sha: "b3b4c5d6".to_owned(),
                    title: "Second commit".to_owned(),
                },
            ],
            review_threads: vec![],
        }
    }

    #[test]
    fn entry_count_includes_description_entry() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_count(), 3, "2 commits + 1 description entry");
    }

    #[test]
    fn entry_count_single_commit() {
        let mut pr = make_pr();
        pr.commits.truncate(1);
        let surface = GgrSurface::new(pr);
        assert_eq!(surface.entry_count(), 2);
    }

    #[test]
    fn entry_id_display_returns_pr_number_for_index_0() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_id_display(0), "#42");
    }

    #[test]
    fn entry_id_display_returns_short_sha_for_commits() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_id_display(1), "a3b4c5d6");
        assert_eq!(surface.entry_id_display(2), "b3b4c5d6");
    }

    #[test]
    fn entry_id_display_out_of_range_returns_empty() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_id_display(99), "");
    }

    #[test]
    fn entry_description_returns_pr_title_for_index_0() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_description(0), "PR title");
    }

    #[test]
    fn entry_description_returns_commit_title() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_description(1), "First commit");
    }

    #[test]
    fn entry_description_strips_control_chars_from_pr_title() {
        let mut pr = make_pr();
        pr.title = "\x1b[31mevil\x1b[0m".to_owned();
        let surface = GgrSurface::new(pr);
        let desc = surface.entry_description(0);
        assert!(
            !desc.chars().any(char::is_control),
            "entry_description must strip control chars; got: {desc:?}"
        );
    }

    #[test]
    fn entry_description_out_of_range_returns_empty() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.entry_description(99), "");
    }

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_thread(
        path: &str,
        line: Option<u32>,
        position: Option<u32>,
        body: &str,
        created_at: &str,
    ) -> ReviewThread {
        ReviewThread {
            path: path.to_owned(),
            position,
            original_commit_id: CommitSha::try_from("a3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b4")
                .unwrap(),
            root: ThreadComment {
                id: 1,
                author: "reviewer".to_owned(),
                created_at: created_at.to_owned(),
                body: body.to_owned(),
            },
            replies: vec![],
            line,
            original_line: None,
            diff_side: None,
            severity: Severity::Note,
        }
    }

    fn make_surface_with_thread(thread: ReviewThread) -> GgrSurface {
        let mut pr = make_pr();
        pr.review_threads.push(thread);
        let mut surface = GgrSurface::new(pr);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        surface
    }

    #[test]
    fn inline_comments_for_view_returns_empty() {
        let surface = GgrSurface::new(make_pr());
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 0, None)
            .is_empty());
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
            .is_empty());
    }

    #[test]
    fn threads_expanded_defaults_to_true() {
        let surface = GgrSurface::new(make_pr());
        assert!(
            surface.threads_expanded,
            "threads_expanded must be true after new()"
        );
    }

    #[test]
    fn handle_extra_key_t_expands_then_collapses() {
        let mut surface = GgrSurface::new(make_pr());
        assert!(surface.threads_expanded);

        let result = surface
            .handle_extra_key(make_key(KeyCode::Char('T')))
            .unwrap();
        assert!(!surface.threads_expanded, "T must collapse when expanded");
        assert!(
            matches!(result, ExtraKeyAction::StatusMessage(_)),
            "T must return StatusMessage"
        );

        let result = surface
            .handle_extra_key(make_key(KeyCode::Char('T')))
            .unwrap();
        assert!(surface.threads_expanded, "T must expand when collapsed");
        assert!(
            matches!(result, ExtraKeyAction::StatusMessage(_)),
            "T must return StatusMessage"
        );
    }

    #[test]
    fn handle_extra_key_unknown_returns_ignored() {
        let mut surface = GgrSurface::new(make_pr());
        let result = surface
            .handle_extra_key(make_key(KeyCode::Char('x')))
            .unwrap();
        assert!(
            matches!(result, ExtraKeyAction::Ignored),
            "unhandled key must return Ignored"
        );
    }

    #[test]
    fn inline_comments_for_view_returns_empty_when_collapsed() {
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let mut surface = make_surface_with_thread(thread);
        surface.threads_expanded = false;
        assert!(
            surface
                .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
                .is_empty(),
            "collapsed threads must yield no inline comments"
        );
    }

    #[test]
    fn inline_comments_for_view_returns_empty_in_description_state() {
        let surface = GgrSurface::new(make_pr());
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 0, None)
            .is_empty());
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
            .is_empty());
    }

    #[test]
    fn inline_comments_for_view_returns_empty_for_view_idx_zero_in_commit_diff() {
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        assert!(
            surface
                .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 0, None)
                .is_empty(),
            "view_idx 0 is the commit description page; no inline threads anchor there"
        );
    }

    #[test]
    fn inline_comments_for_view_returns_empty_for_out_of_range_view_idx() {
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        assert!(
            surface
                .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 2, None)
                .is_empty(),
            "out-of-range view_idx must return empty"
        );
    }

    #[test]
    fn inline_comments_for_view_maps_thread_to_inline_comment() {
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "good stuff",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        // 2024-01-15T10:30:00Z = 1_705_314_600 secs; 3 days later = 1_705_573_800.
        let ts_plus_3d: u64 = 1_705_573_800;
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(ts_plus_3d);
        let comments = surface.inline_comments_for_view(now, 1, None);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].target_line, Some(10));
        assert_eq!(comments[0].source_line, None);
        assert!(
            comments[0].age.ends_with("days ago"),
            "age for a years-old timestamp must be in days bucket; got: {:?}",
            comments[0].age
        );
        assert_eq!(comments[0].body_lines, vec!["good stuff"]);
    }

    #[test]
    fn inline_comments_for_view_comment_index_is_global_review_threads_index() {
        let thread_other = make_thread(
            "src/bar.rs",
            Some(5),
            Some(1),
            "other file",
            "2024-01-15T10:30:00Z",
        );
        let thread_match = make_thread(
            "src/foo.rs",
            Some(10),
            Some(2),
            "matching file",
            "2024-01-15T11:00:00Z",
        );
        let mut pr = make_pr();
        pr.review_threads.push(thread_other);
        pr.review_threads.push(thread_match);
        let mut surface = GgrSurface::new(pr);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        let comments = surface.inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None);
        assert_eq!(
            comments.len(),
            1,
            "only the matching thread must be returned"
        );
        assert_eq!(
            comments[0].comment_index, 1,
            "comment_index must be the global index in review_threads (1), not the filtered-slice index (0)"
        );
    }

    #[test]
    fn inline_comments_for_view_skips_mismatched_file() {
        // Thread is on src/bar.rs; diff file is src/foo.rs.
        let thread = make_thread(
            "src/bar.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        assert!(
            surface
                .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
                .is_empty(),
            "thread on different file must be skipped"
        );
    }

    #[test]
    fn inline_comments_for_view_skips_outdated_threads() {
        // position: None means is_outdated() == true
        let thread = make_thread("src/foo.rs", None, None, "body", "2024-01-15T10:30:00Z");
        let surface = make_surface_with_thread(thread);
        assert!(
            surface
                .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
                .is_empty(),
            "outdated thread (position: None) must be skipped"
        );
    }

    #[test]
    fn inline_comments_for_view_severity_filter_excludes_note() {
        // GitHub threads are Note; a Required filter must exclude them.
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        assert!(
            surface
                .inline_comments_for_view(
                    std::time::SystemTime::UNIX_EPOCH,
                    1,
                    Some(Severity::Required)
                )
                .is_empty(),
            "Required filter must exclude Note-severity GitHub threads"
        );
    }

    #[test]
    fn inline_comments_for_view_suggestion_filter_excludes_note_thread() {
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        assert!(
            surface
                .inline_comments_for_view(
                    std::time::SystemTime::UNIX_EPOCH,
                    1,
                    Some(Severity::Suggestion)
                )
                .is_empty(),
            "Suggestion filter must exclude Note-severity GitHub threads"
        );
    }

    #[test]
    fn inline_comments_for_view_severity_filter_note_includes_thread() {
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        assert_eq!(
            surface
                .inline_comments_for_view(
                    std::time::SystemTime::UNIX_EPOCH,
                    1,
                    Some(Severity::Note)
                )
                .len(),
            1,
            "Note filter must include Note-severity threads"
        );
    }

    #[test]
    fn inline_comments_for_view_non_note_severity_filter_includes_matching_thread() {
        let mut thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "body",
            "2024-01-15T10:30:00Z",
        );
        thread.severity = Severity::Required;
        let surface = make_surface_with_thread(thread);
        let comments = surface.inline_comments_for_view(
            std::time::SystemTime::UNIX_EPOCH,
            1,
            Some(Severity::Required),
        );
        assert_eq!(
            comments.len(),
            1,
            "Required filter must include Required-severity thread"
        );
        assert_eq!(
            comments[0].severity,
            Severity::Required,
            "InlineComment.severity must match thread.severity"
        );
    }

    #[test]
    fn inline_comments_for_view_strips_control_chars_from_body() {
        let thread = make_thread(
            "src/foo.rs",
            Some(5),
            Some(1),
            "\x1b[31mevil\x1b[0m\ngood",
            "2024-01-15T10:30:00Z",
        );
        let surface = make_surface_with_thread(thread);
        let comments = surface.inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None);
        assert_eq!(comments.len(), 1);
        let body = &comments[0].body_lines;
        assert!(
            body.iter().all(|l| !l.chars().any(char::is_control)),
            "body_lines must contain no control characters; got: {body:?}"
        );
        assert_eq!(body.len(), 2, "newline in body must produce two lines");
    }

    #[test]
    fn inline_comments_for_view_age_fallback_strips_control_chars() {
        let thread = make_thread(
            "src/foo.rs",
            Some(5),
            Some(1),
            "body",
            "\x1b[31m2024-01-15T10:30:00Z\x1b[0m",
        );
        let surface = make_surface_with_thread(thread);
        let comments = surface.inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None);
        assert_eq!(comments.len(), 1);
        assert!(
            !comments[0].age.chars().any(char::is_control),
            "age must contain no control characters; got: {:?}",
            comments[0].age
        );
    }

    #[test]
    fn inline_comments_for_view_empty_review_threads_returns_empty() {
        let mut surface = GgrSurface::new(make_pr());
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        assert!(
            surface
                .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
                .is_empty(),
            "empty review_threads must yield no comments"
        );
    }

    #[test]
    fn save_comment_returns_refused() {
        let mut surface = GgrSurface::new(make_pr());
        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "test comment",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Refused { .. }),
            "save_comment must return Refused in read-only ggr"
        );
    }

    #[test]
    fn update_comment_returns_refused() {
        let mut surface = GgrSurface::new(make_pr());
        let identity = CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = UpdateRequest {
            identity,
            body: "updated body",
            severity: Severity::Note,
            oversized: false,
        };
        let outcome = surface.update_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Refused { .. }),
            "update_comment must return Refused in read-only ggr"
        );
    }

    #[test]
    fn is_view_reviewed_returns_false() {
        let surface = GgrSurface::new(make_pr());
        assert!(!surface.is_view_reviewed(0));
        assert!(!surface.is_view_reviewed(1));
    }

    #[test]
    fn severity_histogram_returns_default() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.severity_histogram(), SeverityHistogram::default());
    }

    #[test]
    fn help_screen_title_is_ggr() {
        let surface = GgrSurface::new(make_pr());
        assert_eq!(surface.help_screen_title(), "ggr");
    }

    #[test]
    fn pr_description_text_includes_title_body_and_comments() {
        let mut pr = make_pr();
        pr.comments.push(PrComment {
            author: "alice".to_owned(),
            body: "great PR!".to_owned(),
        });
        let surface = GgrSurface::new(pr);
        let text = surface.pr_description_text();
        assert!(text.contains("PR title"), "must contain title");
        assert!(text.contains("PR body"), "must contain body");
        assert!(text.contains("alice"), "must contain comment author");
        assert!(text.contains("great PR!"), "must contain comment body");
    }

    #[test]
    fn pr_description_text_strips_control_chars() {
        let mut pr = make_pr();
        pr.comments.push(PrComment {
            author: "alice".to_owned(),
            body: "\x1b[31mmalicious\x1b[0m content".to_owned(),
        });
        let surface = GgrSurface::new(pr);
        let text = surface.pr_description_text();
        let has_non_newline_control = text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r');
        assert!(
            !has_non_newline_control,
            "pr_description_text must strip ESC/control chars; got: {text:?}"
        );
    }

    #[test]
    fn pr_description_text_preserves_newlines_in_body() {
        let mut pr = make_pr();
        pr.body = "line one\nline two".to_owned();
        pr.comments.push(PrComment {
            author: "alice".to_owned(),
            body: "first line\nsecond line".to_owned(),
        });
        let surface = GgrSurface::new(pr);
        let text = surface.pr_description_text();
        assert!(text.contains("line one"), "body line one must be present");
        assert!(text.contains("line two"), "body line two must be present");
        let body_start = text.find("line one").expect("line one present");
        let body_slice = &text[body_start..];
        assert!(
            body_slice.contains('\n'),
            "newline between body lines must be preserved; got: {text:?}"
        );
    }

    #[test]
    fn file_picker_entries_for_description_page_returns_one_entry() {
        let surface = GgrSurface::new(make_pr());
        let entries = surface.file_picker_entries();
        assert_eq!(
            entries.len(),
            1,
            "description page must yield exactly one file-picker entry"
        );
        assert_eq!(entries[0].view_index, 0, "entry must map to view index 0");
    }

    #[test]
    fn fetch_views_index_zero_returns_description_view() {
        let mut surface = GgrSurface::new(make_pr());
        let views = surface.fetch_views(0).expect("fetch_views(0) must succeed");
        assert_eq!(
            views.len(),
            1,
            "description page must yield exactly one view"
        );
        assert!(
            views[0].lines.iter().any(|l| l.text.contains("PR title")),
            "description view content must contain PR title; got: {:?}",
            views[0].lines.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fetch_views_out_of_range_returns_err() {
        let mut surface = GgrSurface::new(make_pr());
        let count = surface.entry_count();
        let result = surface.fetch_views(count);
        assert!(result.is_err(), "out-of-range fetch_views must return Err");
    }

    #[test]
    fn pr_description_text_with_empty_body_has_no_extra_newlines() {
        let mut pr = make_pr();
        pr.body = String::new();
        let surface = GgrSurface::new(pr);
        let text = surface.pr_description_text();
        assert_eq!(
            text, "PR title",
            "empty body must not add trailing newlines"
        );
    }

    #[test]
    fn delete_comment_returns_refused() {
        let mut surface = GgrSurface::new(make_pr());
        let identity = CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = DeleteRequest::new(identity, None);
        let result = surface
            .delete_comment(req)
            .expect("delete_comment must not error");
        assert!(
            matches!(result, DeleteOutcome::Refused { .. }),
            "delete_comment must return Refused in read-only ggr"
        );
        if let DeleteOutcome::Refused { reason } = result {
            assert!(
                reason.contains("read-only"),
                "expected 'read-only' in refused reason; got: {reason}"
            );
        }
    }

    #[test]
    fn entry_count_zero_commits_returns_one() {
        let mut pr = make_pr();
        pr.commits.clear();
        let surface = GgrSurface::new(pr);
        assert_eq!(
            surface.entry_count(),
            1,
            "description entry exists even with no commits"
        );
    }

    #[test]
    fn fetch_views_zero_resets_to_description_page() {
        let mut surface = GgrSurface::new(make_pr());
        // Start in CommitDiff state to prove that fetch_views(0) actually resets it.
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff { files: vec![] },
        };
        let views = surface.fetch_views(0).expect("fetch_views(0) must succeed");
        assert_eq!(views.len(), 1, "description page must yield one view");
        assert!(
            matches!(surface.state, State::Description),
            "state must be reset to Description after fetch_views(0)"
        );
        let entries = surface.file_picker_entries();
        assert_eq!(
            entries.len(),
            1,
            "file picker must show description entry after reset"
        );
    }

    #[test]
    fn fetch_views_on_zero_commit_pr_returns_err() {
        let mut surface = GgrSurface::new(make_pr_zero_commits());
        let count = surface.entry_count();
        assert_eq!(count, 1, "zero-commit PR has one entry (description only)");
        let result = surface.fetch_views(1);
        assert!(
            result.is_err(),
            "fetch_views(1) on a zero-commit PR must return Err (out of range)"
        );
    }

    #[test]
    fn fetch_views_out_of_range_leaves_state_intact() {
        let mut surface = GgrSurface::new(make_pr());
        // Pre-load a CommitDiff state to prove it is not clobbered on Err.
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff { files: vec![] },
        };
        let result = surface.fetch_views(99);
        assert!(result.is_err(), "out-of-range index must return Err");
        assert!(
            matches!(surface.state, State::CommitDiff { index: 1, .. }),
            "state must remain CommitDiff{{index:1}} after failed fetch_views; \
             struct must never hold an index that disagrees with the loaded diff"
        );
    }

    #[test]
    fn toggle_view_reviewed_returns_not_tracked() {
        let mut surface = GgrSurface::new(make_pr());
        assert_eq!(
            surface.toggle_view_reviewed(0),
            ReviewedOutcome::NotTracked,
            "ggr does not track reviewed state; toggle must return NotTracked"
        );
    }

    #[test]
    fn file_picker_entries_for_commit_diff_state_returns_description_plus_files() {
        let mut surface = GgrSurface::new(make_pr());
        let diff = Diff {
            files: vec![
                DiffFile::Modified {
                    path: std::path::PathBuf::from("a.rs"),
                    hunks: vec![],
                },
                DiffFile::Modified {
                    path: std::path::PathBuf::from("b.rs"),
                    hunks: vec![],
                },
            ],
        };
        surface.state = State::CommitDiff { index: 1, diff };
        let entries = surface.file_picker_entries();
        assert_eq!(
            entries.len(),
            3,
            "CommitDiff with 2 files must yield 1 description + 2 file entries; got {} entries",
            entries.len()
        );
        assert_eq!(
            entries[0].view_index, 0,
            "first entry must map to view index 0 (description)"
        );
        assert_eq!(
            entries[1].view_index, 1,
            "second entry must map to view index 1 (first file)"
        );
    }

    #[test]
    fn entry_description_strips_control_chars_from_commit_title() {
        let mut pr = make_pr();
        pr.commits[0].title = "\x1b[31mevil\x1b[0m".to_owned();
        let surface = GgrSurface::new(pr);
        let desc = surface.entry_description(1);
        assert!(
            !desc.chars().any(char::is_control),
            "entry_description must strip control chars from commit title; got: {desc:?}"
        );
    }
}

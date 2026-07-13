//! Terminal UI entry point for ggr: wires `PrDetails` into the shared
//! `App<GgrSurface>` review loop from `local-review-core`.

use std::io::{stdout, Stdout, Write as _};

use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Flex, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line as TuiLine;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use local_review_core::change_id::ChangeId;
use local_review_core::comment::Side;
use local_review_core::revset_hash::RevsetHash;
use local_review_core::tui::composer::{
    handle_composer_key, Composer, ComposerAction, ComposerInit, ComposerScope, EditedComment,
    LineTarget, StackContextSnapshot,
};
use local_review_core::tui::composer_overlay;
use local_review_core::tui::diff_view::InlineComment;
use local_review_core::tui::try_downcast_mut;
use local_review_core::tui::{
    collect_context, file_picker, run_app as core_run_app, App, AppError, CommentId, CommentIndex,
    ComposerScreen, DeleteOutcome, DeleteRequest, DiffView, ExtraKeyAction, ExtraScreen,
    ExtraScreenAction, ExtraScreenContext, FilePickerEntry, MarkReviewedOutcome, RenderedLineKind,
    ReviewSurface, ReviewSurfaceExt, ReviewedOutcome, SaveOutcome, SaveRequest, SeverityHistogram,
    TransitionMode, UpdateRequest, MIN_COLS, MIN_ROWS,
};
use local_review_core::util::{strip_controls, strip_controls_preserve_newlines};
use local_review_core::Severity;

use crate::cursor;
use crate::error::{GgrError, Result};
use crate::gh;
use crate::pr::PrDetails;

// ── constants ─────────────────────────────────────────────────────────────────

const THREADS_EXPANDED_MSG: &str = "threads expanded";
const THREADS_COLLAPSED_MSG: &str = "threads collapsed";

/// Maximum body size accepted without the "body truncated" warning. 64 KiB
/// matches the GitHub API limit for review comment bodies.
const DRAFT_BODY_MAX: usize = 65_536;

// ── GgrSurface ────────────────────────────────────────────────────────────────

/// Which pane is active for entry 0 (the PR overview).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrPane {
    /// Full PR description body as scrollable text (default).
    Description,
    /// Aggregated entity list across all commits.
    Entities,
}

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
/// Lifecycle of the eager repo clone that feeds `build_graph`. Shared
/// between the TUI thread (readers) and the background clone thread (one
/// writer) via `Arc<Mutex<_>>`.
#[derive(Debug)]
pub(crate) enum GraphCloneState {
    /// Clone thread is running (or about to). Graphs are unavailable but
    /// may become available; tiers degrade with a "clone in progress" note.
    InProgress,
    /// Clone checked out at the PR head SHA; graphs build from this path.
    Ready(std::path::PathBuf),
    /// Clone will not happen this session — opt-out or failure. The string
    /// is the reason rendered in the degraded-tiers notice.
    Unavailable(String),
}

impl GraphCloneState {
    fn from_status(status: crate::repo_cache::CloneStatus) -> Self {
        match status {
            crate::repo_cache::CloneStatus::Ready(p) => Self::Ready(p),
            crate::repo_cache::CloneStatus::Disabled(r)
            | crate::repo_cache::CloneStatus::Failed(r) => Self::Unavailable(r),
        }
    }
}

pub(crate) struct GgrSurface {
    pr: PrDetails,
    state: State,
    threads_expanded: bool,
    /// Consumed on the first `fetch_views` call; after that, `state` is the
    /// authoritative index source and this stays `None` to prevent
    /// `current_entry_index` returning a stale value.
    pending_initial_index: Option<usize>,
    /// Consumed once by `initial_view_position` so the initial scroll position
    /// is applied exactly once on first render; subsequent calls return `(0, 0)`.
    pending_cursor: Option<(String, usize)>,
    /// Local draft comments for the currently loaded commit entry.
    /// Refreshed on each `fetch_views` call and after each save/update/delete.
    loaded_drafts: Vec<crate::draft::GgrDraft>,
    /// Pending reply drafts for the current PR.
    /// Refreshed alongside `loaded_drafts`.
    loaded_replies: Vec<crate::draft::GgrReply>,
    /// Last severity used, so new composers default to the same severity.
    last_severity: Option<Severity>,
    /// Shown once on the first entry load if stale drafts were found on open.
    pending_stale_message: Option<String>,
    /// Verdict chosen in the verdict modal; executed by `poll_immediate_action`
    /// on the first draw tick after the submitting overlay is visible.
    pending_submit: Option<crate::submit::Verdict>,
    /// Eager clone lifecycle; written by the background thread spawned in
    /// [`GgrSurface::start_graph_clone`], read wherever a graph is wanted.
    /// Opt-outs (`--no-graph`, `GGR_NO_GRAPH_CLONE=1`) are folded in at
    /// construction as `Unavailable`, so this is the single source of truth
    /// for graph availability.
    graph_clone: std::sync::Arc<std::sync::Mutex<GraphCloneState>>,
    /// Active pane for entry 0. Persists across `n`/`p` navigation so the
    /// reviewer's pane preference is remembered within a session.
    pr_pane: PrPane,
    /// Maps each position in the aggregated PR entity list back to the
    /// commit index (1-based entry index) that last modified that entity.
    /// Populated by `fetch_entity_list(0)` in entity pane mode; used by
    /// `fetch_entity_diff(0, entity_idx, ...)` to load the right commit diff.
    pr_entity_commit_indices: Vec<usize>,
}

impl GgrSurface {
    pub(crate) fn new(
        pr: PrDetails,
        initial_cursor: Option<&cursor::CursorState>,
        allow_graph_clone: bool,
    ) -> Self {
        let restored_index = initial_cursor.and_then(|c| {
            pr.commits
                .iter()
                .position(|commit| commit.sha.as_str() == c.commit_sha)
                .map(|pos| pos + 1)
        });
        let pending_cursor = restored_index
            .and(initial_cursor)
            .map(|c| (strip_controls(&c.file), c.line));
        // Default landing is the first commit's entity list (entry 1), not the
        // PR description page (entry 0), so the reviewer opens directly into
        // per-commit entity review — consistent with jjr's entity-first entry.
        // A restored cursor wins; a PR with no commits falls back to entry 0
        // (the only entry that exists). Entry 0 remains reachable via `p`.
        let pending_initial_index =
            restored_index.or_else(|| (!pr.commits.is_empty()).then_some(1));
        // Fold opt-outs and the no-commits edge into the clone state up
        // front; `start_graph_clone` only spawns when this is InProgress.
        let no_clone_env = std::env::var_os(crate::repo_cache::NO_CLONE_ENV_VAR);
        let clone_state =
            match crate::repo_cache::opt_out_reason(allow_graph_clone, no_clone_env.as_deref()) {
                Some(reason) => GraphCloneState::Unavailable(reason),
                None if pr.commits.is_empty() => {
                    GraphCloneState::Unavailable("PR has no commits".to_owned())
                }
                None => GraphCloneState::InProgress,
            };
        Self {
            pr,
            state: State::Description,
            threads_expanded: true,
            pending_initial_index,
            pending_cursor,
            loaded_drafts: Vec::new(),
            loaded_replies: Vec::new(),
            last_severity: None,
            pending_stale_message: None,
            pending_submit: None,
            graph_clone: std::sync::Arc::new(std::sync::Mutex::new(clone_state)),
            pr_pane: PrPane::Description,
            pr_entity_commit_indices: Vec::new(),
        }
    }

    /// Kick off the eager background clone at the PR head SHA. Called once
    /// at PR open; a no-op when the clone state was resolved at
    /// construction (opt-out, zero commits). The thread is detached — its
    /// only side effect is the state write, and abandoning it at process
    /// exit is harmless.
    pub(crate) fn start_graph_clone(&self) {
        {
            let Ok(state) = self.graph_clone.lock() else {
                return;
            };
            if !matches!(*state, GraphCloneState::InProgress) {
                return;
            }
        }
        // Head = last commit (commits are oldest-first).
        let Some(head_sha) = self.pr.commits.last().map(|c| c.sha.as_str().to_owned()) else {
            return;
        };
        let owner_repo = self.pr.repo_name.as_str().to_owned();
        let hostname = self.pr.hostname.clone();
        let pr_number = self.pr.number;
        let shared = std::sync::Arc::clone(&self.graph_clone);
        std::thread::spawn(move || {
            let status = crate::repo_cache::ensure_clone_at(&crate::repo_cache::CloneRequest {
                owner_repo: &owner_repo,
                hostname: hostname.as_deref(),
                pr_number,
                head_sha: &head_sha,
                allow_clone: true,
                remote_override: None,
                cache_root: None,
            });
            if let Ok(mut state) = shared.lock() {
                *state = GraphCloneState::from_status(status);
            }
        });
    }

    /// Clone path when (and only when) the clone is ready at the PR head.
    fn ready_repo_path(&self) -> Option<std::path::PathBuf> {
        match self.graph_clone.lock() {
            Ok(state) => match &*state {
                GraphCloneState::Ready(p) => Some(p.clone()),
                GraphCloneState::InProgress | GraphCloneState::Unavailable(_) => None,
            },
            Err(_) => None,
        }
    }

    /// Returns `None` when `State::Description` is active.
    /// `file_index == 0` (commit description sub-view) stores `file: ""`.
    pub(crate) fn current_cursor_state(
        &self,
        file_index: usize,
        line_index: usize,
    ) -> Option<cursor::CursorState> {
        let State::CommitDiff { index, ref diff } = self.state else {
            return None;
        };
        let commit_sha = self
            .pr
            .commits
            .get(index.wrapping_sub(1))?
            .sha
            .as_str()
            .to_owned();
        let file = if file_index == 0 {
            String::new()
        } else {
            diff.files
                .get(file_index - 1)
                .map(|f| strip_controls(&f.display_path().to_string_lossy()))
                .unwrap_or_default()
        };
        Some(cursor::CursorState {
            commit_sha,
            file,
            line: line_index,
        })
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

    /// Reload `loaded_drafts` and `loaded_replies` from disk.
    fn reload_drafts(&mut self) {
        let Some(base) = crate::util::data_home() else {
            self.loaded_drafts.clear();
            self.loaded_replies.clear();
            return;
        };
        let State::CommitDiff { index, .. } = &self.state else {
            self.loaded_drafts.clear();
            self.loaded_replies.clear();
            return;
        };
        let Some(commit) = self.pr.commits.get(index.wrapping_sub(1)) else {
            self.loaded_drafts.clear();
            self.loaded_replies.clear();
            return;
        };
        let host = self.pr.hostname.as_deref().unwrap_or("github.com");
        let slug = self.pr.repo_name.as_str();
        let Some((owner, repo)) = slug.split_once('/') else {
            self.loaded_drafts.clear();
            self.loaded_replies.clear();
            return;
        };
        let drafts_dir =
            crate::draft::drafts_dir_from_base(&base, host, owner, repo, self.pr.number);

        let mut all: Vec<crate::draft::GgrDraft> = Vec::new();
        let commit_file = drafts_dir.join(format!("{}.jsonl", commit.sha.as_str()));
        match crate::draft::list_drafts(&commit_file) {
            Ok(ds) => all.extend(ds),
            Err(e) => {
                let mut stderr = std::io::stderr().lock();
                let _ = writeln!(stderr, "ggr: warning: failed to load drafts: {e}");
            }
        }
        let pr_file = drafts_dir.join("_pr.jsonl");
        match crate::draft::list_drafts(&pr_file) {
            Ok(ds) => all.extend(ds),
            Err(e) => {
                let mut stderr = std::io::stderr().lock();
                let _ = writeln!(stderr, "ggr: warning: failed to load PR drafts: {e}");
            }
        }
        self.loaded_drafts = all;

        let replies_file =
            crate::draft::replies_file_from_base(&base, host, owner, repo, self.pr.number);
        match crate::draft::list_replies(&replies_file) {
            Ok(rs) => self.loaded_replies = rs,
            Err(e) => {
                let mut stderr = std::io::stderr().lock();
                let _ = writeln!(stderr, "ggr: warning: failed to load reply drafts: {e}");
                self.loaded_replies.clear();
            }
        }
    }

    /// Return the `CommitSha` for the currently-loaded commit entry, or `None` in
    /// `Description` state.
    fn current_commit_sha(&self) -> Option<&crate::pr::CommitSha> {
        let State::CommitDiff { index, .. } = &self.state else {
            return None;
        };
        self.pr.commits.get(index.wrapping_sub(1)).map(|c| &c.sha)
    }

    /// Build a `ChangeId` from a commit SHA (using the first 8 hex chars).
    ///
    /// `CommitSha` is validated as 40 lowercase hex chars; the first 8 always
    /// satisfy `ChangeId`'s ≥8-char alphanumeric requirement.
    fn commit_change_id(sha: &crate::pr::CommitSha) -> ChangeId {
        match ChangeId::parse(&sha.as_str()[..8]) {
            Ok(id) => id,
            Err(_) => match ChangeId::parse("00000000") {
                Ok(id) => id,
                Err(_) => unreachable!("8-char all-zero hex always satisfies ChangeId invariants"),
            },
        }
    }

    /// Build a `ChangeId` from a PR number (formatted as 16-char lowercase hex).
    fn pr_change_id(pr_number: u64) -> ChangeId {
        match ChangeId::parse(&format!("{pr_number:016x}")) {
            Ok(id) => id,
            Err(_) => match ChangeId::parse("00000000") {
                Ok(id) => id,
                Err(_) => unreachable!("8-char all-zero hex always satisfies ChangeId invariants"),
            },
        }
    }

    /// Open a new line- or commit-scoped composer at the current cursor position.
    fn open_composer_at(
        &self,
        file_index: usize,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let State::CommitDiff { index, ref diff } = self.state else {
            return ExtraKeyAction::StatusMessage("use P to add a PR-scope comment".to_owned());
        };
        let Some(sha) = self.current_commit_sha() else {
            return ExtraKeyAction::StatusMessage("no commit selected".to_owned());
        };
        let change_id = Self::commit_change_id(sha);
        let commit_title = self
            .pr
            .commits
            .get(index.wrapping_sub(1))
            .map(|c| strip_controls(&c.title))
            .unwrap_or_default();

        let line_target = Self::build_line_target(file_index, line_index, diff, current_view);

        let (scope, line_available) = match line_target {
            Some(target) => (ComposerScope::Line(target.clone()), Some(target)),
            None => (ComposerScope::Change, None),
        };

        let stack_available = Some(StackContextSnapshot {
            revset: format!("PR #{}", self.pr.number),
            revset_hash: RevsetHash::from_revset(&format!("pr:{}", self.pr.number)),
        });

        let init = ComposerInit {
            scope,
            severity: self.last_severity.unwrap_or(Severity::Note),
            change_id,
            change_description: commit_title,
            line_available,
            stack_available,
            description_available: None,
        };
        ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(Composer::new(init)))))
    }

    /// Open a commit-scoped composer (key 'm').
    fn open_commit_scope_composer(&self) -> ExtraKeyAction {
        let State::CommitDiff { index, .. } = &self.state else {
            return ExtraKeyAction::StatusMessage(
                "open a commit diff to add a commit comment".to_owned(),
            );
        };
        let Some(sha) = self.current_commit_sha() else {
            return ExtraKeyAction::StatusMessage("no commit selected".to_owned());
        };
        let change_id = Self::commit_change_id(sha);
        let commit_title = self
            .pr
            .commits
            .get(index.wrapping_sub(1))
            .map(|c| strip_controls(&c.title))
            .unwrap_or_default();
        let stack_available = Some(StackContextSnapshot {
            revset: format!("PR #{}", self.pr.number),
            revset_hash: RevsetHash::from_revset(&format!("pr:{}", self.pr.number)),
        });
        let init = ComposerInit {
            scope: ComposerScope::Change,
            severity: self.last_severity.unwrap_or(Severity::Note),
            change_id,
            change_description: commit_title,
            line_available: None,
            stack_available,
            description_available: None,
        };
        ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(Composer::new(init)))))
    }

    /// Open a PR-scoped composer (key 'P').
    fn open_pr_scope_composer(&self) -> ExtraKeyAction {
        let change_id = Self::pr_change_id(self.pr.number);
        let stack = StackContextSnapshot {
            revset: format!("PR #{}", self.pr.number),
            revset_hash: RevsetHash::from_revset(&format!("pr:{}", self.pr.number)),
        };
        let init = ComposerInit {
            scope: ComposerScope::Stack(stack.clone()),
            severity: self.last_severity.unwrap_or(Severity::Note),
            change_id,
            change_description: strip_controls(&self.pr.title),
            line_available: None,
            stack_available: Some(stack),
            description_available: None,
        };
        ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(Composer::new(init)))))
    }

    /// Resolve `change_id` and commit title for edit-mode from current state.
    fn draft_change_id_and_title(&self) -> (ChangeId, String) {
        match self.current_commit_sha() {
            Some(sha) => (
                Self::commit_change_id(sha),
                self.pr
                    .commits
                    .iter()
                    .find(|c| c.sha.as_str() == sha.as_str())
                    .map(|c| strip_controls(&c.title))
                    .unwrap_or_default(),
            ),
            None => (
                Self::pr_change_id(self.pr.number),
                strip_controls(&self.pr.title),
            ),
        }
    }

    /// Open an edit-mode composer for the draft comment at `line_index` (key 'e').
    fn open_edit_composer(
        &self,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let Some(view) = current_view else {
            return ExtraKeyAction::StatusMessage("no view loaded".to_owned());
        };
        let Some(row) = view.lines.get(line_index) else {
            return ExtraKeyAction::StatusMessage("cursor out of bounds".to_owned());
        };
        let RenderedLineKind::InlineCommentMeta { comment_index } = row.kind else {
            return ExtraKeyAction::StatusMessage("cursor is not on a comment".to_owned());
        };
        let draft_idx = match comment_index {
            CommentIndex::Local(idx) => idx,
            CommentIndex::LocalReply(idx) => {
                return self.open_edit_reply_composer(idx, line_index);
            }
            CommentIndex::GitHubThread(_) => {
                return ExtraKeyAction::StatusMessage(
                    "GitHub review threads cannot be edited locally".to_owned(),
                );
            }
        };
        let Some(draft) = self.loaded_drafts.get(draft_idx) else {
            return ExtraKeyAction::StatusMessage(
                "draft not found — navigate away and back to refresh".to_owned(),
            );
        };

        let (change_id, commit_title) = self.draft_change_id_and_title();
        let stack_available = Some(StackContextSnapshot {
            revset: format!("PR #{}", self.pr.number),
            revset_hash: RevsetHash::from_revset(&format!("pr:{}", self.pr.number)),
        });

        let scope = draft_anchor_to_scope(&draft.anchor, line_index, stack_available.as_ref());

        // Parse the draft's created_at as the edit identity.
        let Ok(identity_dt) = time::OffsetDateTime::parse(
            &draft.created_at,
            &time::format_description::well_known::Rfc3339,
        ) else {
            return ExtraKeyAction::StatusMessage(
                "draft has invalid timestamp — cannot edit".to_owned(),
            );
        };

        let init = ComposerInit {
            scope,
            severity: draft.severity,
            change_id,
            change_description: commit_title,
            line_available: None,
            stack_available,
            description_available: None,
        };
        let edited = EditedComment {
            init,
            body: draft.body.clone(),
            identity: identity_dt,
            comment_index: Some(draft_idx),
        };
        ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(Composer::for_edit(
            edited,
        )))))
    }

    /// Build a `LineTarget` from the annotated view at the given cursor position.
    fn build_line_target(
        file_index: usize,
        line_index: usize,
        diff: &local_review_core::diff::Diff,
        current_view: Option<&DiffView>,
    ) -> Option<LineTarget> {
        let view = current_view?;
        let row = view.lines.get(line_index)?;
        // Only diff content kinds are commentable.
        match row.kind {
            RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context => {}
            RenderedLineKind::HunkHeader
            | RenderedLineKind::HunkSeparator
            | RenderedLineKind::Notice
            | RenderedLineKind::InlineCommentMeta { .. }
            | RenderedLineKind::InlineCommentBody
            | RenderedLineKind::DescriptionLine => return None,
        }
        // file_index 0 is the commit description sub-view — no diff file.
        let diff_file_idx = file_index.checked_sub(1)?;
        let diff_file = diff.files.get(diff_file_idx)?;

        let (context_before, context_after) = collect_context(&view.lines, line_index);

        Some(LineTarget {
            file: diff_file.display_path().to_path_buf(),
            rendered_index: line_index,
            source_line: row.source_line,
            target_line: row.target_line,
            target_text: row.text.clone(),
            hunk_header: row.hunk_header.clone().unwrap_or_default(),
            context_before,
            context_after,
        })
    }

    /// Handle composer key dispatch and save/delete.
    fn handle_composer_key_impl(
        &mut self,
        composer: &mut Composer,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        match handle_composer_key(composer, key) {
            ComposerAction::Continue => ExtraScreenAction::StayOpen,
            ComposerAction::Cancel => ExtraScreenAction::Close,
            ComposerAction::Save => match self.save_via_composer(composer) {
                Ok(msg) => {
                    self.last_severity = Some(composer.severity());
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::Close
                }
                Err(msg) => {
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::StayOpen
                }
            },
            ComposerAction::Delete => match self.delete_via_composer(composer) {
                Ok(()) => ExtraScreenAction::Close,
                Err(msg) => {
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::StayOpen
                }
            },
            ComposerAction::RefusedScopeChord(status) => {
                *ctx.status_message = Some(status.to_owned());
                ExtraScreenAction::StayOpen
            }
        }
    }

    /// Route save through the surface's `save_comment` / `update_comment`.
    fn save_via_composer(&mut self, composer: &Composer) -> std::result::Result<String, String> {
        let body = composer.body_text();
        if body.trim().is_empty() {
            return Err("comment body is empty — not saved".to_owned());
        }

        if let Some(edit_ctx) = composer.editing() {
            let req = UpdateRequest {
                identity: CommentId::new(edit_ctx.identity),
                severity: composer.severity(),
                body: &body,
                oversized: body.len() > DRAFT_BODY_MAX,
            };
            return match self.update_comment(req) {
                Ok(SaveOutcome::Saved { status_message }) => Ok(status_message),
                Ok(SaveOutcome::Refused { reason }) => Err(reason),
                Ok(SaveOutcome::Errored { message }) => Err(message),
                Err(e) => Err(format!("update failed: {}", strip_controls(&e.to_string()))),
            };
        }

        let req = SaveRequest {
            scope: composer.scope(),
            severity: composer.severity(),
            body: &body,
            entry_idx: 0,
        };
        match self.save_comment(req) {
            Ok(SaveOutcome::Saved { status_message }) => Ok(status_message),
            Ok(SaveOutcome::Refused { reason }) => Err(reason),
            Ok(SaveOutcome::Errored { message }) => Err(message),
            Err(e) => Err(format!("save failed: {}", strip_controls(&e.to_string()))),
        }
    }

    /// Route delete through the surface's `delete_comment`.
    /// Delete the draft comment under the cursor without opening the composer.
    /// Returns a status message action — either a confirmation or an error.
    fn delete_at_cursor(
        &mut self,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let Some(view) = current_view else {
            return ExtraKeyAction::Ignored;
        };
        let Some(row) = view.lines.get(line_index) else {
            return ExtraKeyAction::Ignored;
        };
        let RenderedLineKind::InlineCommentMeta { comment_index } = row.kind else {
            return ExtraKeyAction::Ignored;
        };

        match comment_index {
            CommentIndex::Local(draft_idx) => {
                let Some(draft) = self.loaded_drafts.get(draft_idx) else {
                    return ExtraKeyAction::StatusMessage(
                        "draft not found — navigate away and back to refresh".to_owned(),
                    );
                };
                // Use the stored created_at string directly as the key so
                // there is no RFC 3339 round-trip that could change "Z" to
                // "+00:00" and break the equality check in delete_draft.
                let created_at = draft.created_at.clone();
                let Some(base) = crate::util::data_home() else {
                    return ExtraKeyAction::StatusMessage(
                        "could not determine data directory".to_owned(),
                    );
                };
                let path = crate::draft::draft_path_from_base(&base, draft);
                match crate::draft::delete_draft(&path, |d| d.created_at == created_at) {
                    Ok(true) => {
                        self.reload_drafts();
                        ExtraKeyAction::RefreshAndStatus("draft deleted".to_owned())
                    }
                    Ok(false) => {
                        ExtraKeyAction::StatusMessage("draft not found on disk".to_owned())
                    }
                    Err(e) => ExtraKeyAction::StatusMessage(format!(
                        "delete failed: {}",
                        strip_controls(&e.to_string())
                    )),
                }
            }
            CommentIndex::LocalReply(reply_idx) => {
                let Some(reply) = self.loaded_replies.get(reply_idx) else {
                    return ExtraKeyAction::StatusMessage(
                        "reply draft not found — navigate away and back to refresh".to_owned(),
                    );
                };
                let created_at = reply.created_at.clone();
                let Some(base) = crate::util::data_home() else {
                    return ExtraKeyAction::StatusMessage(
                        "could not determine data directory".to_owned(),
                    );
                };
                let host = self.pr.hostname.as_deref().unwrap_or("github.com");
                let slug = self.pr.repo_name.as_str();
                let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
                let path =
                    crate::draft::replies_file_from_base(&base, host, owner, repo, self.pr.number);
                match crate::draft::delete_reply(&path, |r| r.created_at == created_at) {
                    Ok(true) => {
                        self.reload_drafts();
                        ExtraKeyAction::RefreshAndStatus("reply draft deleted".to_owned())
                    }
                    Ok(false) => {
                        ExtraKeyAction::StatusMessage("reply draft not found on disk".to_owned())
                    }
                    Err(e) => ExtraKeyAction::StatusMessage(format!(
                        "delete failed: {}",
                        strip_controls(&e.to_string())
                    )),
                }
            }
            CommentIndex::GitHubThread(_) => ExtraKeyAction::StatusMessage(
                "GitHub review threads cannot be deleted locally".to_owned(),
            ),
        }
    }

    fn delete_via_composer(&mut self, composer: &Composer) -> std::result::Result<(), String> {
        let Some(edit_ctx) = composer.editing() else {
            return Err("nothing to delete — composer is not in edit mode".to_owned());
        };
        let req = DeleteRequest::new(CommentId::new(edit_ctx.identity), edit_ctx.comment_index);
        match self.delete_comment(req) {
            Ok(DeleteOutcome::Deleted) => Ok(()),
            Ok(DeleteOutcome::Refused { reason }) => Err(reason),
            Err(e) => Err(format!("delete failed: {}", strip_controls(&e.to_string()))),
        }
    }

    /// Build a `GgrDraft` from the composer scope, or return a `SaveOutcome`
    /// indicating a refused/errored save.
    fn build_draft_from_scope(
        &self,
        scope: &ComposerScope,
        common: &crate::draft::CommonParams,
    ) -> std::result::Result<crate::draft::GgrDraft, SaveOutcome> {
        match scope {
            ComposerScope::Line(line_target) => {
                let sha = match self.current_commit_sha() {
                    Some(s) => s.clone(),
                    None => {
                        return Err(SaveOutcome::Errored {
                            message: "no commit selected; cannot save line comment".to_owned(),
                        })
                    }
                };
                let file_str = line_target.file.to_string_lossy().into_owned();
                if !crate::pr::valid_file_path(&file_str) {
                    return Err(SaveOutcome::Refused {
                        reason: "file path contains invalid segment".to_owned(),
                    });
                }
                let side = if line_target.target_line.is_some() {
                    Side::New
                } else {
                    Side::Old
                };
                let anchor = crate::draft::LineAnchorParams {
                    commit_sha: sha,
                    file: file_str,
                    side,
                    // Only set the line number for the chosen side; the
                    // validation in GgrDraft::new_line rejects anchors where
                    // both are set (context lines have both source and target).
                    old_line: if side == Side::Old {
                        line_target.source_line
                    } else {
                        None
                    },
                    new_line: if side == Side::New {
                        line_target.target_line
                    } else {
                        None
                    },
                    hunk_header: line_target.hunk_header.clone(),
                    target_text: line_target.target_text.clone(),
                    context_before: line_target.context_before.clone(),
                    context_after: line_target.context_after.clone(),
                };
                crate::draft::GgrDraft::new_line(common, &anchor).map_err(|e| {
                    SaveOutcome::Errored {
                        message: e.to_string(),
                    }
                })
            }
            ComposerScope::Change => {
                let sha = match self.current_commit_sha() {
                    Some(s) => s.clone(),
                    None => {
                        return Err(SaveOutcome::Errored {
                            message: "no commit selected; cannot save commit comment".to_owned(),
                        })
                    }
                };
                crate::draft::GgrDraft::new_commit(common, sha.as_str()).map_err(|e| {
                    SaveOutcome::Errored {
                        message: e.to_string(),
                    }
                })
            }
            ComposerScope::Stack(_) => {
                crate::draft::GgrDraft::new_pr(common).map_err(|e| SaveOutcome::Errored {
                    message: e.to_string(),
                })
            }
            ComposerScope::Description(_) => Err(SaveOutcome::Refused {
                reason: "description scope not supported in ggr".to_owned(),
            }),
        }
    }
}

// ── inline comment helpers ────────────────────────────────────────────────────

impl GgrSurface {
    fn collect_draft_inline(
        &self,
        file_path: &str,
        severity_filter: Option<Severity>,
        out: &mut Vec<InlineComment>,
    ) {
        for (draft_idx, draft) in self.loaded_drafts.iter().enumerate() {
            let crate::draft::GgrAnchor::Line {
                old_line,
                new_line,
                file: draft_file,
                ..
            } = &draft.anchor
            else {
                continue;
            };
            if draft_file.as_str() != file_path {
                continue;
            }
            if severity_filter.is_some_and(|f| draft.severity != f) {
                continue;
            }
            out.push(InlineComment {
                source_line: *old_line,
                target_line: *new_line,
                severity: draft.severity,
                age: "[draft]".to_owned(),
                body_lines: strip_controls_preserve_newlines(&draft.body)
                    .lines()
                    .map(str::to_owned)
                    .collect(),
                comment_index: CommentIndex::Local(draft_idx),
            });
        }
    }

    fn collect_thread_inline(
        &self,
        now: std::time::SystemTime,
        file_path: &str,
        severity_filter: Option<Severity>,
        out: &mut Vec<InlineComment>,
    ) {
        for (enumerate_idx, thread) in self.pr.review_threads.iter().enumerate() {
            if thread.path != file_path || thread.is_outdated() {
                continue;
            }
            if severity_filter.is_some_and(|f| thread.severity != f) {
                continue;
            }
            let mut body_lines: Vec<String> = strip_controls_preserve_newlines(&thread.root.body)
                .lines()
                .map(str::to_owned)
                .collect();
            // Append each reply as `── @<author> · <age>` header followed by
            // the reply body. Without this, replies-by-other-reviewers in a
            // GitHub PR thread silently disappear from the diff view.
            for reply in &thread.replies {
                body_lines.push(String::new());
                let age = local_review_core::util::format_age_from_iso_str(now, &reply.created_at);
                body_lines.push(format!("── @{} · {}", strip_controls(&reply.author), age));
                body_lines.extend(
                    strip_controls_preserve_newlines(&reply.body)
                        .lines()
                        .map(str::to_owned),
                );
            }
            out.push(InlineComment {
                source_line: thread.original_line,
                target_line: thread.line,
                severity: thread.severity,
                age: local_review_core::util::format_age_from_iso_str(now, &thread.root.created_at),
                body_lines,
                comment_index: CommentIndex::GitHubThread(enumerate_idx),
            });
        }
    }

    fn collect_reply_inline(&self, file_path: &str, out: &mut Vec<InlineComment>) {
        for (reply_idx, reply) in self.loaded_replies.iter().enumerate() {
            let Some(thread) = self.pr.review_threads.iter().find(|t| {
                t.root.id.to_string() == reply.parent_comment_id
                    && t.path == file_path
                    && !t.is_outdated()
            }) else {
                continue;
            };
            out.push(InlineComment {
                source_line: thread.original_line,
                target_line: thread.line,
                severity: reply.severity,
                age: "[pending reply]".to_owned(),
                body_lines: strip_controls_preserve_newlines(&reply.body)
                    .lines()
                    .map(str::to_owned)
                    .collect(),
                comment_index: CommentIndex::LocalReply(reply_idx),
            });
        }
    }
}

// ── ReviewSurface impl ────────────────────────────────────────────────────────

impl ReviewSurface for GgrSurface {
    type Error = GgrError;

    fn entry_count(&self) -> usize {
        self.pr.commits.len() + 1
    }

    fn current_entry_index(&self) -> usize {
        if let Some(idx) = self.pending_initial_index {
            return idx;
        }
        match self.state {
            State::Description => 0,
            State::CommitDiff { index, .. } => index,
        }
    }

    fn entry_id_display(&self, idx: usize) -> String {
        if idx == 0 {
            return match self.pr_pane {
                PrPane::Description => "overview".to_owned(),
                PrPane::Entities => {
                    let k = self.pr_entity_commit_indices.len();
                    format!("all entities ({k})")
                }
            };
        }
        self.pr
            .commits
            .get(idx - 1)
            .map(|c| c.short_sha.clone())
            .unwrap_or_default()
    }

    fn stack_bar_spec(&self) -> local_review_core::tui::StackBarSpec {
        let current = self.current_entry_index();
        let commits = self.pr.commits.len();
        // Two entry species share the walk: the PR overview (entry 0) and
        // the commits (1..=N). The overview has no position among the
        // commits, so it gets a label instead of a counter — numbering it
        // uniformly would show a 5-commit PR as "1/6 overview" with
        // every commit off-by-one.
        if current == 0 {
            local_review_core::tui::StackBarSpec {
                title: "Pull Request".to_owned(),
                progress: None,
                label: format!("overview  PR #{}", self.pr.number),
            }
        } else {
            local_review_core::tui::StackBarSpec {
                title: "Pull Request".to_owned(),
                progress: Some((current, commits)),
                label: format!(
                    "commit {current}/{commits}  {}",
                    self.entry_id_display(current)
                ),
            }
        }
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
        self.pending_initial_index.take();
        if idx == 0 {
            self.state = State::Description;
            let desc = self.pr_description_text();
            let title = format!("PR #{} — description", self.pr.number);
            let views = vec![DiffView::from_description(&desc).with_title(title)];
            self.loaded_drafts.clear();
            return Ok(views);
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
            let file_path = file.display_path().to_string_lossy();
            views.push(DiffView::from_file(file).with_syntax_highlighting(&file_path));
        }
        self.state = State::CommitDiff { index: idx, diff };
        self.reload_drafts();
        Ok(views)
    }

    fn fetch_entity_list(
        &mut self,
        entry_idx: usize,
    ) -> std::result::Result<Vec<local_review_core::semantic::EntitySummary>, GgrError> {
        if entry_idx == 0 {
            // Entity pane: aggregate entities across all commits.
            let (summaries, commit_indices) = self.aggregate_pr_entities();
            self.pr_entity_commit_indices = commit_indices;
            return Ok(summaries);
        }
        let commit_idx = entry_idx - 1;
        let Some(commit) = self.pr.commits.get(commit_idx) else {
            return Ok(Vec::new());
        };
        let sha = commit.sha.as_str();
        let owner_repo = self.pr.repo_name.as_str();
        let cache_path =
            ggr_entity_cache_base(owner_repo, self.pr.number, self.pr.hostname.as_deref())
                .map(|base| local_review_core::semantic::cache::ggr_cache_path(&base, sha));

        let registry = local_review_core::semantic::create_default_registry();

        // Always fetch the diff: cache-hit path needs it to interleave fallback
        // rows for files that produced no entities. The diff itself is cheap
        // (a single gh API call) compared to extraction.
        let diff =
            gh::fetch_commit_diff(&self.pr.repo_name, &commit.sha, self.pr.hostname.as_deref())?;

        if let Some(ref p) = cache_path {
            if let Ok(Some(mut entry)) = local_review_core::semantic::cache::read(p) {
                // Cache hit. If the graph is absent but the eager clone has
                // since become ready, build the graph now and update the
                // cache — this upgrades entries extracted while the clone
                // was still in flight.
                if entry.graph.is_none() {
                    if let Some(repo_path) = self.ready_repo_path() {
                        let files = crate::repo_cache::list_files(&repo_path);
                        let graph =
                            local_review_core::semantic::build_graph(&registry, &repo_path, &files);
                        entry.graph = Some(graph);
                        let _ = local_review_core::semantic::cache::write(p, &entry);
                    }
                }
                return Ok(ggr_build_entity_summaries_interleaved(entry, &diff));
            }
        }

        let file_paths: Vec<String> = diff
            .files
            .iter()
            .map(|f| f.display_path().to_string_lossy().into_owned())
            .collect();

        let pairs = gh::fetch_commit_file_contents(
            &self.pr.repo_name,
            &commit.sha,
            &file_paths,
            self.pr.hostname.as_deref(),
        );

        let mut entities = Vec::new();
        let mut failed_files = Vec::new();

        for pair in &pairs {
            let before_raw = registry.extract(&pair.before, &pair.path);
            let after_raw = registry.extract(&pair.after, &pair.path);
            match (before_raw, after_raw) {
                (Ok(b), Ok(a)) => {
                    entities.extend(local_review_core::semantic::diff_entities(&b, &a));
                }
                _ => failed_files.push(pair.path.clone()),
            }
        }

        // Build the cross-file call graph from the eager clone (checked out
        // at the PR head SHA). A clone still in flight or unavailable means
        // the graph stays None — tiers degrade visibly, never a hard error;
        // the cache-hit path above upgrades the entry once the clone lands.
        let graph = self.ready_repo_path().map(|repo_path| {
            let files = crate::repo_cache::list_files(&repo_path);
            local_review_core::semantic::build_graph(&registry, &repo_path, &files)
        });

        let cache_entry = local_review_core::semantic::cache::CacheEntry {
            schema_version: local_review_core::semantic::cache::SCHEMA_VERSION,
            extraction_hash: local_review_core::semantic::cache::EXTRACTION_HASH.to_owned(),
            entities,
            graph,
            failed_files,
        };
        if let Some(ref p) = cache_path {
            let _ = local_review_core::semantic::cache::write(p, &cache_entry);
        }
        Ok(ggr_build_entity_summaries_interleaved(cache_entry, &diff))
    }

    fn fetch_description_summary(
        &self,
        entry_idx: usize,
    ) -> std::result::Result<local_review_core::semantic::DescriptionSummary, GgrError> {
        // Entry 0 peeks the PR body; commit entries peek the commit message
        // body. Both are untrusted GitHub input — strip controls here.
        let body_peek = if entry_idx == 0 {
            local_review_core::semantic::body_peek_from_body(&self.pr.body)
        } else {
            self.pr
                .commits
                .get(entry_idx - 1)
                .and_then(|c| local_review_core::semantic::body_peek_from_body(&c.body))
        }
        .map(|p| strip_controls(&p));
        Ok(local_review_core::semantic::DescriptionSummary {
            subject: self.entry_description(entry_idx),
            comment_count: 0,
            body_peek,
        })
    }

    fn clear_entity_cache(&mut self, entry_idx: usize) {
        if entry_idx == 0 {
            return;
        }
        let Some(commit) = self.pr.commits.get(entry_idx - 1) else {
            return;
        };
        let sha = commit.sha.as_str();
        let owner_repo = self.pr.repo_name.as_str();
        if let Some(cache_path) =
            ggr_entity_cache_base(owner_repo, self.pr.number, self.pr.hostname.as_deref())
                .map(|base| local_review_core::semantic::cache::ggr_cache_path(&base, sha))
        {
            let _ = std::fs::remove_file(&cache_path);
        }
    }

    fn graph_unavailable_reason(&self) -> Option<String> {
        match self.graph_clone.lock() {
            Ok(state) => match &*state {
                GraphCloneState::Ready(_) => None,
                GraphCloneState::InProgress => Some("clone in progress".to_owned()),
                GraphCloneState::Unavailable(reason) => Some(reason.clone()),
            },
            Err(_) => None,
        }
    }

    fn entry_graph(&self, entry_idx: usize) -> Option<local_review_core::semantic::GraphData> {
        // One cache read per invocation — at entry load and on `o`, never
        // from render. Entry 0 is the PR overview: its entity list spans
        // commits, so no single per-commit graph applies.
        if entry_idx == 0 {
            return None;
        }
        let commit = self.pr.commits.get(entry_idx - 1)?;
        let sha = commit.sha.as_str();
        let owner_repo = self.pr.repo_name.as_str();
        let cache_path =
            ggr_entity_cache_base(owner_repo, self.pr.number, self.pr.hostname.as_deref())
                .map(|base| local_review_core::semantic::cache::ggr_cache_path(&base, sha))?;
        let entry = local_review_core::semantic::cache::read(&cache_path)
            .ok()
            .flatten()?;
        entry.graph
    }

    fn has_pr_pane_toggle(&self, entry_idx: usize) -> bool {
        entry_idx == 0
    }

    fn is_description_entry(&self, entry_idx: usize) -> bool {
        entry_idx == 0 && self.pr_pane == PrPane::Description
    }

    fn toggle_pr_pane(&mut self) {
        self.pr_pane = match self.pr_pane {
            PrPane::Description => PrPane::Entities,
            PrPane::Entities => PrPane::Description,
        };
    }

    fn pr_entity_commit_entry(&self, entity_idx: usize) -> Option<usize> {
        let entry = self.pr_entity_commit_indices.get(entity_idx).copied()?;
        // Sentinel 0 means commit index unknown (cached aggregation without
        // index data). Fall back to None so the core uses the current entry.
        if entry == 0 {
            None
        } else {
            Some(entry)
        }
    }

    fn appended_comments_for_view(
        &self,
        view_idx: usize,
        severity_filter: Option<Severity>,
    ) -> Vec<InlineComment> {
        // view_idx=0 is the synthetic description/title sub-view for every
        // entry (PR description page or commit title). Append PR-scope and
        // commit-scope draft markers there so the reviewer can see them.
        if view_idx != 0 {
            return Vec::new();
        }
        let now = std::time::SystemTime::now();
        let mut result = Vec::new();
        for (draft_idx, draft) in self.loaded_drafts.iter().enumerate() {
            if let Some(crate::draft::DraftStatus::Stale) = draft.status {
                continue;
            }
            if let Some(f) = severity_filter {
                if draft.severity != f {
                    continue;
                }
            }
            let age = local_review_core::util::format_age_from_iso_str(now, &draft.created_at);
            let label = match &draft.anchor {
                crate::draft::GgrAnchor::Pr => "[PR draft]".to_owned(),
                crate::draft::GgrAnchor::Commit { .. } => "[commit draft]".to_owned(),
                crate::draft::GgrAnchor::Line { .. } => continue,
            };
            result.push(InlineComment {
                source_line: None,
                target_line: None,
                severity: draft.severity,
                age: format!("{label} · {age}"),
                body_lines: strip_controls_preserve_newlines(&draft.body)
                    .lines()
                    .map(str::to_owned)
                    .collect(),
                comment_index: CommentIndex::Local(draft_idx),
            });
        }
        result
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
        let Some(file) = diff.files.get(view_idx - 1) else {
            return Vec::new();
        };
        let file_path = file.display_path().to_string_lossy();
        let mut result: Vec<InlineComment> = Vec::new();
        self.collect_draft_inline(&file_path, severity_filter, &mut result);
        self.collect_thread_inline(now, &file_path, severity_filter, &mut result);
        self.collect_reply_inline(&file_path, &mut result);
        result
    }

    fn save_comment(&mut self, req: SaveRequest<'_>) -> std::result::Result<SaveOutcome, GgrError> {
        let body = req.body;
        if body.trim().is_empty() {
            return Ok(SaveOutcome::Refused {
                reason: "comment body is empty — not saved".to_owned(),
            });
        }
        let Some(base) = crate::util::data_home() else {
            return Ok(SaveOutcome::Errored {
                message: "could not determine data directory; XDG_DATA_HOME and HOME unset"
                    .to_owned(),
            });
        };
        let created_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| GgrError::Io {
                source: std::io::Error::other(e.to_string()),
            })?;
        let host = self
            .pr
            .hostname
            .as_deref()
            .unwrap_or("github.com")
            .to_owned();
        let slug = self.pr.repo_name.as_str();
        // RepoName is validated as "owner/repo"; split_once always succeeds for
        // valid values. Fallback to ("_", "_") satisfies the linter without unwrap.
        let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        let common = crate::draft::CommonParams {
            host,
            owner,
            repo,
            pr_number: self.pr.number,
            body: body.to_owned(),
            severity: req.severity,
            created_at,
        };

        let draft = match self.build_draft_from_scope(req.scope, &common) {
            Ok(d) => d,
            Err(outcome) => return Ok(outcome),
        };

        let path = crate::draft::draft_path_from_base(&base, &draft);
        crate::draft::append_draft(&path, &draft)?;
        self.reload_drafts();

        let msg = match &draft.anchor {
            crate::draft::GgrAnchor::Line { .. } => "line draft saved",
            crate::draft::GgrAnchor::Commit { .. } => {
                "commit draft saved — visible at top of this commit"
            }
            crate::draft::GgrAnchor::Pr => {
                "PR draft saved — visible on description page (f → PR description)"
            }
        };
        Ok(SaveOutcome::Saved {
            status_message: msg.to_owned(),
        })
    }

    fn update_comment(
        &mut self,
        req: UpdateRequest<'_>,
    ) -> std::result::Result<SaveOutcome, GgrError> {
        let body = req.body;
        if body.trim().is_empty() {
            return Ok(SaveOutcome::Refused {
                reason: "comment body is empty — not saved".to_owned(),
            });
        }
        let created_at_key = req
            .identity
            .as_offset_date_time()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| GgrError::Io {
                source: std::io::Error::other(e.to_string()),
            })?;
        let Some(base) = crate::util::data_home() else {
            return Ok(SaveOutcome::Errored {
                message: "could not determine data directory".to_owned(),
            });
        };
        // Compute path within a borrow scope so the immutable borrow on
        // loaded_drafts ends before the mutable reload_drafts call below.
        let path = {
            let Some(draft) = self
                .loaded_drafts
                .iter()
                .find(|d| d.created_at == created_at_key)
            else {
                return Ok(SaveOutcome::Refused {
                    reason: "comment not found".to_owned(),
                });
            };
            crate::draft::draft_path_from_base(&base, draft)
        };
        let updated_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| GgrError::Io {
                source: std::io::Error::other(e.to_string()),
            })?;
        if crate::draft::update_draft(&path, &created_at_key, body, req.severity, &updated_at)? {
            self.reload_drafts();
            let msg = if req.oversized {
                "draft updated (body exceeds 64 KB — will be trimmed on submit)"
            } else {
                "draft updated"
            };
            Ok(SaveOutcome::Saved {
                status_message: msg.to_owned(),
            })
        } else {
            Ok(SaveOutcome::Refused {
                reason: "comment not found on disk".to_owned(),
            })
        }
    }

    fn delete_comment(
        &mut self,
        req: DeleteRequest,
    ) -> std::result::Result<DeleteOutcome, GgrError> {
        let created_at_key = req
            .identity
            .as_offset_date_time()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| GgrError::Io {
                source: std::io::Error::other(e.to_string()),
            })?;
        let Some(base) = crate::util::data_home() else {
            return Ok(DeleteOutcome::Refused {
                reason: "could not determine data directory".to_owned(),
            });
        };
        // Compute path within a borrow scope so the immutable borrow on
        // loaded_drafts ends before the mutable reload_drafts call below.
        let path = {
            let Some(draft) = self
                .loaded_drafts
                .iter()
                .find(|d| d.created_at == created_at_key)
            else {
                return Ok(DeleteOutcome::Refused {
                    reason: "comment not found".to_owned(),
                });
            };
            crate::draft::draft_path_from_base(&base, draft)
        };
        if crate::draft::delete_draft(&path, |d| d.created_at == created_at_key)? {
            self.reload_drafts();
            Ok(DeleteOutcome::Deleted)
        } else {
            Ok(DeleteOutcome::Refused {
                reason: "comment not found on disk".to_owned(),
            })
        }
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
        let mut h = SeverityHistogram::default();
        for draft in &self.loaded_drafts {
            match draft.severity {
                Severity::Required => h.required = h.required.saturating_add(1),
                Severity::Suggestion => h.suggestion = h.suggestion.saturating_add(1),
                Severity::Note => h.note = h.note.saturating_add(1),
            }
        }
        h
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "unhandled KeyCode variants are intentionally passed through as Ignored"
    )]
    fn handle_extra_key(
        &mut self,
        key: KeyEvent,
        file_index: usize,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> std::result::Result<ExtraKeyAction, GgrError> {
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
            KeyCode::Char('c') | KeyCode::Enter => {
                let comment_at_cursor = current_view
                    .and_then(|v| v.lines.get(line_index))
                    .and_then(|row| match row.kind {
                        RenderedLineKind::InlineCommentMeta { comment_index } => {
                            Some(comment_index)
                        }
                        _ => None,
                    });
                match comment_at_cursor {
                    Some(CommentIndex::GitHubThread(_)) => {
                        Ok(self.open_reply_composer(line_index, current_view))
                    }
                    Some(_) => Ok(self.open_edit_composer(line_index, current_view)),
                    None => Ok(self.open_composer_at(file_index, line_index, current_view)),
                }
            }
            KeyCode::Char('m') => Ok(self.open_commit_scope_composer()),
            KeyCode::Char('P') => Ok(self.open_pr_scope_composer()),
            KeyCode::Char('e') => Ok(self.open_edit_composer(line_index, current_view)),
            KeyCode::Char('d') => Ok(self.delete_at_cursor(line_index, current_view)),
            KeyCode::Char('r') => Ok(self.open_reply_composer(line_index, current_view)),
            KeyCode::Char('S') => Ok(ExtraKeyAction::OpenScreen(Box::new(VerdictScreen))),
            KeyCode::Char('R') => Ok(self.run_refresh()),
            _ => Ok(ExtraKeyAction::Ignored),
        }
    }

    fn render_extra_screen(&self, frame: &mut Frame<'_>, state: &mut dyn ExtraScreen) {
        if let Some(s) = state.as_any_mut().downcast_mut::<ComposerScreen>() {
            composer_overlay::render_composer_overlay(frame, &s.0, None);
        } else if let Some(s) = state.as_any_mut().downcast_mut::<ReplyComposerScreen>() {
            composer_overlay::render_composer_overlay(frame, &s.composer, None);
        } else if state.as_any().downcast_ref::<VerdictScreen>().is_some() {
            render_verdict_screen(frame);
        } else if state.as_any().downcast_ref::<SubmittingOverlay>().is_some() {
            render_submitting_overlay(frame);
        } else if let Some(panel) = state.as_any().downcast_ref::<StalePanel>() {
            render_stale_panel(frame, panel);
        }
    }

    fn handle_extra_screen_key(
        &mut self,
        state: &mut dyn ExtraScreen,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> std::result::Result<ExtraScreenAction, GgrError> {
        if let Some(s) = try_downcast_mut::<ComposerScreen>(state) {
            return Ok(self.handle_composer_key_impl(&mut s.0, key, ctx));
        }
        if let Some(s) = try_downcast_mut::<ReplyComposerScreen>(state) {
            let parent_id = s.parent_comment_id.clone();
            return Ok(self.handle_reply_composer_key(&mut s.composer, &parent_id, key, ctx));
        }
        if let Some(panel) = try_downcast_mut::<StalePanel>(state) {
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "all non-navigation KeyCode variants pass through as StayOpen"
            )]
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    panel.cursor = panel.cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    panel.cursor = panel
                        .cursor
                        .saturating_add(1)
                        .min(panel.items.len().saturating_sub(1));
                }
                KeyCode::Char('d') => {
                    let idx = panel.cursor;
                    if idx < panel.items.len() {
                        let item = panel.items.remove(idx);
                        self.delete_stale_item(&item, ctx);
                        if panel.cursor > 0 && panel.cursor >= panel.items.len() {
                            panel.cursor -= 1;
                        }
                    }
                    if panel.items.is_empty() {
                        return Ok(ExtraScreenAction::Close);
                    }
                }
                KeyCode::Esc => return Ok(ExtraScreenAction::Close),
                _ => {}
            }
            return Ok(ExtraScreenAction::StayOpen);
        }
        if try_downcast_mut::<VerdictScreen>(state).is_some() {
            if key.code == KeyCode::Esc {
                return Ok(ExtraScreenAction::Close);
            }
            if let Some(v) = verdict_from_key(key.code) {
                self.pending_submit = Some(v);
                return Ok(ExtraScreenAction::OpenScreen(Box::new(SubmittingOverlay)));
            }
            return Ok(ExtraScreenAction::StayOpen);
        }
        if try_downcast_mut::<SubmittingOverlay>(state).is_some() {
            // Key presses during submit are ignored; poll_immediate_action
            // drives the transition once the overlay frame is visible.
            return Ok(ExtraScreenAction::StayOpen);
        }
        Ok(ExtraScreenAction::StayOpen)
    }

    fn file_picker_entries(&self) -> Vec<FilePickerEntry> {
        match &self.state {
            State::Description => file_picker::build_entries(&[], &|_| 0, &|_| false, &|_| 0),
            State::CommitDiff { diff, .. } => {
                let comment_count = |view_idx: usize| -> usize {
                    let Some(file_idx) = view_idx.checked_sub(1) else {
                        return 0;
                    };
                    let Some(file) = diff.files.get(file_idx) else {
                        return 0;
                    };
                    let file_path = file.display_path().to_string_lossy();
                    let draft_count = self
                        .loaded_drafts
                        .iter()
                        .filter(|d| {
                            d.status != Some(crate::draft::DraftStatus::Stale)
                                && matches!(&d.anchor, crate::draft::GgrAnchor::Line { file, .. } if file.as_str() == file_path.as_ref())
                        })
                        .count();
                    let thread_count = self
                        .pr
                        .review_threads
                        .iter()
                        .filter(|t| t.path == file_path.as_ref() && !t.is_outdated())
                        .count();
                    draft_count + thread_count
                };
                let mut entries = file_picker::build_entries(
                    diff.files.as_slice(),
                    &comment_count,
                    &|_| false,
                    &|_| 0,
                );
                // Rename the description entry to clarify it's the PR
                // description + comments, not a code file.
                if let Some(e) = entries.first_mut() {
                    e.display_path =
                        std::path::PathBuf::from(format!("<PR #{} description>", self.pr.number));
                }
                entries
            }
        }
    }

    fn help_screen_title(&self) -> &'static str {
        "ggr · keybindings"
    }

    fn help_screen_body(&self) -> &'static str {
        GGR_HELP_BODY
    }

    fn footer_hint(
        &self,
        width: u16,
        _has_stack: bool,
        severity_filter: Option<Severity>,
    ) -> String {
        ggr_footer_text_for_width(width, severity_filter)
    }
}

impl GgrSurface {
    /// Aggregate entities across all commits into a net PR-level entity list.
    ///
    /// Returns `(summaries, commit_indices)` where `commit_indices[i]` is the
    /// 1-based entry index of the commit that last modified `summaries[i]`.
    ///
    /// Cache key: `_pr-<head_sha>.json` under the ggr entity cache base.
    /// Head SHA is the last commit's SHA; this invalidates when new commits are
    /// pushed without requiring a separate base-SHA API call.
    fn aggregate_pr_entities(
        &self,
    ) -> (Vec<local_review_core::semantic::EntitySummary>, Vec<usize>) {
        use local_review_core::semantic::EntityCoreData;
        use std::collections::HashMap;

        let owner_repo = self.pr.repo_name.as_str();
        let host = self.pr.hostname.as_deref();

        // Cache check: try the PR-level cache entry first.
        let head_sha = self
            .pr
            .commits
            .last()
            .map(|c| format!("_pr-{}", c.sha.as_str()))
            .unwrap_or_else(|| "_pr-empty".to_owned());
        let pr_cache_path = ggr_entity_cache_base(owner_repo, self.pr.number, host)
            .map(|base| local_review_core::semantic::cache::ggr_cache_path(&base, &head_sha));

        // A cached aggregation entry has no `diff` to interleave fallback rows
        // against, so we store and return raw EntityCoreData and rebuild the
        // commit-index mapping from the per-commit caches.
        if let Some(ref p) = pr_cache_path {
            if let Ok(Some(entry)) = local_review_core::semantic::cache::read(p) {
                let summaries: Vec<_> = entry
                    .entities
                    .iter()
                    .map(ggr_entity_summary_from_core)
                    .collect();
                // Rebuild commit indices by walking per-commit caches.  This is
                // the same last-writer-wins walk used on a cache miss, but only
                // touches EntityIds (no network, no extraction).  Without this
                // step all indices would be 0, and Enter on an aggregated entity
                // would try to open an entity diff on entry 0 (the PR overview)
                // instead of navigating to the right commit.
                let indices = rebuild_commit_indices(
                    &summaries,
                    &self.pr.commits,
                    owner_repo,
                    self.pr.number,
                    host,
                );
                return (summaries, indices);
            }
        }

        // Cache miss: walk all commits, load per-commit caches, aggregate.
        // `by_entity` maps EntityId to (EntityCoreData, 1-based entry_idx).
        // Later commits overwrite earlier ones — last writer wins for the net
        // change type. Added-then-Deleted pairs cancel out (removed from map).
        let mut by_entity: HashMap<local_review_core::semantic::EntityId, (EntityCoreData, usize)> =
            HashMap::new();

        for (commit_idx, commit) in self.pr.commits.iter().enumerate() {
            let entry_idx = commit_idx + 1;
            let sha = commit.sha.as_str();
            let cache_path = ggr_entity_cache_base(owner_repo, self.pr.number, host)
                .map(|base| local_review_core::semantic::cache::ggr_cache_path(&base, sha));
            let cache_entry = cache_path
                .as_ref()
                .and_then(|p| local_review_core::semantic::cache::read(p).ok().flatten());

            if let Some(entry) = cache_entry {
                for entity in entry.entities {
                    use local_review_core::semantic::ChangeType;
                    // Added-then-Deleted: net is absent.
                    if entity.change == ChangeType::Deleted {
                        by_entity.remove(&entity.id);
                    } else {
                        by_entity.insert(entity.id.clone(), (entity, entry_idx));
                    }
                }
            }
        }

        // Sort by file path, then by start line for stable ordering.
        let mut pairs: Vec<(EntityCoreData, usize)> = by_entity.into_values().collect();
        pairs.sort_by(|(a, _), (b, _)| {
            a.id.file_path
                .cmp(&b.id.file_path)
                .then(a.line_range.0.cmp(&b.line_range.0))
        });

        let summaries: Vec<_> = pairs
            .iter()
            .map(|(e, _)| ggr_entity_summary_from_core(e))
            .collect();
        let commit_indices: Vec<usize> = pairs.iter().map(|(_, idx)| *idx).collect();

        // Write the aggregated list to the PR-level cache for next time.
        if let Some(ref p) = pr_cache_path {
            let agg_entry = local_review_core::semantic::cache::CacheEntry {
                schema_version: local_review_core::semantic::cache::SCHEMA_VERSION,
                extraction_hash: local_review_core::semantic::cache::EXTRACTION_HASH.to_owned(),
                entities: pairs.into_iter().map(|(e, _)| e).collect(),
                graph: None,
                failed_files: Vec::new(),
            };
            let _ = local_review_core::semantic::cache::write(p, &agg_entry);
        }

        (summaries, commit_indices)
    }
}

/// Rebuild the commit-index mapping for a cached aggregated entity list.
///
/// Walks per-commit caches oldest-to-newest recording the last commit that
/// contained each `EntityId`. Returns a parallel `Vec<usize>` where each
/// element is the 1-based entry index of the commit that last modified the
/// corresponding entity summary.  Entities not found in any per-commit cache
/// (shouldn't happen but defensive) get index 0 (unknown).
fn rebuild_commit_indices(
    summaries: &[local_review_core::semantic::EntitySummary],
    commits: &[crate::pr::CommitEntry],
    owner_repo: &str,
    pr_number: u64,
    hostname: Option<&str>,
) -> Vec<usize> {
    use std::collections::HashMap;

    let mut last_commit: HashMap<local_review_core::semantic::EntityId, usize> = HashMap::new();

    for (commit_idx, commit) in commits.iter().enumerate() {
        let entry_idx = commit_idx + 1;
        let sha = commit.sha.as_str();
        if let Some(cache_path) = ggr_entity_cache_base(owner_repo, pr_number, hostname)
            .map(|base| local_review_core::semantic::cache::ggr_cache_path(&base, sha))
        {
            if let Ok(Some(entry)) = local_review_core::semantic::cache::read(&cache_path) {
                for entity in entry.entities {
                    last_commit.insert(entity.id, entry_idx);
                }
            }
        }
    }

    summaries
        .iter()
        .map(|s| last_commit.get(&s.id).copied().unwrap_or(0))
        .collect()
}

/// Return the XDG-style cache base path for ggr entity extraction.
///
/// `owner_repo` is `"owner/repo"`. Creates a path like:
/// Returns `None` when no suitable data home can be determined (both
/// `XDG_DATA_HOME` and `HOME` are unset). The caller should treat `None` as
/// "no cache available" and skip the cache for this session.
fn ggr_entity_cache_base(
    owner_repo: &str,
    pr_number: u64,
    hostname: Option<&str>,
) -> Option<std::path::PathBuf> {
    let base = crate::util::data_home()?
        .join("ggr")
        .join("cache")
        .join("entities");
    // Include the hostname so the same PR reviewed on github.com vs a GHE
    // instance uses separate cache directories.
    let host_segment = hostname.unwrap_or("github.com");
    let (owner, repo) = owner_repo.split_once('/').unwrap_or((owner_repo, "repo"));
    Some(
        base.join(host_segment)
            .join(owner)
            .join(repo)
            .join(pr_number.to_string()),
    )
}

/// Convert a `CacheEntry` into renderable `EntitySummary` values.
/// Convert a single `EntityCoreData` to an `EntitySummary` without a diff
/// context (no fallback rows, `comment_count` 0, `reviewed` false).
fn ggr_entity_summary_from_core(
    e: &local_review_core::semantic::EntityCoreData,
) -> local_review_core::semantic::EntitySummary {
    local_review_core::semantic::EntitySummary::from_core(e)
}

fn ggr_build_entity_summaries(
    entry: local_review_core::semantic::cache::CacheEntry,
) -> Vec<local_review_core::semantic::EntitySummary> {
    entry
        .entities
        .into_iter()
        .map(|e| local_review_core::semantic::EntitySummary::from_core(&e))
        .collect()
}

/// Build summaries from a cache entry, interleaving a synthetic fallback row
/// for every file in `diff` that has no entities. Order follows `diff.files`
/// so the entity list stays aligned with the diff — extraction failure or
/// an unrecognised language never erases a file from the reviewer's view.
fn ggr_build_entity_summaries_interleaved(
    entry: local_review_core::semantic::cache::CacheEntry,
    diff: &local_review_core::diff::Diff,
) -> Vec<local_review_core::semantic::EntitySummary> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let raw = ggr_build_entity_summaries(entry);
    let mut by_path: HashMap<PathBuf, Vec<local_review_core::semantic::EntitySummary>> =
        HashMap::new();
    for s in raw {
        by_path.entry(s.file_path.clone()).or_default().push(s);
    }
    let mut out = Vec::new();
    for file in &diff.files {
        let path = file.display_path().to_path_buf();
        match by_path.remove(&path) {
            Some(file_entities) if !file_entities.is_empty() => out.extend(file_entities),
            _ => out.push(local_review_core::semantic::fallback_summary_for_file(file)),
        }
    }
    out
}

fn ggr_footer_text_for_width(width: u16, severity_filter: Option<Severity>) -> String {
    let badge = match severity_filter {
        Some(Severity::Required) => "  [F:required]",
        Some(Severity::Suggestion) => "  [F:suggestion]",
        Some(Severity::Note) => "  [F:note]",
        None => "",
    };
    // Build from widest to narrowest; drop optional segments to fit.
    let full = format!(
        " ↑↓ line  Tab file  n/p commit  c comment  e/d edit/delete  S submit  R refresh  ?{badge}"
    );
    let medium =
        format!(" ↑↓ line  Tab file  n/p commit  c comment  S submit  R refresh  ?{badge}");
    let narrow = format!(" ↑↓ Tab n/p  c comment  S submit  ?{badge}");
    let w = usize::from(width);
    if full.chars().count() <= w {
        full
    } else if medium.chars().count() <= w {
        medium
    } else {
        narrow
    }
}

const GGR_HELP_BODY: &str = "
Movement
    ↑ ↓     k j           line
    PgUp PgDn             page
    Home End  g G         top / bottom
    Tab     S-Tab         next / previous file
    n       p             next / previous commit (or PR overview)

Comments
    Enter   c             new line-scoped comment
    m                     new commit-scoped comment
    P                     new PR-scoped comment
    r                     reply to thread on current line
    e                     edit draft (cursor must be on the draft line)
    d                     delete draft (cursor must be on the draft line)
    T                     toggle thread expand / collapse

Filters
    1 / 2 / 3             severity filter: required / suggestion / note
    ;                     hide / show behavior-preserving rows
                          (cosmetic + renamed / moved / extracted tags)

Views
    (header)              entity list opens with the entry subject (PR title
                          on the overview, commit subject on commits), a
                          body peek, and a Σ scope line
                          (entities · files · LOC · sig changes)
    o                     cycle entity order: risk / dependency / file
                          (risk puts ! high-tier rows first — sig changes
                          with callers, deletions with dangling references)
    f                     file picker
    R                     refresh — re-fetch PR state and re-anchor drafts
    y                     yank ±10 lines around cursor (with file:line header)
                          to system clipboard — paste into Claude as context
    C                     send current commit to Claude (opens preview)
    S                     submit review (opens verdict modal)
    |                     cycle diff layout: auto / unified / side-by-side
                          (auto picks side-by-side at >=120 cols)
    ?                     this help
    q                     quit

── Sub-screens (context-sensitive) ──────────────────────────────────────────

Verdict modal  (press S from main view)
    a                     Approve
    r                     Request changes
    c   Enter             Comment (default)
    Esc                   cancel

Stale panel  (opens automatically when stale drafts exist)
    ↑ ↓     k j           select entry
    d                     delete focused stale draft
    Esc                   dismiss

In comment composer
    M-l M-c M-k           scope:    line / commit / PR
    M-r M-s M-n           severity: required / suggestion / note
    ^X                    save
    ^D                    delete (edit mode only)
    Esc                   cancel
";

// ── ReviewSurfaceExt impl ─────────────────────────────────────────────────────

impl ReviewSurfaceExt for GgrSurface {
    fn on_entry_loaded(&mut self, _idx: usize, _record_cursor: bool) {
        // `load_entry` always calls `fetch_views` before `on_entry_loaded`,
        // and `fetch_views` already sets `self.state` with the real commit diff
        // and clears `pending_initial_index`. This hook only needs to ensure
        // `pending_initial_index` is cleared so `current_entry_index()` returns
        // the updated index rather than the stale startup value — that is the
        // fix for `n`/`p` navigation appearing to do nothing.
        self.pending_initial_index = None;
    }

    fn take_pending_status_message(&mut self) -> Option<String> {
        self.pending_stale_message.take()
    }

    fn severity_histogram_for_transition(&self) -> (Option<usize>, SeverityHistogram) {
        let mut hist = SeverityHistogram::default();
        for d in &self.loaded_drafts {
            if d.status == Some(crate::draft::DraftStatus::Stale) {
                continue;
            }
            match d.severity {
                Severity::Required => hist.required += 1,
                Severity::Suggestion => hist.suggestion += 1,
                Severity::Note => hist.note += 1,
            }
        }
        for r in &self.loaded_replies {
            if r.status == Some(crate::draft::DraftStatus::Stale) {
                continue;
            }
            match r.severity {
                Severity::Required => hist.required += 1,
                Severity::Suggestion => hist.suggestion += 1,
                Severity::Note => hist.note += 1,
            }
        }
        let total = hist.total();
        (Some(total), hist)
    }

    fn initial_view_position(&mut self) -> (usize, usize) {
        let Some((file_path, line)) = self.pending_cursor.take() else {
            return (0, 0);
        };
        let State::CommitDiff { ref diff, .. } = self.state else {
            return (0, 0);
        };
        let file_idx = diff
            .files
            .iter()
            .position(|f| strip_controls(&f.display_path().to_string_lossy()) == file_path)
            .map(|pos| pos + 1)
            .unwrap_or(0);
        (file_idx, line)
    }

    fn poll_immediate_action(
        &mut self,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> std::result::Result<Option<ExtraScreenAction>, GgrError> {
        let Some(verdict) = self.pending_submit.take() else {
            return Ok(None);
        };
        Ok(Some(self.run_submit(verdict, ctx)))
    }
}

/// Convert a `GgrAnchor` to a `ComposerScope` for use in edit-mode composers.
///
/// `line_index` is the rendered cursor row, used when reconstructing a
/// `LineTarget`. `stack_available` is cloned for the `Pr` variant.
fn draft_anchor_to_scope(
    anchor: &crate::draft::GgrAnchor,
    line_index: usize,
    stack_available: Option<&StackContextSnapshot>,
) -> ComposerScope {
    match anchor {
        crate::draft::GgrAnchor::Line {
            file,
            old_line,
            new_line,
            hunk_header,
            target_text,
            context_before,
            context_after,
            ..
        } => ComposerScope::Line(LineTarget {
            file: std::path::PathBuf::from(file),
            rendered_index: line_index,
            source_line: *old_line,
            target_line: *new_line,
            target_text: target_text.clone(),
            hunk_header: hunk_header.clone(),
            context_before: context_before.clone(),
            context_after: context_after.clone(),
        }),
        crate::draft::GgrAnchor::Commit { .. } => ComposerScope::Change,
        crate::draft::GgrAnchor::Pr => {
            // stack_available is always Some at this call site; the
            // unwrap_or_else path is a lint-compliant fallback.
            ComposerScope::Stack(
                stack_available
                    .cloned()
                    .unwrap_or_else(|| StackContextSnapshot {
                        revset: String::new(),
                        revset_hash: RevsetHash::from_revset(""),
                    }),
            )
        }
    }
}

// ── ReplyComposerScreen ───────────────────────────────────────────────────────

/// Wraps a `Composer` for replying to an existing GitHub review comment.
/// Carries the `parent_comment_id` so `handle_reply_composer_key` can route
/// the save to `save_reply` instead of `save_comment`.
struct ReplyComposerScreen {
    composer: Box<Composer>,
    parent_comment_id: String,
}

impl ExtraScreen for ReplyComposerScreen {
    fn is_overlay(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ── GgrSurface reply helpers ──────────────────────────────────────────────────

impl GgrSurface {
    /// Open a reply composer when the cursor is on a `GitHubThread` line.
    fn open_reply_composer(
        &self,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let Some(view) = current_view else {
            return ExtraKeyAction::StatusMessage("no view loaded".to_owned());
        };
        let Some(row) = view.lines.get(line_index) else {
            return ExtraKeyAction::StatusMessage("cursor out of bounds".to_owned());
        };
        let RenderedLineKind::InlineCommentMeta { comment_index } = row.kind else {
            return ExtraKeyAction::StatusMessage("cursor is not on a comment thread".to_owned());
        };
        let CommentIndex::GitHubThread(enumerate_idx) = comment_index else {
            return ExtraKeyAction::StatusMessage(
                "cursor is not on a GitHub review thread".to_owned(),
            );
        };
        let Some(thread) = self.pr.review_threads.get(enumerate_idx) else {
            return ExtraKeyAction::StatusMessage("thread index out of bounds".to_owned());
        };

        let parent_comment_id = thread.root.id.to_string();
        let (change_id, commit_title) = self.draft_change_id_and_title();
        let stack_available = Some(StackContextSnapshot {
            revset: format!("PR #{}", self.pr.number),
            revset_hash: RevsetHash::from_revset(&format!("pr:{}", self.pr.number)),
        });
        let init = ComposerInit {
            scope: ComposerScope::Change,
            severity: self.last_severity.unwrap_or(Severity::Note),
            change_id,
            change_description: commit_title,
            line_available: None,
            stack_available,
            description_available: None,
        };
        ExtraKeyAction::OpenScreen(Box::new(ReplyComposerScreen {
            composer: Box::new(Composer::new(init)),
            parent_comment_id,
        }))
    }

    /// Handle key dispatch for the reply composer overlay.
    fn handle_reply_composer_key(
        &mut self,
        composer: &mut Composer,
        parent_comment_id: &str,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        use local_review_core::tui::composer::{handle_composer_key, ComposerAction};
        match handle_composer_key(composer, key) {
            ComposerAction::Continue => ExtraScreenAction::StayOpen,
            ComposerAction::Cancel => ExtraScreenAction::Close,
            ComposerAction::Save => match self.save_reply(composer, parent_comment_id) {
                Ok(msg) => {
                    self.last_severity = Some(composer.severity());
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::Close
                }
                Err(msg) => {
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::StayOpen
                }
            },
            ComposerAction::Delete => match self.delete_reply_via_composer(composer) {
                Ok(()) => ExtraScreenAction::Close,
                Err(msg) => {
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::StayOpen
                }
            },
            ComposerAction::RefusedScopeChord(status) => {
                *ctx.status_message = Some(status.to_owned());
                ExtraScreenAction::StayOpen
            }
        }
    }

    /// Persist a new or edited reply draft.
    fn save_reply(
        &mut self,
        composer: &Composer,
        parent_comment_id: &str,
    ) -> std::result::Result<String, String> {
        let body = composer.body_text();
        if body.trim().is_empty() {
            return Err("reply body is empty — not saved".to_owned());
        }

        // Edit path: update existing reply.
        if let Some(edit_ctx) = composer.editing() {
            let created_at_key = edit_ctx
                .identity
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|e| format!("timestamp format error: {e}"))?;
            let updated_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|e| format!("timestamp format error: {e}"))?;
            let Some(base) = crate::util::data_home() else {
                return Err("could not determine data directory".to_owned());
            };
            let host = self.pr.hostname.as_deref().unwrap_or("github.com");
            let slug = self.pr.repo_name.as_str();
            let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
            let path =
                crate::draft::replies_file_from_base(&base, host, owner, repo, self.pr.number);
            return match crate::draft::update_reply(
                &path,
                &created_at_key,
                &body,
                composer.severity(),
                &updated_at,
            ) {
                Ok(true) => {
                    self.reload_drafts();
                    Ok("reply updated".to_owned())
                }
                Ok(false) => Err("reply not found on disk".to_owned()),
                Err(e) => Err(format!("update failed: {}", strip_controls(&e.to_string()))),
            };
        }

        // New reply path.
        let Some(base) = crate::util::data_home() else {
            return Err(
                "could not determine data directory; XDG_DATA_HOME and HOME unset".to_owned(),
            );
        };
        let created_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| format!("timestamp format error: {e}"))?;
        let host = self
            .pr
            .hostname
            .as_deref()
            .unwrap_or("github.com")
            .to_owned();
        let slug = self.pr.repo_name.as_str();
        let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
        let params = crate::draft::ReplyParams {
            host,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            pr_number: self.pr.number,
            parent_comment_id: parent_comment_id.to_owned(),
            body: body.clone(),
            severity: composer.severity(),
            created_at,
        };
        let reply = crate::draft::GgrReply::new(&params)
            .map_err(|e| format!("save failed: {}", strip_controls(&e.to_string())))?;
        let path = crate::draft::replies_file_from_base(
            &base,
            &reply.host,
            &reply.owner,
            &reply.repo,
            reply.pr_number,
        );
        crate::draft::append_reply(&path, &reply)
            .map_err(|e| format!("save failed: {}", strip_controls(&e.to_string())))?;
        self.reload_drafts();
        Ok("reply draft saved".to_owned())
    }

    /// Open the reply composer in edit mode for an existing reply draft.
    fn open_edit_reply_composer(&self, reply_idx: usize, line_index: usize) -> ExtraKeyAction {
        let Some(reply) = self.loaded_replies.get(reply_idx) else {
            return ExtraKeyAction::StatusMessage(
                "reply draft not found — navigate away and back to refresh".to_owned(),
            );
        };
        let Ok(identity_dt) = time::OffsetDateTime::parse(
            &reply.created_at,
            &time::format_description::well_known::Rfc3339,
        ) else {
            return ExtraKeyAction::StatusMessage(
                "reply draft has invalid timestamp — cannot edit".to_owned(),
            );
        };
        let (change_id, commit_title) = self.draft_change_id_and_title();
        let stack_available = Some(StackContextSnapshot {
            revset: format!("PR #{}", self.pr.number),
            revset_hash: RevsetHash::from_revset(&format!("pr:{}", self.pr.number)),
        });
        let init = ComposerInit {
            scope: ComposerScope::Change,
            severity: reply.severity,
            change_id,
            change_description: commit_title,
            line_available: None,
            stack_available,
            description_available: None,
        };
        let edited = EditedComment {
            init,
            body: reply.body.clone(),
            identity: identity_dt,
            comment_index: Some(line_index),
        };
        ExtraKeyAction::OpenScreen(Box::new(ReplyComposerScreen {
            composer: Box::new(Composer::for_edit(edited)),
            parent_comment_id: reply.parent_comment_id.clone(),
        }))
    }

    /// Delete a reply draft via the edit composer's identity.
    fn delete_reply_via_composer(
        &mut self,
        composer: &Composer,
    ) -> std::result::Result<(), String> {
        let Some(edit_ctx) = composer.editing() else {
            return Err("nothing to delete — composer is not in edit mode".to_owned());
        };
        let created_at_key = edit_ctx
            .identity
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| format!("timestamp format error: {e}"))?;
        let Some(base) = crate::util::data_home() else {
            return Err("could not determine data directory".to_owned());
        };
        let host = self.pr.hostname.as_deref().unwrap_or("github.com");
        let slug = self.pr.repo_name.as_str();
        let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
        let path = crate::draft::replies_file_from_base(&base, host, owner, repo, self.pr.number);
        match crate::draft::delete_reply(&path, |r| r.created_at == created_at_key) {
            Ok(true) => {
                self.reload_drafts();
                Ok(())
            }
            Ok(false) => Err("reply not found — already deleted?".to_owned()),
            Err(e) => Err(format!("delete failed: {}", strip_controls(&e.to_string()))),
        }
    }
}

// ── StalePanel ────────────────────────────────────────────────────────────────

/// Overlay listing all stale drafts with their mismatch reasons.
///
/// Opened automatically on startup when stale drafts are detected, and
/// reachable via the status bar. Press `d` to delete the focused stale draft,
/// Esc to dismiss.
struct StalePanel {
    items: Vec<StalePanelItem>,
    cursor: usize,
}

struct StalePanelItem {
    created_at: String,
    anchor_desc: String,
    reason: String,
    is_reply: bool,
}

impl StalePanel {
    fn new(drafts: &[crate::draft::GgrDraft], replies: &[crate::draft::GgrReply]) -> Self {
        let mut items = Vec::new();
        for d in drafts {
            if d.status != Some(crate::draft::DraftStatus::Stale) {
                continue;
            }
            let anchor_desc = match &d.anchor {
                crate::draft::GgrAnchor::Line {
                    file,
                    new_line,
                    old_line,
                    ..
                } => {
                    let line = new_line
                        .map(|l| (l, "new"))
                        .or_else(|| old_line.map(|l| (l, "old")));
                    match line {
                        Some((n, s)) => format!("{file}:{n} ({s})"),
                        None => file.clone(),
                    }
                }
                crate::draft::GgrAnchor::Commit { commit_sha } => {
                    format!("commit {}", &commit_sha.as_str()[..8])
                }
                crate::draft::GgrAnchor::Pr => "PR-scope".to_owned(),
            };
            items.push(StalePanelItem {
                created_at: d.created_at.clone(),
                anchor_desc,
                reason: d.mismatch_reason.clone().unwrap_or_default(),
                is_reply: false,
            });
        }
        for r in replies {
            if r.status != Some(crate::draft::DraftStatus::Stale) {
                continue;
            }
            items.push(StalePanelItem {
                created_at: r.created_at.clone(),
                anchor_desc: format!("reply to {}", r.parent_comment_id),
                reason: r.mismatch_reason.clone().unwrap_or_default(),
                is_reply: true,
            });
        }
        Self { items, cursor: 0 }
    }
}

impl ExtraScreen for StalePanel {
    fn is_overlay(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn render_stale_panel(frame: &mut Frame<'_>, panel: &StalePanel) {
    use ratatui::style::Modifier;
    use ratatui::text::Span;

    let area = frame.area();
    let [_, col, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(60.min(area.width.saturating_sub(4))),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(area);
    let [_, modal, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(18.min(area.height.saturating_sub(4))),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(col);

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(format!(" Stale drafts ({}) ", panel.items.len()));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    if panel.items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No stale drafts.").style(Style::default()),
            inner,
        );
        return;
    }

    let mut lines: Vec<TuiLine<'_>> = Vec::new();
    lines.push(TuiLine::raw("  d — delete focused   Esc — dismiss"));
    lines.push(TuiLine::raw(""));
    for (idx, item) in panel.items.iter().enumerate() {
        let selected = idx == panel.cursor;
        let prefix = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(TuiLine::from(Span::styled(
            format!("{prefix}{}", item.anchor_desc),
            style,
        )));
        lines.push(TuiLine::from(Span::styled(
            format!("    reason: {}", item.reason),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

// ── VerdictScreen ─────────────────────────────────────────────────────────────

/// Modal that prompts the reviewer to choose a submit verdict.
///
/// Keys: `a` → Approve, `r` → Request changes, `c`/Enter → Comment (default),
/// Esc → cancel. Each key immediately triggers submit.
struct VerdictScreen;

impl ExtraScreen for VerdictScreen {
    fn is_overlay(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn render_verdict_screen(frame: &mut Frame<'_>) {
    let area = frame.area();
    let [_, center, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(46),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(area);
    let [_, modal, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(center);

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Submit Review ");
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let lines = vec![
        TuiLine::raw(""),
        TuiLine::raw("  [a]  Approve"),
        TuiLine::raw("  [r]  Request changes"),
        TuiLine::raw("  [c]  Comment  (default)"),
        TuiLine::raw(""),
        TuiLine::raw("  Esc  Cancel"),
        TuiLine::raw(""),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants intentionally ignored in verdict modal"
)]
fn verdict_from_key(code: KeyCode) -> Option<crate::submit::Verdict> {
    use crate::submit::Verdict;
    match code {
        KeyCode::Char('a' | 'A') => Some(Verdict::Approve),
        KeyCode::Char('r' | 'R') => Some(Verdict::RequestChanges),
        KeyCode::Char('c' | 'C') | KeyCode::Enter => Some(Verdict::Comment),
        _ => None,
    }
}

// ── SubmittingOverlay ─────────────────────────────────────────────────────────

/// Overlay rendered while a submit is in flight.
///
/// One frame of this is drawn by `run_app` before `poll_immediate_action`
/// fires and executes the actual blocking network call.
struct SubmittingOverlay;

impl ExtraScreen for SubmittingOverlay {
    fn is_overlay(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn render_submitting_overlay(frame: &mut Frame<'_>) {
    let area = frame.area();
    let [_, center, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(36),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(area);
    let [_, modal, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(4),
        Constraint::Fill(1),
    ])
    .flex(Flex::Center)
    .areas(center);

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Submit Review ");
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    frame.render_widget(
        Paragraph::new(TuiLine::raw("  Submitting…")).alignment(Alignment::Left),
        inner,
    );
}

// ── GgrSurface submit helpers ─────────────────────────────────────────────────

impl GgrSurface {
    /// Re-fetch PR state and re-run the re-anchor pass in-place.
    fn run_refresh(&mut self) -> ExtraKeyAction {
        let pr = match gh::fetch_pr_details(
            self.pr.number,
            Some(self.pr.repo_name.as_str()),
            self.pr.hostname.as_deref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                return ExtraKeyAction::StatusMessage(format!(
                    "refresh failed: {}",
                    strip_controls(&e.to_string())
                ));
            }
        };
        if let Some(base) = crate::util::data_home() {
            let stale = crate::reanchor::reanchor_all(&pr, &base);
            self.pr = pr;
            self.reload_drafts();
            if stale > 0 {
                let panel = StalePanel::new(&self.loaded_drafts, &self.loaded_replies);
                return ExtraKeyAction::OpenScreen(Box::new(panel));
            }
            ExtraKeyAction::StatusMessage("refreshed".to_owned())
        } else {
            self.pr = pr;
            self.reload_drafts();
            ExtraKeyAction::StatusMessage("refreshed".to_owned())
        }
    }

    /// Delete a single stale item from disk (called from stale panel `d` key).
    fn delete_stale_item(&mut self, item: &StalePanelItem, ctx: &mut ExtraScreenContext<'_>) {
        let Some(base) = crate::util::data_home() else {
            return;
        };
        let host = self.pr.hostname.as_deref().unwrap_or("github.com");
        let slug = self.pr.repo_name.as_str();
        let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
        let created_at = &item.created_at;
        let result = if item.is_reply {
            let path =
                crate::draft::replies_file_from_base(&base, host, owner, repo, self.pr.number);
            crate::draft::delete_reply(&path, |r| &r.created_at == created_at).map(|_| ())
        } else {
            let drafts_dir =
                crate::draft::drafts_dir_from_base(&base, host, owner, repo, self.pr.number);
            // Try each commit file to find the matching draft.
            let mut deleted = false;
            for commit in &self.pr.commits {
                let path = drafts_dir.join(format!("{}.jsonl", commit.sha.as_str()));
                if let Ok(true) = crate::draft::delete_draft(&path, |d| &d.created_at == created_at)
                {
                    deleted = true;
                    break;
                }
            }
            // Also try _pr.jsonl.
            if !deleted {
                let pr_path = drafts_dir.join("_pr.jsonl");
                let _ = crate::draft::delete_draft(&pr_path, |d| &d.created_at == created_at);
            }
            Ok(())
        };
        if let Err(e) = result {
            *ctx.status_message =
                Some(format!("delete failed: {}", strip_controls(&e.to_string())));
        } else {
            self.reload_drafts();
        }
    }

    fn run_submit(
        &mut self,
        verdict: crate::submit::Verdict,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        let Some(base) = crate::util::data_home() else {
            *ctx.status_message =
                Some("submit failed: could not determine data directory".to_owned());
            return ExtraScreenAction::Close;
        };
        match crate::submit::submit(&self.pr, verdict, &base) {
            Ok(outcome) => {
                // Re-fetch the PR so submitted comments appear as GitHub
                // review threads immediately instead of disappearing.
                if let Ok(fresh) = gh::fetch_pr_details(
                    self.pr.number,
                    Some(self.pr.repo_name.as_str()),
                    self.pr.hostname.as_deref(),
                ) {
                    self.pr = fresh;
                }
                self.reload_drafts();
                *ctx.status_message = Some(outcome.message);
            }
            Err(e) => {
                *ctx.status_message =
                    Some(format!("submit failed: {}", strip_controls(&e.to_string())));
            }
        }
        ExtraScreenAction::Close
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

pub(crate) fn run(pr: PrDetails, stale_count: usize, allow_graph_clone: bool) -> Result<()> {
    let size = crossterm::terminal::size().map_err(|source| GgrError::Io { source })?;
    if size.0 < MIN_COLS {
        return Err(GgrError::TerminalTooNarrow { cols: size.0 });
    }
    if size.1 < MIN_ROWS {
        return Err(GgrError::TerminalTooShort { rows: size.1 });
    }
    let cursor_path = cursor::cursor_path(&pr);
    let initial_cursor = cursor_path.as_deref().and_then(cursor::load);
    let mut surface = GgrSurface::new(pr, initial_cursor.as_ref(), allow_graph_clone);
    // Eager clone at PR open: by the time extraction wants a graph, the
    // clone (checked out at the PR head SHA) is usually already there.
    surface.start_graph_clone();
    if stale_count > 0 {
        surface.pending_stale_message = Some(format!(
            "{stale_count} stale draft{} — press R to review",
            if stale_count == 1 { "" } else { "s" }
        ));
    }
    let mut app = App::new(surface, vec![], TransitionMode::Auto);
    let (mut terminal, _guard) = enter_tui()?;
    core_run_app(&mut terminal, &mut app, |app| {
        if let Some(ref path) = cursor_path {
            if let Some(state) = app
                .surface
                .current_cursor_state(app.file_index(), app.line_index())
            {
                if let Err(e) = cursor::save(path, &state) {
                    let mut stderr = std::io::stderr().lock();
                    let _ = writeln!(stderr, "ggr: warning: failed to save cursor: {e}");
                }
            }
        }
    })
    .map_err(|e| match e {
        AppError::Io(source) => GgrError::Io { source },
        AppError::Surface(e) => e,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::{CommitEntry, CommitSha, PrComment, RepoName, ReviewThread, ThreadComment};

    #[test]
    fn ggr_footer_text_full_width_contains_key_hints() {
        let text = ggr_footer_text_for_width(200, None);
        assert!(
            text.contains("c comment"),
            "full footer must mention c comment"
        );
        assert!(
            text.contains("S submit"),
            "full footer must mention S submit"
        );
        assert!(
            text.contains("R refresh"),
            "full footer must mention R refresh"
        );
    }

    #[test]
    fn ggr_footer_text_narrow_drops_to_minimal() {
        let text = ggr_footer_text_for_width(40, None);
        assert!(
            text.contains("S submit"),
            "narrow footer must still mention submit"
        );
    }

    #[test]
    fn ggr_footer_text_badge_appended_when_filter_active() {
        let text = ggr_footer_text_for_width(200, Some(Severity::Required));
        assert!(
            text.contains("[F:required]"),
            "badge must appear with active filter"
        );
    }
    use crossterm::event::{KeyCode, KeyModifiers};
    use local_review_core::change_id::ChangeId;
    use local_review_core::diff::{Diff, DiffFile};
    use local_review_core::tui::{
        composer::{ComposerScope, DescriptionContext},
        CommentId,
    };
    use serial_test::serial;

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
                    body: "Extracts validation so the sweeper can reuse it.".to_owned(),
                },
                CommitEntry {
                    sha: CommitSha::try_from("b3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b4").unwrap(),
                    short_sha: "b3b4c5d6".to_owned(),
                    title: "Second commit".to_owned(),
                    body: String::new(),
                },
            ],
            review_threads: vec![],
        }
    }

    #[test]
    fn entry_count_includes_description_entry() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.entry_count(), 3, "2 commits + 1 description entry");
    }

    #[test]
    fn entry_count_single_commit() {
        let mut pr = make_pr();
        pr.commits.truncate(1);
        let surface = GgrSurface::new(pr, None, false);
        assert_eq!(surface.entry_count(), 2);
    }

    #[test]
    fn entry_id_display_returns_overview_for_index_0() {
        let surface = GgrSurface::new(make_pr(), None, false);
        // Entry 0 defaults to description pane; the label reflects the pane.
        assert_eq!(surface.entry_id_display(0), "overview");
    }

    // ── graph clone state (phase 4) ───────────────────────────────────────

    #[test]
    fn no_graph_flag_reports_opt_out_and_serves_no_path() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(
            surface.graph_unavailable_reason().as_deref(),
            Some("--no-graph")
        );
        assert!(surface.ready_repo_path().is_none());
    }

    #[test]
    fn zero_commit_pr_reports_no_commits_and_start_is_noop() {
        let surface = GgrSurface::new(make_pr_zero_commits(), None, true);
        // No head SHA exists; start must not spawn a clone thread.
        surface.start_graph_clone();
        assert_eq!(
            surface.graph_unavailable_reason().as_deref(),
            Some("PR has no commits")
        );
    }

    #[test]
    fn in_flight_clone_reports_progress_then_ready_serves_path() {
        let surface = GgrSurface::new(make_pr(), None, true);
        // Constructed InProgress; clone thread not started in this test.
        assert_eq!(
            surface.graph_unavailable_reason().as_deref(),
            Some("clone in progress")
        );
        assert!(surface.ready_repo_path().is_none());

        let path = std::path::PathBuf::from("/tmp/ggr-repos/x");
        if let Ok(mut state) = surface.graph_clone.lock() {
            *state = GraphCloneState::Ready(path.clone());
        }
        assert_eq!(surface.graph_unavailable_reason(), None);
        assert_eq!(surface.ready_repo_path(), Some(path));
    }

    #[test]
    fn clone_status_maps_disabled_and_failed_to_unavailable() {
        use crate::repo_cache::CloneStatus;
        let ready = GraphCloneState::from_status(CloneStatus::Ready("/x".into()));
        assert!(matches!(ready, GraphCloneState::Ready(_)));
        let disabled = GraphCloneState::from_status(CloneStatus::Disabled("--no-graph".to_owned()));
        assert!(matches!(disabled, GraphCloneState::Unavailable(ref r) if r == "--no-graph"));
        let failed = GraphCloneState::from_status(CloneStatus::Failed("clone failed".to_owned()));
        assert!(matches!(failed, GraphCloneState::Unavailable(ref r) if r == "clone failed"));
    }

    #[test]
    fn stack_bar_overview_has_label_and_no_progress() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        // Land on the overview: clear the entity-first initial index.
        surface.pending_initial_index = None;
        let spec = surface.stack_bar_spec();
        assert_eq!(spec.title, "Pull Request");
        assert!(
            spec.progress.is_none(),
            "overview has no position among the commits"
        );
        assert_eq!(spec.label, "overview  PR #42");
    }

    #[test]
    fn stack_bar_commits_numbered_without_the_overview() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        surface.pending_initial_index = None;
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff { files: vec![] },
        };
        let spec = surface.stack_bar_spec();
        // Entry 1 is the FIRST commit of two: `commit 1/2`, not `2/3` —
        // the overview must not shift commit numbering.
        assert_eq!(spec.label, "commit 1/2  a3b4c5d6");
        assert_eq!(spec.progress, Some((1, 2)));
        assert_eq!(spec.title, "Pull Request");
    }

    #[test]
    fn entry_id_display_returns_short_sha_for_commits() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.entry_id_display(1), "a3b4c5d6");
        assert_eq!(surface.entry_id_display(2), "b3b4c5d6");
    }

    #[test]
    fn entry_id_display_out_of_range_returns_empty() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.entry_id_display(99), "");
    }

    #[test]
    fn entry_description_returns_pr_title_for_index_0() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.entry_description(0), "PR title");
    }

    #[test]
    fn entry_description_returns_commit_title() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.entry_description(1), "First commit");
    }

    #[test]
    fn entry_description_strips_control_chars_from_pr_title() {
        let mut pr = make_pr();
        pr.title = "\x1b[31mevil\x1b[0m".to_owned();
        let surface = GgrSurface::new(pr, None, false);
        let desc = surface.entry_description(0);
        assert!(
            !desc.chars().any(char::is_control),
            "entry_description must strip control chars; got: {desc:?}"
        );
    }

    #[test]
    fn entry_description_out_of_range_returns_empty() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.entry_description(99), "");
    }

    #[test]
    fn description_summary_peeks_pr_body_on_entry_zero() {
        let surface = GgrSurface::new(make_pr(), None, false);
        let s = surface.fetch_description_summary(0).unwrap();
        assert_eq!(s.subject, "PR title");
        assert_eq!(s.body_peek.as_deref(), Some("PR body"));
    }

    #[test]
    fn description_summary_peeks_commit_body_and_strips_controls() {
        let mut pr = make_pr();
        pr.commits[0].body = "\n\x1b[31mfirst body line\x1b[0m\nmore".to_owned();
        let surface = GgrSurface::new(pr, None, false);
        let s = surface.fetch_description_summary(1).unwrap();
        assert_eq!(s.subject, "First commit");
        let peek = s.body_peek.expect("commit body must yield a peek");
        assert!(
            !peek.chars().any(char::is_control),
            "peek must strip control chars; got: {peek:?}"
        );
        assert!(peek.contains("first body line"));
    }

    #[test]
    fn description_summary_empty_commit_body_yields_no_peek() {
        let surface = GgrSurface::new(make_pr(), None, false);
        let s = surface.fetch_description_summary(2).unwrap();
        assert_eq!(s.body_peek, None);
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
        let mut surface = GgrSurface::new(pr, None, false);
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
        let surface = GgrSurface::new(make_pr(), None, false);
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 0, None)
            .is_empty());
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
            .is_empty());
    }

    #[test]
    fn threads_expanded_defaults_to_true() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert!(
            surface.threads_expanded,
            "threads_expanded must be true after new()"
        );
    }

    #[test]
    fn handle_extra_key_t_expands_then_collapses() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        assert!(surface.threads_expanded);

        let result = surface
            .handle_extra_key(make_key(KeyCode::Char('T')), 0, 0, None)
            .unwrap();
        assert!(!surface.threads_expanded, "T must collapse when expanded");
        assert!(
            matches!(result, ExtraKeyAction::StatusMessage(_)),
            "T must return StatusMessage"
        );

        let result = surface
            .handle_extra_key(make_key(KeyCode::Char('T')), 0, 0, None)
            .unwrap();
        assert!(surface.threads_expanded, "T must expand when collapsed");
        assert!(
            matches!(result, ExtraKeyAction::StatusMessage(_)),
            "T must return StatusMessage"
        );
    }

    #[test]
    fn handle_extra_key_unknown_returns_ignored() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        let result = surface
            .handle_extra_key(make_key(KeyCode::Char('x')), 0, 0, None)
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
        let surface = GgrSurface::new(make_pr(), None, false);
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
    fn inline_comments_for_view_comment_index_is_github_thread_variant() {
        // GitHub thread indices are encoded as CommentIndex::GitHubThread(enumerate_idx).
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
        let mut surface = GgrSurface::new(pr, None, false);
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
        // thread_match is at enumerate_idx=1 (second thread in the vec).
        assert_eq!(
            comments[0].comment_index,
            CommentIndex::GitHubThread(1),
            "comment_index for GitHub threads is CommentIndex::GitHubThread(enumerate_idx)"
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
    fn inline_comments_for_view_renders_thread_replies_in_body() {
        // A GitHub PR thread with replies from other reviewers must surface
        // the reply bodies inline below the root, not silently drop them.
        let mut thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "fix this typo",
            "2024-01-15T10:30:00Z",
        );
        thread.replies.push(ThreadComment {
            id: 2,
            author: "octocat".to_owned(),
            created_at: "2024-01-15T11:00:00Z".to_owned(),
            body: "good catch".to_owned(),
        });
        thread.replies.push(ThreadComment {
            id: 3,
            author: "hubot".to_owned(),
            created_at: "2024-01-15T12:00:00Z".to_owned(),
            body: "fixed in next commit".to_owned(),
        });
        let surface = make_surface_with_thread(thread);
        let comments = surface.inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None);
        assert_eq!(comments.len(), 1, "one InlineComment per thread");
        let body = comments[0].body_lines.join("\n");
        assert!(
            body.contains("fix this typo"),
            "root body present: {body:?}"
        );
        assert!(
            body.contains("good catch"),
            "reply 1 body present: {body:?}"
        );
        assert!(
            body.contains("fixed in next commit"),
            "reply 2 body present: {body:?}"
        );
        assert!(
            body.contains("@octocat"),
            "reply 1 author present: {body:?}"
        );
        assert!(body.contains("@hubot"), "reply 2 author present: {body:?}");
    }

    #[test]
    fn inline_comments_for_view_includes_pending_reply_for_matching_thread() {
        // A locally-drafted reply to an existing GitHub thread must surface
        // in the inline view alongside the thread itself, anchored to the
        // same diff line.
        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "review note",
            "2024-01-15T10:30:00Z",
        );
        let thread_id = thread.root.id;
        let mut surface = make_surface_with_thread(thread);

        let reply = crate::draft::GgrReply::new(&crate::draft::ReplyParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 1,
            parent_comment_id: thread_id.to_string(),
            body: "thanks, fixed".to_owned(),
            severity: Severity::Note,
            created_at: "2024-01-16T09:00:00Z".to_owned(),
        })
        .unwrap();
        surface.loaded_replies.push(reply);

        let comments = surface.inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None);
        assert!(
            comments
                .iter()
                .any(|c| matches!(c.comment_index, CommentIndex::LocalReply(_))),
            "pending reply must appear in the inline view: {:?}",
            comments
                .iter()
                .map(|c| (c.comment_index, &c.body_lines))
                .collect::<Vec<_>>()
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
        let mut surface = GgrSurface::new(make_pr(), None, false);
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

    // ── save_comment tests ────────────────────────────────────────────────────

    fn set_commit_diff_state(surface: &mut GgrSurface) {
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
    }

    #[test]
    fn save_comment_empty_body_returns_refused() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "   ",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Refused { .. }),
            "empty body must return Refused"
        );
    }

    #[test]
    fn save_comment_description_scope_returns_refused() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let scope = ComposerScope::Description(DescriptionContext {
            change_id: ChangeId::parse("a3b4c5d6").unwrap(),
            target_line: None,
            target_text: String::new(),
            context_before: vec![],
            context_after: vec![],
        });
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "some text",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Refused { .. }),
            "description scope must return Refused"
        );
    }

    #[test]
    #[serial]
    fn save_comment_commit_scope_writes_draft() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "commit-level review note",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "commit-scope save must succeed; got: {outcome:?}"
        );
        assert_eq!(
            surface.loaded_drafts.len(),
            1,
            "one draft must be loaded after save"
        );
        assert!(
            matches!(
                surface.loaded_drafts[0].anchor,
                crate::draft::GgrAnchor::Commit { .. }
            ),
            "anchor must be Commit"
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    #[serial]
    fn save_comment_pr_scope_writes_draft() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        // PR scope does not require CommitDiff state.
        let stack = StackContextSnapshot {
            revset: "PR #42".to_owned(),
            revset_hash: RevsetHash::from_revset("pr:42"),
        };
        let scope = ComposerScope::Stack(stack);
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Suggestion,
            body: "overall PR comment",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "PR-scope save must succeed; got: {outcome:?}"
        );
        // Verify the _pr.jsonl file was written to disk.
        let base = dir.path();
        let drafts_dir =
            crate::draft::drafts_dir_from_base(base, "github.com", "owner", "repo", 42);
        assert!(
            drafts_dir.join("_pr.jsonl").exists(),
            "_pr.jsonl must exist after PR-scope save"
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    #[serial]
    fn save_comment_line_scope_writes_draft() {
        use local_review_core::tui::composer::LineTarget;

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);

        let target = LineTarget {
            file: std::path::PathBuf::from("src/foo.rs"),
            rendered_index: 3,
            source_line: None,
            target_line: Some(42),
            target_text: "let x = 1;".to_owned(),
            hunk_header: "@@ -40,6 +40,7 @@".to_owned(),
            context_before: vec!["fn foo() {".to_owned()],
            context_after: vec!["}".to_owned()],
        };
        let scope = ComposerScope::Line(target);
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Required,
            body: "fix this",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "line-scope save must succeed; got: {outcome:?}"
        );
        assert_eq!(surface.loaded_drafts.len(), 1);
        assert!(
            matches!(
                surface.loaded_drafts[0].anchor,
                crate::draft::GgrAnchor::Line { ref file, new_line: Some(42), .. }
                if file == "src/foo.rs"
            ),
            "draft anchor must be Line with correct file and line"
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    // Regression: user reported a line-scope draft disappearing immediately
    // after Ctrl-X save in ggr. Save succeeded (status bar showed "line
    // draft saved") and the draft was on disk (re-opening composer found
    // it). This test mirrors the user's flow end-to-end and exercises the
    // full pipeline up through `with_inline_comments` injection.
    #[test]
    #[serial]
    fn save_line_draft_then_view_contains_inline_meta() {
        use local_review_core::diff::{Hunk, Line, LineKind};
        use local_review_core::tui::composer::LineTarget;
        use local_review_core::tui::diff_view::{DiffView, RenderedLineKind};

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        // Real-shape diff: one file with a hunk that contains line 42 in the
        // after-state, so `with_inline_comments` has a row to match against.
        let hunk = Hunk {
            header: "@@ -40,6 +40,7 @@".to_owned(),
            function_context: None,
            source_start: 40,
            source_length: 3,
            target_start: 40,
            target_length: 3,
            lines: vec![
                Line {
                    kind: LineKind::Context,
                    text: "fn foo() {".to_owned(),
                    source_line: Some(40),
                    target_line: Some(40),
                },
                Line {
                    kind: LineKind::Context,
                    text: "    let x = 1;".to_owned(),
                    source_line: Some(41),
                    target_line: Some(42),
                },
                Line {
                    kind: LineKind::Context,
                    text: "}".to_owned(),
                    source_line: Some(42),
                    target_line: Some(43),
                },
            ],
        };
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![hunk.clone()],
                }],
            },
        };

        let target = LineTarget {
            file: std::path::PathBuf::from("src/foo.rs"),
            rendered_index: 3,
            source_line: None,
            target_line: Some(42),
            target_text: "    let x = 1;".to_owned(),
            hunk_header: "@@ -40,6 +40,7 @@".to_owned(),
            context_before: vec!["fn foo() {".to_owned()],
            context_after: vec!["}".to_owned()],
        };
        let scope = ComposerScope::Line(target);
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Required,
            body: "fix this",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "save: {outcome:?}"
        );

        // Step 1: surface returns the new draft as an inline comment.
        let now = std::time::SystemTime::now();
        let inline = surface.inline_comments_for_view(now, 1, None);
        assert_eq!(
            inline.len(),
            1,
            "inline_comments_for_view must return the draft"
        );

        // Step 2: a fresh DiffView built from the diff file, with comments
        // injected, contains an `InlineCommentMeta` row below line 42.
        let file = match &surface.state {
            State::CommitDiff { diff, .. } => &diff.files[0],
            State::Description => panic!("state must be CommitDiff"),
        };
        let view = DiffView::from_file(file).with_inline_comments(&inline);
        let meta_position = view
            .lines
            .iter()
            .position(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }));
        assert!(
            meta_position.is_some(),
            "with_inline_comments must inject an InlineCommentMeta row; got {:?}",
            view.lines.iter().map(|l| &l.kind).collect::<Vec<_>>()
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    #[serial]
    fn save_line_draft_then_inline_comments_returns_it() {
        use local_review_core::tui::composer::LineTarget;

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);

        let target = LineTarget {
            file: std::path::PathBuf::from("src/foo.rs"),
            rendered_index: 3,
            source_line: None,
            target_line: Some(42),
            target_text: "let x = 1;".to_owned(),
            hunk_header: "@@ -40,6 +40,7 @@".to_owned(),
            context_before: vec!["fn foo() {".to_owned()],
            context_after: vec!["}".to_owned()],
        };
        let scope = ComposerScope::Line(target);
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Required,
            body: "fix this",
            entry_idx: 0,
        };
        let outcome = surface.save_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "save: {outcome:?}"
        );
        assert_eq!(surface.loaded_drafts.len(), 1, "draft must be in memory");

        // view_idx=1 → first diff file (view_idx=0 is the description sub-view).
        let now = std::time::SystemTime::now();
        let inline = surface.inline_comments_for_view(now, 1, None);
        assert_eq!(
            inline.len(),
            1,
            "inline_comments_for_view must return the just-saved draft; got {inline:?}"
        );
        assert_eq!(
            inline[0].target_line,
            Some(42),
            "inline comment must point at the after-state line; got {:?}",
            inline[0]
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    #[serial]
    fn update_comment_changes_body() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        // Save a commit-scope draft first.
        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "original body",
            entry_idx: 0,
        };
        surface.save_comment(req).unwrap();
        assert_eq!(surface.loaded_drafts.len(), 1);
        let identity = time::OffsetDateTime::parse(
            &surface.loaded_drafts[0].created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();

        let update_req = UpdateRequest {
            identity: CommentId::new(identity),
            body: "updated body",
            severity: Severity::Required,
            oversized: false,
        };
        let outcome = surface.update_comment(update_req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "update must succeed; got: {outcome:?}"
        );
        assert_eq!(surface.loaded_drafts[0].body, "updated body");
        assert_eq!(surface.loaded_drafts[0].severity, Severity::Required);

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn update_comment_not_found_returns_refused() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let identity = CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = UpdateRequest {
            identity,
            body: "new body",
            severity: Severity::Note,
            oversized: false,
        };
        let outcome = surface.update_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Refused { .. }),
            "update with unknown identity must return Refused"
        );
    }

    #[test]
    fn update_comment_empty_body_returns_refused() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let identity = CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = UpdateRequest {
            identity,
            body: "",
            severity: Severity::Note,
            oversized: false,
        };
        let outcome = surface.update_comment(req).unwrap();
        assert!(
            matches!(outcome, SaveOutcome::Refused { .. }),
            "empty body must return Refused"
        );
    }

    #[test]
    #[serial]
    fn delete_comment_removes_draft() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "to be deleted",
            entry_idx: 0,
        };
        surface.save_comment(req).unwrap();
        assert_eq!(surface.loaded_drafts.len(), 1);
        let identity = time::OffsetDateTime::parse(
            &surface.loaded_drafts[0].created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();

        let del_req = DeleteRequest::new(CommentId::new(identity), None);
        let outcome = surface.delete_comment(del_req).unwrap();
        assert!(
            matches!(outcome, DeleteOutcome::Deleted),
            "delete must succeed; got: {outcome:?}"
        );
        assert!(
            surface.loaded_drafts.is_empty(),
            "loaded_drafts must be empty after delete"
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn delete_comment_not_found_returns_refused() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);
        let identity = CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = DeleteRequest::new(identity, None);
        let outcome = surface.delete_comment(req).unwrap();
        assert!(
            matches!(outcome, DeleteOutcome::Refused { .. }),
            "delete with unknown identity must return Refused"
        );
    }

    #[test]
    #[serial]
    fn severity_histogram_counts_loaded_drafts() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);

        let scope = ComposerScope::Change;
        surface
            .save_comment(SaveRequest {
                scope: &scope,
                severity: Severity::Note,
                body: "note comment",
                entry_idx: 0,
            })
            .unwrap();
        surface
            .save_comment(SaveRequest {
                scope: &scope,
                severity: Severity::Required,
                body: "required comment",
                entry_idx: 0,
            })
            .unwrap();

        let hist = surface.severity_histogram();
        assert_eq!(hist.note, 1);
        assert_eq!(hist.required, 1);
        assert_eq!(hist.suggestion, 0);

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn is_view_reviewed_returns_false() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert!(!surface.is_view_reviewed(0));
        assert!(!surface.is_view_reviewed(1));
    }

    #[test]
    fn severity_histogram_returns_default_with_no_drafts() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.severity_histogram(), SeverityHistogram::default());
    }

    #[test]
    fn help_screen_title_is_ggr() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.help_screen_title(), "ggr · keybindings");
    }

    #[test]
    fn pr_description_text_includes_title_body_and_comments() {
        let mut pr = make_pr();
        pr.comments.push(PrComment {
            author: "alice".to_owned(),
            body: "great PR!".to_owned(),
        });
        let surface = GgrSurface::new(pr, None, false);
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
        let surface = GgrSurface::new(pr, None, false);
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
        let surface = GgrSurface::new(pr, None, false);
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
        let surface = GgrSurface::new(make_pr(), None, false);
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
        let mut surface = GgrSurface::new(make_pr(), None, false);
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
        let mut surface = GgrSurface::new(make_pr(), None, false);
        let count = surface.entry_count();
        let result = surface.fetch_views(count);
        assert!(result.is_err(), "out-of-range fetch_views must return Err");
    }

    #[test]
    fn pr_description_text_with_empty_body_has_no_extra_newlines() {
        let mut pr = make_pr();
        pr.body = String::new();
        let surface = GgrSurface::new(pr, None, false);
        let text = surface.pr_description_text();
        assert_eq!(
            text, "PR title",
            "empty body must not add trailing newlines"
        );
    }

    #[test]
    #[serial]
    fn delete_at_cursor_returns_refresh_and_status_on_success() {
        use local_review_core::tui::diff_view::{CommentIndex, RenderedLine, RenderedLineKind};
        use local_review_core::tui::ExtraKeyAction;

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);

        // Save a draft so there's something to delete.
        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "to be deleted",
            entry_idx: 0,
        };
        surface.save_comment(req).unwrap();
        assert_eq!(surface.loaded_drafts.len(), 1);

        // Build a fake view with an InlineCommentMeta pointing at draft 0.
        let meta_line = RenderedLine {
            kind: RenderedLineKind::InlineCommentMeta {
                comment_index: CommentIndex::Local(0),
            },
            text: "┃ ● note".to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        };
        let view = DiffView {
            title: "test".to_owned(),
            lines: vec![meta_line],
            paired_rows: vec![],
            token_spans: std::collections::HashMap::new(),
        };

        let action = surface.delete_at_cursor(0, Some(&view));
        assert!(
            matches!(action, ExtraKeyAction::RefreshAndStatus(_)),
            "delete_at_cursor must return RefreshAndStatus so the core rebuilds the view"
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    #[serial]
    fn enter_on_inline_comment_meta_opens_edit_composer_with_draft_body() {
        use local_review_core::tui::diff_view::{CommentIndex, RenderedLine, RenderedLineKind};
        use local_review_core::tui::ComposerScreen;

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None, false);
        set_commit_diff_state(&mut surface);

        let scope = ComposerScope::Change;
        let req = SaveRequest {
            scope: &scope,
            severity: Severity::Note,
            body: "this is my review note",
            entry_idx: 0,
        };
        surface.save_comment(req).unwrap();
        assert_eq!(
            surface.loaded_drafts.len(),
            1,
            "draft must be in loaded_drafts"
        );

        let meta_line = RenderedLine {
            kind: RenderedLineKind::InlineCommentMeta {
                comment_index: CommentIndex::Local(0),
            },
            text: "┃ ● note  this is my review note".to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        };
        let view = DiffView {
            title: "test".to_owned(),
            lines: vec![meta_line],
            paired_rows: vec![],
            token_spans: std::collections::HashMap::new(),
        };

        let action = surface
            .handle_extra_key(make_key(KeyCode::Enter), 0, 0, Some(&view))
            .unwrap();

        let ExtraKeyAction::OpenScreen(mut state) = action else {
            panic!("expected ExtraKeyAction::OpenScreen from Enter on InlineCommentMeta");
        };
        let screen = state
            .as_any_mut()
            .downcast_mut::<ComposerScreen>()
            .expect("opened screen must be a ComposerScreen");
        assert_eq!(
            screen.0.body_text(),
            "this is my review note",
            "edit composer must pre-populate body from the saved draft"
        );

        if let Some(p) = prev {
            std::env::set_var("XDG_DATA_HOME", p);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn enter_on_github_thread_meta_opens_reply_composer() {
        use local_review_core::tui::diff_view::{CommentIndex, RenderedLine, RenderedLineKind};

        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "upstream review comment",
            "2024-01-15T10:30:00Z",
        );
        let mut surface = make_surface_with_thread(thread);

        let meta_line = RenderedLine {
            kind: RenderedLineKind::InlineCommentMeta {
                comment_index: CommentIndex::GitHubThread(0),
            },
            text: "┃ ● note  upstream review comment".to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        };
        let view = DiffView {
            title: "test".to_owned(),
            lines: vec![meta_line],
            paired_rows: vec![],
            token_spans: std::collections::HashMap::new(),
        };

        let action_enter = surface
            .handle_extra_key(make_key(KeyCode::Enter), 0, 0, Some(&view))
            .unwrap();
        let ExtraKeyAction::OpenScreen(mut state) = action_enter else {
            panic!("Enter on GitHubThread must open a ReplyComposerScreen, not edit / error");
        };
        assert!(
            state
                .as_any_mut()
                .downcast_mut::<ReplyComposerScreen>()
                .is_some(),
            "Enter on GitHubThread must open a ReplyComposerScreen (not a ComposerScreen)"
        );

        let action_c = surface
            .handle_extra_key(make_key(KeyCode::Char('c')), 0, 0, Some(&view))
            .unwrap();
        let ExtraKeyAction::OpenScreen(mut state) = action_c else {
            panic!("c on GitHubThread must open a ReplyComposerScreen");
        };
        assert!(
            state
                .as_any_mut()
                .downcast_mut::<ReplyComposerScreen>()
                .is_some(),
            "c on GitHubThread must open a ReplyComposerScreen"
        );
    }

    #[test]
    fn e_on_github_thread_meta_returns_edit_not_allowed_status() {
        use local_review_core::tui::diff_view::{CommentIndex, RenderedLine, RenderedLineKind};

        let thread = make_thread(
            "src/foo.rs",
            Some(10),
            Some(1),
            "upstream review comment",
            "2024-01-15T10:30:00Z",
        );
        let mut surface = make_surface_with_thread(thread);

        let meta_line = RenderedLine {
            kind: RenderedLineKind::InlineCommentMeta {
                comment_index: CommentIndex::GitHubThread(0),
            },
            text: "┃ ● note  upstream review comment".to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        };
        let view = DiffView {
            title: "test".to_owned(),
            lines: vec![meta_line],
            paired_rows: vec![],
            token_spans: std::collections::HashMap::new(),
        };

        // e is "edit local draft" — must NOT open the reply composer on a GitHub
        // thread row. It must surface the status message instead. This pins the
        // semantic split between Enter/c (which route Local→edit, GitHub→reply)
        // and e (which is edit-only and refuses GitHub threads).
        let action = surface
            .handle_extra_key(make_key(KeyCode::Char('e')), 0, 0, Some(&view))
            .unwrap();
        let ExtraKeyAction::StatusMessage(msg) = action else {
            panic!("e on GitHubThread must return StatusMessage, not OpenScreen");
        };
        assert!(
            msg.contains("cannot be edited"),
            "e on GitHubThread must surface the 'cannot be edited locally' status, got: {msg}"
        );
    }

    #[test]
    fn delete_comment_returns_refused_when_no_drafts() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        let identity = CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = DeleteRequest::new(identity, None);
        let result = surface
            .delete_comment(req)
            .expect("delete_comment must not error");
        assert!(
            matches!(result, DeleteOutcome::Refused { .. }),
            "delete_comment must return Refused when no drafts exist"
        );
    }

    #[test]
    fn entry_count_zero_commits_returns_one() {
        let mut pr = make_pr();
        pr.commits.clear();
        let surface = GgrSurface::new(pr, None, false);
        assert_eq!(
            surface.entry_count(),
            1,
            "description entry exists even with no commits"
        );
    }

    #[test]
    fn fetch_views_zero_resets_to_description_page() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
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
        let mut surface = GgrSurface::new(make_pr_zero_commits(), None, false);
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
        let mut surface = GgrSurface::new(make_pr(), None, false);
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
        let mut surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(
            surface.toggle_view_reviewed(0),
            ReviewedOutcome::NotTracked,
            "ggr does not track reviewed state; toggle must return NotTracked"
        );
    }

    #[test]
    fn file_picker_entries_for_commit_diff_state_returns_description_plus_files() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
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
        let surface = GgrSurface::new(pr, None, false);
        let desc = surface.entry_description(1);
        assert!(
            !desc.chars().any(char::is_control),
            "entry_description must strip control chars from commit title; got: {desc:?}"
        );
    }

    // ── current_cursor_state tests ────────────────────────────────────────────

    #[test]
    fn current_cursor_state_returns_none_in_description_state() {
        let surface = GgrSurface::new(make_pr(), None, false);
        // Default state is Description; must return None regardless of file/line.
        assert!(surface.current_cursor_state(0, 0).is_none());
        assert!(surface.current_cursor_state(1, 5).is_none());
    }

    #[test]
    fn current_cursor_state_at_file_index_zero_saves_empty_file() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        // file_index=0 is the commit description sub-view; file must be "".
        let state = surface.current_cursor_state(0, 3).unwrap();
        assert_eq!(
            state.file, "",
            "file_index=0 is description sub-view; file must be empty string"
        );
    }

    #[test]
    fn current_cursor_state_oob_file_index_aliases_to_description_sentinel() {
        // App-layer clamping prevents this in production; the test documents the aliasing.
        let mut surface = GgrSurface::new(make_pr(), None, false);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        let state = surface.current_cursor_state(99, 0).unwrap();
        assert_eq!(
            state.file, "",
            "OOB file_index must alias to description sentinel (file==\"\")"
        );
    }

    #[test]
    fn current_cursor_state_commit_diff_saves_correct_sha_and_file() {
        let pr = make_pr();
        let expected_sha = pr.commits[0].sha.as_str().to_owned();
        let mut surface = GgrSurface::new(pr, None, false);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        // file_index=1 corresponds to diff.files[0] ("src/foo.rs").
        let state = surface.current_cursor_state(1, 7).unwrap();
        assert_eq!(state.commit_sha, expected_sha, "SHA must match commits[0]");
        assert_eq!(
            state.file, "src/foo.rs",
            "file must match diff.files[0] path"
        );
        assert_eq!(state.line, 7);
    }

    // ── GgrSurface::new with cursor tests ─────────────────────────────────────

    fn make_cursor_state(sha: &str, file: &str, line: usize) -> cursor::CursorState {
        cursor::CursorState {
            commit_sha: sha.to_owned(),
            file: file.to_owned(),
            line,
        }
    }

    #[test]
    fn cursor_resume_drives_current_entry_index() {
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/lib.rs", 5);
        let surface = GgrSurface::new(pr, Some(&cursor), false);
        assert_eq!(
            surface.current_entry_index(),
            1,
            "pending_initial_index must drive current_entry_index before first fetch_views"
        );
    }

    #[test]
    fn new_with_cursor_matching_sha_sets_pending_cursor() {
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/lib.rs", 10);
        let surface = GgrSurface::new(pr, Some(&cursor), false);
        assert_eq!(
            surface.pending_cursor,
            Some(("src/lib.rs".to_owned(), 10)),
            "matching SHA must populate pending_cursor"
        );
        assert_eq!(
            surface.pending_initial_index,
            Some(1),
            "matching SHA at commits[0] must yield pending_initial_index=1"
        );
    }

    #[test]
    fn new_with_cursor_unmatched_sha_falls_back_to_first_commit() {
        let pr = make_pr();
        let cursor = make_cursor_state(&"0".repeat(40), "src/lib.rs", 3);
        let surface = GgrSurface::new(pr, Some(&cursor), false);
        assert_eq!(
            surface.pending_initial_index,
            Some(1),
            "unknown SHA must fall back to the default landing (first commit)"
        );
        assert_eq!(
            surface.pending_cursor, None,
            "unmatched SHA must not restore a stale cursor position"
        );
    }

    #[test]
    fn new_without_cursor_lands_on_first_commit() {
        let surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(
            surface.current_entry_index(),
            1,
            "default landing is the first commit's entity list, not the PR description page"
        );
    }

    #[test]
    fn new_without_cursor_on_zero_commit_pr_lands_on_entry_zero() {
        let mut pr = make_pr();
        pr.commits.clear();
        let surface = GgrSurface::new(pr, None, false);
        assert_eq!(
            surface.current_entry_index(),
            0,
            "a PR with no commits has only entry 0 to land on"
        );
    }

    // ── initial_view_position tests ───────────────────────────────────────────

    #[test]
    fn initial_view_position_returns_zero_zero_when_no_cursor() {
        let mut surface = GgrSurface::new(make_pr(), None, false);
        assert_eq!(surface.initial_view_position(), (0, 0));
    }

    #[test]
    fn initial_view_position_returns_zero_zero_in_description_state() {
        // cursor was loaded, but state is still Description (entry not yet fetched)
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/foo.rs", 5);
        let mut surface = GgrSurface::new(pr, Some(&cursor), false);
        // state remains Description; initial_view_position must fall back to (0,0)
        assert_eq!(surface.initial_view_position(), (0, 0));
    }

    #[test]
    fn initial_view_position_returns_file_idx_and_line() {
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/foo.rs", 9);
        let mut surface = GgrSurface::new(pr, Some(&cursor), false);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        // src/foo.rs is diff.files[0], so view index is 1 (description is 0).
        let pos = surface.initial_view_position();
        assert_eq!(pos, (1, 9), "must return (file_view_idx=1, line=9)");
    }

    #[test]
    fn initial_view_position_empty_file_falls_back_to_description_subview_preserving_line() {
        let make_surface_with_file = |file: &str, line: usize| {
            let pr = make_pr();
            let sha = pr.commits[0].sha.as_str().to_owned();
            let cursor = make_cursor_state(&sha, file, line);
            let mut surface = GgrSurface::new(pr, Some(&cursor), false);
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
        };

        // Case 1: empty file sentinel
        let pos = make_surface_with_file("", 5).initial_view_position();
        assert_eq!(
            pos.0, 0,
            "empty file must fall back to commit-description sub-view (file_idx=0)"
        );
        assert_eq!(pos.1, 5, "line must be preserved from cursor (empty file)");

        // Case 2: stale file no longer present in diff
        let pos = make_surface_with_file("src/removed.rs", 11).initial_view_position();
        assert_eq!(
            pos.0, 0,
            "stale file not in diff must fall back to file_idx=0"
        );
        assert_eq!(pos.1, 11, "line must be preserved from cursor (stale file)");
    }

    #[test]
    fn initial_view_position_consumed_after_first_call() {
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/foo.rs", 4);
        let mut surface = GgrSurface::new(pr, Some(&cursor), false);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.rs"),
                    hunks: vec![],
                }],
            },
        };
        let first = surface.initial_view_position();
        assert_eq!(first, (1, 4), "first call must return cursor position");
        let second = surface.initial_view_position();
        assert_eq!(
            second,
            (0, 0),
            "second call must return (0,0) — cursor consumed"
        );
    }

    #[test]
    fn initial_view_position_strip_controls_applied_consistently() {
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let stripped_name = "src/file.rs";
        let cursor = make_cursor_state(&sha, stripped_name, 3);
        let mut surface = GgrSurface::new(pr, Some(&cursor), false);
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/\x1bfile.rs"),
                    hunks: vec![],
                }],
            },
        };
        let pos = surface.initial_view_position();
        assert_eq!(
            pos.0, 1,
            "lookup must strip_controls on the diff path to match the saved (stripped) cursor file"
        );
        assert_eq!(pos.1, 3, "line must be preserved");
    }

    // ── on_entry_loaded updates current_entry_index ───────────────────────────

    #[test]
    fn on_entry_loaded_clears_pending_so_current_index_follows_state() {
        // Regression: on_entry_loaded was a no-op, so pending_initial_index
        // was never cleared. current_entry_index() returned the startup value
        // on every call, making n/p navigation appear to do nothing.
        //
        // In production, load_entry always calls fetch_views THEN
        // on_entry_loaded. fetch_views sets self.state (with the real diff) and
        // also clears pending_initial_index. on_entry_loaded's job is to ensure
        // pending_initial_index is cleared even when fetch_views has already run.
        let pr = make_pr(); // 2 commits → entry_count() == 3
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/lib.rs", 5);
        let mut surface = GgrSurface::new(pr, Some(&cursor), false);

        // Startup: pending drives index = 1.
        assert_eq!(surface.current_entry_index(), 1);

        // Simulate fetch_views having set state to CommitDiff { index: 2 }.
        surface.state = State::CommitDiff {
            index: 2,
            diff: Diff { files: Vec::new() },
        };

        // on_entry_loaded clears pending so current_entry_index follows state.
        surface.on_entry_loaded(2, false);
        assert_eq!(
            surface.current_entry_index(),
            2,
            "current_entry_index must follow state.index after pending is cleared"
        );
        assert!(
            surface.pending_initial_index.is_none(),
            "pending_initial_index must be cleared after on_entry_loaded"
        );
    }

    #[test]
    fn on_entry_loaded_does_not_overwrite_state_set_by_fetch_views() {
        // on_entry_loaded must only clear pending_initial_index; it must NOT
        // overwrite self.state. fetch_views runs before on_entry_loaded in
        // load_entry and sets the real commit diff; overwriting it with an
        // empty placeholder breaks inline-comment lookup and entity counts.
        let pr = make_pr();
        let mut surface = GgrSurface::new(pr, None, false);
        // Simulate fetch_views having set CommitDiff with real data.
        surface.state = State::CommitDiff {
            index: 1,
            diff: Diff {
                files: vec![DiffFile::Modified {
                    path: std::path::PathBuf::from("src/foo.py"),
                    hunks: vec![],
                }],
            },
        };
        surface.on_entry_loaded(1, false);
        // State must be preserved — diff files must not be cleared.
        match &surface.state {
            State::CommitDiff { index, diff } => {
                assert_eq!(*index, 1);
                assert_eq!(
                    diff.files.len(),
                    1,
                    "on_entry_loaded must not overwrite fetch_views diff"
                );
            }
            State::Description => panic!("expected CommitDiff state, got Description"),
        }
    }
}

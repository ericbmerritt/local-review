//! Terminal UI entry point for ggr: wires `PrDetails` into the shared
//! `App<GgrSurface>` review loop from `local-review-core`.

use std::io::{stdout, Stdout, Write as _};

use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
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
}

impl GgrSurface {
    pub(crate) fn new(pr: PrDetails, initial_cursor: Option<&cursor::CursorState>) -> Self {
        let pending_initial_index = initial_cursor.and_then(|c| {
            pr.commits
                .iter()
                .position(|commit| commit.sha.as_str() == c.commit_sha)
                .map(|pos| pos + 1)
        });
        let pending_cursor = pending_initial_index
            .and(initial_cursor)
            .map(|c| (strip_controls(&c.file), c.line));
        Self {
            pr,
            state: State::Description,
            threads_expanded: true,
            pending_initial_index,
            pending_cursor,
            loaded_drafts: Vec::new(),
            loaded_replies: Vec::new(),
            last_severity: None,
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
        let commit_sha = match self.pr.commits.get(index.wrapping_sub(1)) {
            Some(c) => c.sha.as_str().to_owned(),
            None => return None,
        };
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
                if file_str
                    .split('/')
                    .any(|seg| !crate::pr::valid_segment(seg))
                {
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
                    old_line: line_target.source_line,
                    new_line: line_target.target_line,
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
            out.push(InlineComment {
                source_line: thread.original_line,
                target_line: thread.line,
                severity: thread.severity,
                age: local_review_core::util::format_age_from_iso_str(now, &thread.root.created_at),
                body_lines: strip_controls_preserve_newlines(&thread.root.body)
                    .lines()
                    .map(str::to_owned)
                    .collect(),
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
        self.pending_initial_index.take();
        if idx == 0 {
            self.state = State::Description;
            let desc = self.pr_description_text();
            let views = vec![DiffView::from_description(&desc)];
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
            views.push(DiffView::from_file(file));
        }
        self.state = State::CommitDiff { index: idx, diff };
        self.reload_drafts();
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
            crate::draft::GgrAnchor::Commit { .. } => "commit draft saved",
            crate::draft::GgrAnchor::Pr => "PR draft saved",
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
                Ok(self.open_composer_at(file_index, line_index, current_view))
            }
            KeyCode::Char('m') => Ok(self.open_commit_scope_composer()),
            KeyCode::Char('P') => Ok(self.open_pr_scope_composer()),
            KeyCode::Char('e') => Ok(self.open_edit_composer(line_index, current_view)),
            KeyCode::Char('r') => Ok(self.open_reply_composer(line_index, current_view)),
            _ => Ok(ExtraKeyAction::Ignored),
        }
    }

    fn render_extra_screen(&self, frame: &mut Frame<'_>, state: &mut dyn ExtraScreen) {
        if let Some(s) = state.as_any_mut().downcast_mut::<ComposerScreen>() {
            composer_overlay::render_composer_overlay(frame, &s.0, None);
        } else if let Some(s) = state.as_any_mut().downcast_mut::<ReplyComposerScreen>() {
            composer_overlay::render_composer_overlay(frame, &s.composer, None);
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
        Ok(ExtraScreenAction::StayOpen)
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
    let cursor_path = cursor::cursor_path(&pr);
    let initial_cursor = cursor_path.as_deref().and_then(cursor::load);
    let surface = GgrSurface::new(pr, initial_cursor.as_ref());
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
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.entry_count(), 3, "2 commits + 1 description entry");
    }

    #[test]
    fn entry_count_single_commit() {
        let mut pr = make_pr();
        pr.commits.truncate(1);
        let surface = GgrSurface::new(pr, None);
        assert_eq!(surface.entry_count(), 2);
    }

    #[test]
    fn entry_id_display_returns_pr_number_for_index_0() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.entry_id_display(0), "#42");
    }

    #[test]
    fn entry_id_display_returns_short_sha_for_commits() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.entry_id_display(1), "a3b4c5d6");
        assert_eq!(surface.entry_id_display(2), "b3b4c5d6");
    }

    #[test]
    fn entry_id_display_out_of_range_returns_empty() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.entry_id_display(99), "");
    }

    #[test]
    fn entry_description_returns_pr_title_for_index_0() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.entry_description(0), "PR title");
    }

    #[test]
    fn entry_description_returns_commit_title() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.entry_description(1), "First commit");
    }

    #[test]
    fn entry_description_strips_control_chars_from_pr_title() {
        let mut pr = make_pr();
        pr.title = "\x1b[31mevil\x1b[0m".to_owned();
        let surface = GgrSurface::new(pr, None);
        let desc = surface.entry_description(0);
        assert!(
            !desc.chars().any(char::is_control),
            "entry_description must strip control chars; got: {desc:?}"
        );
    }

    #[test]
    fn entry_description_out_of_range_returns_empty() {
        let surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(pr, None);
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
        let surface = GgrSurface::new(make_pr(), None);
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 0, None)
            .is_empty());
        assert!(surface
            .inline_comments_for_view(std::time::SystemTime::UNIX_EPOCH, 1, None)
            .is_empty());
    }

    #[test]
    fn threads_expanded_defaults_to_true() {
        let surface = GgrSurface::new(make_pr(), None);
        assert!(
            surface.threads_expanded,
            "threads_expanded must be true after new()"
        );
    }

    #[test]
    fn handle_extra_key_t_expands_then_collapses() {
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(pr, None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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

        let mut surface = GgrSurface::new(make_pr(), None);
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

        let mut surface = GgrSurface::new(make_pr(), None);
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

        let mut surface = GgrSurface::new(make_pr(), None);
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

    #[test]
    #[serial]
    fn update_comment_changes_body() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", dir.path());

        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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

        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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

        let mut surface = GgrSurface::new(make_pr(), None);
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
        let surface = GgrSurface::new(make_pr(), None);
        assert!(!surface.is_view_reviewed(0));
        assert!(!surface.is_view_reviewed(1));
    }

    #[test]
    fn severity_histogram_returns_default_with_no_drafts() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.severity_histogram(), SeverityHistogram::default());
    }

    #[test]
    fn help_screen_title_is_ggr() {
        let surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.help_screen_title(), "ggr");
    }

    #[test]
    fn pr_description_text_includes_title_body_and_comments() {
        let mut pr = make_pr();
        pr.comments.push(PrComment {
            author: "alice".to_owned(),
            body: "great PR!".to_owned(),
        });
        let surface = GgrSurface::new(pr, None);
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
        let surface = GgrSurface::new(pr, None);
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
        let surface = GgrSurface::new(pr, None);
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
        let surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
        let count = surface.entry_count();
        let result = surface.fetch_views(count);
        assert!(result.is_err(), "out-of-range fetch_views must return Err");
    }

    #[test]
    fn pr_description_text_with_empty_body_has_no_extra_newlines() {
        let mut pr = make_pr();
        pr.body = String::new();
        let surface = GgrSurface::new(pr, None);
        let text = surface.pr_description_text();
        assert_eq!(
            text, "PR title",
            "empty body must not add trailing newlines"
        );
    }

    #[test]
    fn delete_comment_returns_refused_when_no_drafts() {
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let surface = GgrSurface::new(pr, None);
        assert_eq!(
            surface.entry_count(),
            1,
            "description entry exists even with no commits"
        );
    }

    #[test]
    fn fetch_views_zero_resets_to_description_page() {
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr_zero_commits(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
        assert_eq!(
            surface.toggle_view_reviewed(0),
            ReviewedOutcome::NotTracked,
            "ggr does not track reviewed state; toggle must return NotTracked"
        );
    }

    #[test]
    fn file_picker_entries_for_commit_diff_state_returns_description_plus_files() {
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let surface = GgrSurface::new(pr, None);
        let desc = surface.entry_description(1);
        assert!(
            !desc.chars().any(char::is_control),
            "entry_description must strip control chars from commit title; got: {desc:?}"
        );
    }

    // ── current_cursor_state tests ────────────────────────────────────────────

    #[test]
    fn current_cursor_state_returns_none_in_description_state() {
        let surface = GgrSurface::new(make_pr(), None);
        // Default state is Description; must return None regardless of file/line.
        assert!(surface.current_cursor_state(0, 0).is_none());
        assert!(surface.current_cursor_state(1, 5).is_none());
    }

    #[test]
    fn current_cursor_state_at_file_index_zero_saves_empty_file() {
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(make_pr(), None);
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
        let mut surface = GgrSurface::new(pr, None);
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
        let surface = GgrSurface::new(pr, Some(&cursor));
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
        let surface = GgrSurface::new(pr, Some(&cursor));
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
    fn new_with_cursor_unmatched_sha_sets_pending_to_none_index() {
        let pr = make_pr();
        let cursor = make_cursor_state(&"0".repeat(40), "src/lib.rs", 3);
        let surface = GgrSurface::new(pr, Some(&cursor));
        assert_eq!(
            surface.pending_initial_index, None,
            "unknown SHA must leave pending_initial_index as None"
        );
        assert_eq!(
            surface.pending_cursor, None,
            "unmatched SHA must clear pending_cursor"
        );
    }

    // ── initial_view_position tests ───────────────────────────────────────────

    #[test]
    fn initial_view_position_returns_zero_zero_when_no_cursor() {
        let mut surface = GgrSurface::new(make_pr(), None);
        assert_eq!(surface.initial_view_position(), (0, 0));
    }

    #[test]
    fn initial_view_position_returns_zero_zero_in_description_state() {
        // cursor was loaded, but state is still Description (entry not yet fetched)
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/foo.rs", 5);
        let mut surface = GgrSurface::new(pr, Some(&cursor));
        // state remains Description; initial_view_position must fall back to (0,0)
        assert_eq!(surface.initial_view_position(), (0, 0));
    }

    #[test]
    fn initial_view_position_returns_file_idx_and_line() {
        let pr = make_pr();
        let sha = pr.commits[0].sha.as_str().to_owned();
        let cursor = make_cursor_state(&sha, "src/foo.rs", 9);
        let mut surface = GgrSurface::new(pr, Some(&cursor));
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
            let mut surface = GgrSurface::new(pr, Some(&cursor));
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
        let mut surface = GgrSurface::new(pr, Some(&cursor));
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
        let mut surface = GgrSurface::new(pr, Some(&cursor));
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
}

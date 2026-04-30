use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use time::OffsetDateTime;
use tui_textarea::TextArea;

use crate::change_id::ChangeId;
use crate::comment::{Comment, Severity};
use crate::stack::RevsetHash;

const SECS_PER_MIN: i64 = 60;
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MIN;
const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;

/// Where the comment is being anchored (what the scope picker shows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerScope {
    Line,
    Change,
    Stack,
    Description,
}

/// Subset of `ComposerScope` whose chord can be refused at keypress time.
/// Line/Change have no context-absent failure mode (they are always populated
/// from the cursor / current change), so they are not part of this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusableScope {
    Stack,
    Description,
}

/// Status hint surfaced when ^K is pressed in single-change mode (no `stack`
/// snapshot). Shared between the chord refusal path and the chrome-time
/// "stack scope unavailable" line in `composer_overlay`.
pub(crate) const STATUS_STACK_UNAVAILABLE: &str = "stack scope unavailable in single-change mode";

/// Status hint surfaced when Alt+D is pressed without a description snapshot
/// (composer not opened from a description line).
pub(crate) const STATUS_DESCRIPTION_UNAVAILABLE: &str =
    "description scope unavailable: open from a description line";

/// All data needed to build a `LineAnchor` once the composer saves.
/// Naming note: `source_line`/`target_line` map to `old_line`/`new_line` in
/// the persisted `LineAnchor` — this struct uses the diff-view-side names.
#[derive(Debug, Clone)]
pub(crate) struct LineTarget {
    pub(crate) file: PathBuf,
    pub(crate) rendered_index: usize,
    pub(crate) source_line: Option<u32>,
    pub(crate) target_line: Option<u32>,
    pub(crate) target_text: String,
    pub(crate) hunk_header: String,
    pub(crate) context_before: Vec<String>,
    pub(crate) context_after: Vec<String>,
}

/// Snapshot of change-level context captured at composer open time.
///
/// `change_id` is the typed `ChangeId` of the change the comment will attach to
/// when scope is `ComposerScope::Change`. For composers opened from the main
/// view this is `app.details.change_id`; for composers opened from the stack
/// overview it is the `change_id` of the cursor row (which may differ from the
/// currently loaded change).
#[derive(Debug, Clone)]
pub(crate) struct ChangeContext {
    pub(crate) change_id: ChangeId,
    pub(crate) description: String,
}

/// Snapshot of stack-level context captured at composer open time.
#[derive(Debug, Clone)]
pub(crate) struct StackContextSnapshot {
    pub(crate) revset: String,
    pub(crate) revset_hash: RevsetHash,
}

/// Description-scope context captured at composer open time. Mirrors
/// `ChangeContext`/`StackContextSnapshot`; carries the cursor's 1-based line
/// number plus the surrounding context window used to build a
/// `DescriptionAnchor` on save.
#[derive(Debug, Clone)]
pub(crate) struct DescriptionContext {
    pub(crate) change_id: ChangeId,
    pub(crate) target_line: Option<u32>,
    pub(crate) target_text: String,
    pub(crate) context_before: Vec<String>,
    pub(crate) context_after: Vec<String>,
}

/// Per-scope context snapshots, captured at composer open time so the chrome
/// block can swap immediately when the reviewer presses ^L/^C/^K without
/// needing to reach back into App.
#[derive(Debug, Clone)]
pub(crate) struct ComposerContexts {
    /// Always present; for Description scope, a synthetic `LineTarget` with
    /// empty path is used (the description view has no diff line).
    pub(crate) line: LineTarget,
    pub(crate) change: ChangeContext,
    /// `None` in single-change mode; stack ^K save will refuse if None.
    pub(crate) stack: Option<StackContextSnapshot>,
    /// `None` when the composer was not opened from a description line. Present
    /// when the cursor was on a `DescriptionLine` at open time.
    pub(crate) description: Option<DescriptionContext>,
}

/// State for the comment composer modal.
pub(crate) struct Composer {
    pub(crate) contexts: ComposerContexts,
    pub(crate) scope: ComposerScope,
    pub(crate) severity: Severity,
    pub(crate) body: TextArea<'static>,
    /// When `Some(created_at)`, the composer is in edit mode and `^X` will
    /// call `update_comment` instead of `save_comment`. The timestamp
    /// identifies the record on disk.
    pub(crate) editing: Option<OffsetDateTime>,
    /// In edit mode, the source `Comment` snapshot when the editor was opened
    /// from a context that doesn't expose the record through `loaded_comments`
    /// (i.e., the stack overview). Save/delete use this snapshot's anchor
    /// directly. `None` for new comments and for main-view line-comment edits.
    pub(crate) original: Option<Comment>,
    /// In-modal status hint, set when a chord is refused (Alt+D without
    /// description snapshot, ^K without stack snapshot). Cleared on the next
    /// keypress so the hint doesn't linger after the user moves on.
    pub(crate) refusal_status: Option<&'static str>,
}

impl Composer {
    /// Convenience accessor: the line target from the contexts.
    pub(crate) fn line_target(&self) -> &LineTarget {
        &self.contexts.line
    }
}

/// Bundle of fields drawn from a single `Comment` to seed an edit-mode
/// composer. Constructing this in the caller keeps `severity`, `body`, and
/// the `identity` timestamp from drifting apart at the `for_edit` boundary.
///
/// `original`, when `Some`, carries the full source `Comment` so save/delete
/// from contexts that don't have the comment in `App::loaded_comments` (i.e.,
/// the stack overview) can route through the store using the original anchor.
/// `None` is used for line-comment edits opened from the main view, where the
/// in-memory `loaded_comments` lookup keyed by `created_at` is the source of
/// truth.
pub(crate) struct EditedComment {
    pub(crate) contexts: ComposerContexts,
    pub(crate) severity: Severity,
    pub(crate) body: String,
    pub(crate) identity: OffsetDateTime,
    pub(crate) scope: ComposerScope,
    pub(crate) original: Option<Comment>,
}

impl Composer {
    pub(crate) fn new(contexts: ComposerContexts, severity: Severity) -> Self {
        Self {
            contexts,
            scope: default_scope_for_cursor(),
            severity,
            body: TextArea::default(),
            editing: None,
            original: None,
            refusal_status: None,
        }
    }

    pub(crate) fn for_edit(edited: EditedComment) -> Self {
        let mut textarea = TextArea::default();
        for (i, line) in edited.body.lines().enumerate() {
            if i > 0 {
                textarea.insert_newline();
            }
            textarea.insert_str(line);
        }
        Self {
            contexts: edited.contexts,
            scope: edited.scope,
            severity: edited.severity,
            body: textarea,
            editing: Some(edited.identity),
            original: edited.original,
            refusal_status: None,
        }
    }

    /// Modal title, reflecting scope and edit vs. new-comment mode.
    pub(crate) fn title(&self) -> String {
        let prefix = if self.editing.is_some() {
            "Edit comment"
        } else {
            "Comment"
        };
        match self.scope {
            ComposerScope::Line => {
                let file = self.contexts.line.file.display();
                let line = self
                    .contexts
                    .line
                    .target_line
                    .or(self.contexts.line.source_line)
                    .unwrap_or(0);
                format!("{prefix} · {file}:{line}")
            }
            ComposerScope::Change => {
                let id = self.contexts.change.change_id.as_str();
                format!("{prefix} · change {id}")
            }
            ComposerScope::Stack => format!("{prefix} · stack"),
            ComposerScope::Description => {
                if let Some(ctx) = &self.contexts.description {
                    let line = ctx.target_line.unwrap_or(0);
                    format!("{prefix} · description:{line}")
                } else {
                    format!("{prefix} · description")
                }
            }
        }
    }

    pub(crate) fn body_text(&self) -> String {
        self.body.lines().join("\n")
    }
}

/// Result of processing a key inside the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    /// Key was consumed; composer remains open.
    Continue,
    /// `^X` pressed; caller should save and close.
    Save,
    /// `Esc` pressed; caller should discard and close.
    Cancel,
    /// `^D` pressed while in edit mode; caller should delete the original
    /// comment and close the composer.
    Delete,
    /// Scope chord pressed without a backing context snapshot.
    RefusedScopeChord(RefusableScope),
}

/// Handle a key event inside the composer.
///
/// Scope chords (`^L`, `^C`, `^K`) and save/delete (`^X`, `^D`) are Ctrl-
/// chorded; severity chords (`Alt+R`, `Alt+S`, `Alt+N`) plus description
/// scope (`Alt+D`) are Alt-chorded because `Ctrl+digit` is unreliable across
/// terminals (Ctrl+3 = ESC, Ctrl+2 = NUL) and Ctrl-letter chords for
/// severity collide with Sublime/VS Code-style "save" interception (Ctrl+S)
/// and tui-textarea's next-line binding (Ctrl+N). All intercepted keys are
/// consumed before being passed to tui-textarea; everything else flows
/// through.
///
/// Scope chords whose context snapshot is absent (`^K` in single-change mode
/// without a `stack` snapshot, `Alt+D` outside the description view without
/// a `description` snapshot) return `RefusedScopeChord(scope)` so the caller
/// can surface a status hint. The scope itself is left unchanged — the radio
/// never points at a scope without backing context.
///
/// `^C` is captured as scope=Change — NOT as SIGINT.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled Ctrl+KeyCode variants are intentionally ignored; forwarded to textarea"
)]
pub(crate) fn handle_composer_key(composer: &mut Composer, key: KeyEvent) -> ComposerAction {
    // Any keypress (including refusal-producing chords below, which overwrite
    // it) clears a stale in-modal refusal hint so it doesn't linger.
    composer.refusal_status = None;

    if key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::Char('l') => {
                composer.scope = ComposerScope::Line;
                return ComposerAction::Continue;
            }
            KeyCode::Char('c') => {
                composer.scope = ComposerScope::Change;
                return ComposerAction::Continue;
            }
            KeyCode::Char('k') => {
                if composer.contexts.stack.is_some() {
                    composer.scope = ComposerScope::Stack;
                    return ComposerAction::Continue;
                }
                composer.refusal_status = Some(STATUS_STACK_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(RefusableScope::Stack);
            }
            KeyCode::Char('x') => {
                return ComposerAction::Save;
            }
            KeyCode::Char('d') if composer.editing.is_some() => {
                return ComposerAction::Delete;
            }
            _ => {}
        }
    }

    if key.modifiers == KeyModifiers::ALT {
        match key.code {
            KeyCode::Char('r' | 'R') => {
                composer.severity = Severity::Required;
                return ComposerAction::Continue;
            }
            KeyCode::Char('s' | 'S') => {
                composer.severity = Severity::Suggestion;
                return ComposerAction::Continue;
            }
            KeyCode::Char('n' | 'N') => {
                composer.severity = Severity::Note;
                return ComposerAction::Continue;
            }
            KeyCode::Char('d' | 'D') => {
                if composer.contexts.description.is_some() {
                    composer.scope = ComposerScope::Description;
                    return ComposerAction::Continue;
                }
                composer.refusal_status = Some(STATUS_DESCRIPTION_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(RefusableScope::Description);
            }
            _ => {}
        }
    }

    if key.code == KeyCode::Esc {
        return ComposerAction::Cancel;
    }

    composer.body.input(key);
    ComposerAction::Continue
}

#[must_use]
pub(crate) fn default_scope_for_cursor() -> ComposerScope {
    ComposerScope::Line
}

/// Choose the opening severity for a new comment.
///
/// - If the reviewer has made a previous choice this session, reuse it.
/// - Otherwise default to `Suggestion`.
#[must_use]
pub(crate) fn default_severity(last: Option<Severity>) -> Severity {
    last.unwrap_or(Severity::Suggestion)
}

#[must_use]
pub(crate) fn format_age(now: OffsetDateTime, created_at: OffsetDateTime) -> String {
    let secs = (now - created_at).whole_seconds().max(0);
    format_age_secs(secs)
}

// Boundary literals (29, 89, 3599, 86399) are rounding-half-open midpoints:
// e.g. < 30s rounds down to "just now", < 90s rounds to "1 min ago".
#[must_use]
pub(crate) fn format_age_secs(secs: i64) -> String {
    match secs {
        0..=29 => "just now".to_owned(),
        30..=89 => "1 min ago".to_owned(),
        90..=3599 => {
            let mins = (secs + SECS_PER_MIN / 2) / SECS_PER_MIN;
            format!("{mins} min ago")
        }
        3600..=86399 => {
            let hours = (secs + SECS_PER_HOUR / 2) / SECS_PER_HOUR;
            if hours == 1 {
                "1 hour ago".to_owned()
            } else {
                format!("{hours} hours ago")
            }
        }
        _ => {
            let days = secs / SECS_PER_DAY;
            if days == 1 {
                "yesterday".to_owned()
            } else {
                format!("{days} days ago")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::Severity;

    fn make_target() -> LineTarget {
        LineTarget {
            file: PathBuf::from("src/client.rs"),
            rendered_index: 5,
            source_line: None,
            target_line: Some(142),
            target_text: ".execute(|| self.inner.request(req.clone()))".to_owned(),
            hunk_header: "@@ -138,8 +138,14 @@ impl Client {".to_owned(),
            context_before: vec![
                "let req = self.prepare(req)?;".to_owned(),
                "let resp = self.retry_wrapper".to_owned(),
            ],
            context_after: vec![".await?;".to_owned()],
        }
    }

    fn make_contexts() -> ComposerContexts {
        ComposerContexts {
            line: make_target(),
            change: ChangeContext {
                change_id: ChangeId::parse("abc12345").unwrap(),
                description: "Add retry policy".to_owned(),
            },
            stack: Some(StackContextSnapshot {
                revset: "trunk()..@".to_owned(),
                revset_hash: RevsetHash::from_revset("trunk()..@"),
            }),
            description: None,
        }
    }

    fn make_composer(severity: Severity) -> Composer {
        Composer::new(make_contexts(), severity)
    }

    #[test]
    fn default_severity_uses_last_when_present() {
        assert_eq!(
            default_severity(Some(Severity::Required)),
            Severity::Required
        );
        assert_eq!(default_severity(Some(Severity::Note)), Severity::Note);
    }

    #[test]
    fn default_severity_falls_back_to_suggestion() {
        assert_eq!(default_severity(None), Severity::Suggestion);
    }

    #[test]
    fn default_scope_for_cursor_returns_line() {
        assert_eq!(default_scope_for_cursor(), ComposerScope::Line);
    }

    #[test]
    fn format_age_secs_just_now() {
        assert_eq!(format_age_secs(0), "just now");
        assert_eq!(format_age_secs(15), "just now");
        assert_eq!(format_age_secs(29), "just now");
    }

    #[test]
    fn format_age_secs_one_min_boundary() {
        assert_eq!(format_age_secs(30), "1 min ago");
        assert_eq!(format_age_secs(89), "1 min ago");
    }

    #[test]
    fn format_age_secs_minutes() {
        assert_eq!(format_age_secs(90), "2 min ago");
        assert_eq!(format_age_secs(120), "2 min ago");
        assert_eq!(format_age_secs(150), "3 min ago");
        assert_eq!(format_age_secs(600), "10 min ago");
    }

    #[test]
    fn format_age_secs_one_hour_boundary() {
        assert_eq!(format_age_secs(3600), "1 hour ago");
        assert_eq!(format_age_secs(5399), "1 hour ago");
    }

    #[test]
    fn format_age_secs_hours() {
        assert_eq!(format_age_secs(7200), "2 hours ago");
        assert_eq!(format_age_secs(10800), "3 hours ago");
    }

    #[test]
    fn format_age_secs_yesterday() {
        assert_eq!(format_age_secs(86_400), "yesterday");
        assert_eq!(format_age_secs(172_799), "yesterday");
    }

    #[test]
    fn format_age_secs_days() {
        assert_eq!(format_age_secs(172_800), "2 days ago");
        assert_eq!(format_age_secs(864_000), "10 days ago");
    }

    #[test]
    fn handle_composer_key_ctrl_l_sets_line_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Line);
    }

    #[test]
    fn handle_composer_key_ctrl_c_sets_change_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Change);
    }

    #[test]
    fn handle_composer_key_ctrl_k_sets_stack_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Stack);
    }

    #[test]
    fn handle_composer_key_alt_r_sets_required() {
        let mut c = make_composer(Severity::Note);
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Required);
    }

    #[test]
    fn handle_composer_key_alt_s_sets_suggestion() {
        let mut c = make_composer(Severity::Required);
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Suggestion);
    }

    #[test]
    fn handle_composer_key_alt_n_sets_note() {
        let mut c = make_composer(Severity::Required);
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Note);
    }

    #[test]
    fn handle_composer_key_alt_uppercase_r_also_sets_required() {
        // Some terminals send Alt+Shift+letter as uppercase; accept both.
        let mut c = make_composer(Severity::Note);
        let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Required);
    }

    #[test]
    fn handle_composer_key_ctrl_x_returns_save() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Save);
    }

    #[test]
    fn handle_composer_key_esc_returns_cancel() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Cancel);
    }

    #[test]
    fn composer_title_new_mode_line_scope() {
        let c = make_composer(Severity::Suggestion);
        assert_eq!(c.title(), "Comment · src/client.rs:142");
    }

    #[test]
    fn composer_title_new_mode_change_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        assert_eq!(c.title(), "Comment · change abc12345");
    }

    #[test]
    fn composer_title_new_mode_stack_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Stack;
        assert_eq!(c.title(), "Comment · stack");
    }

    #[test]
    fn composer_title_edit_mode() {
        let c = Composer::for_edit(EditedComment {
            contexts: make_contexts(),
            severity: Severity::Required,
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            scope: ComposerScope::Line,
            original: None,
        });
        assert_eq!(c.title(), "Edit comment · src/client.rs:142");
    }

    #[test]
    fn composer_title_edit_mode_change_scope() {
        let c = Composer::for_edit(EditedComment {
            contexts: make_contexts(),
            severity: Severity::Required,
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            scope: ComposerScope::Change,
            original: None,
        });
        assert_eq!(c.title(), "Edit comment · change abc12345");
    }

    #[test]
    fn composer_title_edit_mode_stack_scope() {
        let c = Composer::for_edit(EditedComment {
            contexts: make_contexts(),
            severity: Severity::Required,
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            scope: ComposerScope::Stack,
            original: None,
        });
        assert_eq!(c.title(), "Edit comment · stack");
    }

    #[test]
    fn for_edit_prepopulates_body_and_severity() {
        let c = Composer::for_edit(EditedComment {
            contexts: make_contexts(),
            severity: Severity::Required,
            body: "line one\nline two".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            scope: ComposerScope::Line,
            original: None,
        });
        assert_eq!(c.body_text(), "line one\nline two");
        assert_eq!(c.severity, Severity::Required);
        assert_eq!(c.editing, Some(OffsetDateTime::UNIX_EPOCH));
        assert_eq!(c.scope, ComposerScope::Line);
    }

    #[test]
    fn ctrl_d_in_edit_mode_returns_delete() {
        let mut c = Composer::for_edit(EditedComment {
            contexts: make_contexts(),
            severity: Severity::Note,
            body: "body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            scope: ComposerScope::Line,
            original: None,
        });
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Delete);
    }

    #[test]
    fn ctrl_d_outside_edit_mode_forwarded_to_textarea() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
    }

    fn make_description_context() -> DescriptionContext {
        DescriptionContext {
            change_id: ChangeId::parse("abc12345").unwrap(),
            target_line: Some(1),
            target_text: "summary".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
    }

    // -- B5: Alt+D switches to Description scope when a description context
    //   snapshot is present.
    #[test]
    fn handle_composer_key_alt_d_sets_description_scope_when_snapshot_present() {
        let mut c = make_composer(Severity::Suggestion);
        c.contexts.description = Some(make_description_context());
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Description);
    }

    // -- U3: Alt+D without a snapshot returns RefusedScopeChord(Description)
    //   so the caller can surface a status hint. Scope is unchanged.
    #[test]
    fn handle_composer_key_alt_d_emits_refusal_status_when_snapshot_absent() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Line;
        assert!(c.contexts.description.is_none());
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(RefusableScope::Description)
        );
        assert_eq!(c.scope, ComposerScope::Line);
    }

    // -- U3: ^K without a stack snapshot returns RefusedScopeChord(Stack).
    //   Single-change mode has `stack: None`.
    #[test]
    fn handle_composer_key_ctrl_k_emits_refusal_status_when_stack_unavailable() {
        let mut c = make_composer(Severity::Suggestion);
        c.contexts.stack = None;
        c.scope = ComposerScope::Line;
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(RefusableScope::Stack)
        );
        assert_eq!(c.scope, ComposerScope::Line);
    }

    // -- E2: refusal hint stored on `composer.refusal_status` so the modal can
    //   surface it inline. Cleared on the next keypress.
    #[test]
    fn composer_refusal_status_visible_in_modal_when_alt_d_pressed_without_snapshot() {
        let mut c = make_composer(Severity::Suggestion);
        assert!(c.contexts.description.is_none());
        assert!(c.refusal_status.is_none(), "starts unset");
        handle_composer_key(&mut c, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(c.refusal_status, Some(STATUS_DESCRIPTION_UNAVAILABLE));
        // Next non-refusing keypress clears the hint.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(c.refusal_status, None);
    }

    // -- T-E2-stack-clear: ^K refusal pins both set AND clear, symmetric with
    //   the Alt+D test.
    #[test]
    fn composer_refusal_status_set_on_ctrl_k_without_stack() {
        let mut c = make_composer(Severity::Suggestion);
        c.contexts.stack = None;
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(c.refusal_status, Some(STATUS_STACK_UNAVAILABLE));
        // Next non-refusing keypress clears the hint.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(c.refusal_status, None);
    }

    // -- C3: status row contributes 0 rows when `refusal_status` is None and
    //   STATUS_ROWS when set. Pins the "no permanent tax" contract — body
    //   reclaims its row as soon as the hint clears.
    #[test]
    fn status_row_height_zero_when_refusal_status_none_and_reclaims_after_clear() {
        use crate::tui::composer_overlay::{status_row_height, STATUS_ROWS_FOR_TEST};
        let mut c = make_composer(Severity::Suggestion);
        c.contexts.stack = None;
        // Initial state: no refusal → 0 rows.
        assert_eq!(status_row_height(&c), 0);
        // ^K without stack → refusal hint set → STATUS_ROWS_FOR_TEST.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(status_row_height(&c), STATUS_ROWS_FOR_TEST);
        // Next non-refusing keypress → cleared → 0 rows again.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(status_row_height(&c), 0);
    }
}

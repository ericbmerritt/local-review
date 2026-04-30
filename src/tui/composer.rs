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
}

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

/// Per-scope context snapshots, captured at composer open time so the chrome
/// block can swap immediately when the reviewer presses ^L/^C/^K without
/// needing to reach back into App.
#[derive(Debug, Clone)]
pub(crate) struct ComposerContexts {
    /// Always present — every view has a current diff line.
    pub(crate) line: LineTarget,
    pub(crate) change: ChangeContext,
    /// `None` in single-change mode; stack ^K save will refuse if None.
    pub(crate) stack: Option<StackContextSnapshot>,
}

/// State for Screen 2 — Comment composer modal.
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
}

/// Handle a key event inside the composer.
///
/// Scope chords (`^L`, `^C`, `^K`) and save/delete (`^X`, `^D`) are Ctrl-
/// chorded; severity chords (`Alt+R`, `Alt+S`, `Alt+N`) are Alt-chorded
/// because `Ctrl+digit` is unreliable across terminals (Ctrl+3 = ESC,
/// Ctrl+2 = NUL) and Ctrl-letter chords for severity collide with Sublime/
/// VS Code-style "save" interception (Ctrl+S) and tui-textarea's next-line
/// binding (Ctrl+N). All intercepted keys are consumed before being passed
/// to tui-textarea; everything else flows through.
///
/// `^C` is captured as scope=Change — NOT as SIGINT.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled Ctrl+KeyCode variants are intentionally ignored; forwarded to textarea"
)]
pub(crate) fn handle_composer_key(composer: &mut Composer, key: KeyEvent) -> ComposerAction {
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
                composer.scope = ComposerScope::Stack;
                return ComposerAction::Continue;
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
}

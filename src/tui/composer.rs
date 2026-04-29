use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use time::OffsetDateTime;
use tui_textarea::TextArea;

use crate::comment::Severity;

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
#[derive(Debug, Clone)]
pub(crate) struct LineTarget {
    pub(crate) file: PathBuf,
    /// 0-based index into the rendered lines for context capture.
    pub(crate) rendered_index: usize,
    /// 1-based source-side line number (`old_line` in the anchor).
    pub(crate) source_line: Option<u32>,
    /// 1-based target-side line number (`new_line` in the anchor).
    pub(crate) target_line: Option<u32>,
    /// Raw content of the target line (for `target_text`).
    pub(crate) target_text: String,
    /// The verbatim `@@ … @@` hunk header this line belongs to.
    pub(crate) hunk_header: String,
    /// Up to 3 lines immediately before the target (for context).
    pub(crate) context_before: Vec<String>,
    /// Up to 3 lines immediately after the target (for context).
    pub(crate) context_after: Vec<String>,
}

/// State for Screen 2 — Comment composer modal.
pub(crate) struct Composer {
    pub(crate) target: LineTarget,
    pub(crate) scope: ComposerScope,
    pub(crate) severity: Severity,
    pub(crate) body: TextArea<'static>,
    /// When `Some(created_at)`, the composer is in edit mode and `^X` will
    /// call `update_comment` instead of `save_comment`. The timestamp
    /// identifies the record on disk.
    pub(crate) editing: Option<OffsetDateTime>,
}

/// Bundle of fields drawn from a single `Comment` to seed an edit-mode
/// composer. Constructing this in the caller keeps `severity`, `body`, and
/// the `identity` timestamp from drifting apart at the `for_edit` boundary.
pub(crate) struct EditedComment {
    pub(crate) target: LineTarget,
    pub(crate) severity: Severity,
    pub(crate) body: String,
    pub(crate) identity: OffsetDateTime,
}

impl Composer {
    pub(crate) fn new(target: LineTarget, severity: Severity) -> Self {
        Self {
            target,
            scope: default_scope_for_cursor(),
            severity,
            body: TextArea::default(),
            editing: None,
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
            target: edited.target,
            scope: ComposerScope::Line,
            severity: edited.severity,
            body: textarea,
            editing: Some(edited.identity),
        }
    }

    /// Modal title, reflecting edit vs. new-comment mode.
    pub(crate) fn title(&self) -> String {
        let file = self.target.file.display();
        let line = self
            .target
            .target_line
            .or(self.target.source_line)
            .unwrap_or(0);
        if self.editing.is_some() {
            format!("Edit comment · {file}:{line}")
        } else {
            format!("Comment · {file}:{line}")
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
/// Ctrl-chord keys (`^L`, `^C`, `^K`, `^1`–`^3`, `^X`) are intercepted before
/// being passed to tui-textarea. Everything else is forwarded to the textarea
/// so multi-line editing, arrows, backspace, and word-wrap work naturally.
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
            KeyCode::Char('1') => {
                composer.severity = Severity::Note;
                return ComposerAction::Continue;
            }
            KeyCode::Char('2') => {
                composer.severity = Severity::Suggestion;
                return ComposerAction::Continue;
            }
            KeyCode::Char('3') => {
                composer.severity = Severity::Required;
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

    if key.code == KeyCode::Esc {
        return ComposerAction::Cancel;
    }

    composer.body.input(key);
    ComposerAction::Continue
}

/// Pure function so Phase 5 can extend with cursor context without touching `Composer`.
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
        let mut c = Composer::new(make_target(), Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Line);
    }

    #[test]
    fn handle_composer_key_ctrl_c_sets_change_scope() {
        let mut c = Composer::new(make_target(), Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Change);
    }

    #[test]
    fn handle_composer_key_ctrl_k_sets_stack_scope() {
        let mut c = Composer::new(make_target(), Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.scope, ComposerScope::Stack);
    }

    #[test]
    fn handle_composer_key_ctrl_1_sets_note() {
        let mut c = Composer::new(make_target(), Severity::Required);
        let key = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Note);
    }

    #[test]
    fn handle_composer_key_ctrl_2_sets_suggestion() {
        let mut c = Composer::new(make_target(), Severity::Required);
        let key = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Suggestion);
    }

    #[test]
    fn handle_composer_key_ctrl_3_sets_required() {
        let mut c = Composer::new(make_target(), Severity::Note);
        let key = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Required);
    }

    #[test]
    fn handle_composer_key_ctrl_x_returns_save() {
        let mut c = Composer::new(make_target(), Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Save);
    }

    #[test]
    fn handle_composer_key_esc_returns_cancel() {
        let mut c = Composer::new(make_target(), Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Cancel);
    }

    #[test]
    fn composer_title_new_mode() {
        let c = Composer::new(make_target(), Severity::Suggestion);
        assert_eq!(c.title(), "Comment · src/client.rs:142");
    }

    #[test]
    fn composer_title_edit_mode() {
        let c = Composer::for_edit(EditedComment {
            target: make_target(),
            severity: Severity::Required,
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
        });
        assert_eq!(c.title(), "Edit comment · src/client.rs:142");
    }

    #[test]
    fn for_edit_prepopulates_body_and_severity() {
        let c = Composer::for_edit(EditedComment {
            target: make_target(),
            severity: Severity::Required,
            body: "line one\nline two".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
        });
        assert_eq!(c.body_text(), "line one\nline two");
        assert_eq!(c.severity, Severity::Required);
        assert_eq!(c.editing, Some(OffsetDateTime::UNIX_EPOCH));
        assert_eq!(c.scope, ComposerScope::Line);
    }

    #[test]
    fn ctrl_d_in_edit_mode_returns_delete() {
        let mut c = Composer::for_edit(EditedComment {
            target: make_target(),
            severity: Severity::Note,
            body: "body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
        });
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Delete);
    }

    #[test]
    fn ctrl_d_outside_edit_mode_forwarded_to_textarea() {
        let mut c = Composer::new(make_target(), Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
    }
}

//! Comment composer modal — scope/severity picker + text area.
//!
//! Provides key-handling logic and rendering helpers for the composer overlay.
//! Reads wall-clock time via `time::OffsetDateTime` when formatting comment age
//! labels. The caller drives the event loop and calls [`handle_composer_key`]
//! on every keypress; the core builds the anchor payload from [`ComposerScope`]
//! at save time.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use time::OffsetDateTime;

use crate::change_id::ChangeId;
use crate::revset_hash::RevsetHash;
use crate::severity::Severity;
use crate::tui::textarea::TextArea;
use crate::util::format_age_secs;

/// Where the comment is being anchored. Each variant carries the data needed
/// to build its anchor at save time, so a variant cannot exist without its
/// backing context.
#[derive(Debug, Clone)]
pub enum ComposerScope {
    Line(LineTarget),
    Change,
    Stack(StackContextSnapshot),
    Description(DescriptionContext),
}

/// Discriminator-only view of `ComposerScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeTag {
    Line,
    Change,
    Stack,
    Description,
}

impl ScopeTag {
    #[must_use]
    pub fn of(scope: &ComposerScope) -> Self {
        match scope {
            ComposerScope::Line(_) => Self::Line,
            ComposerScope::Change => Self::Change,
            ComposerScope::Stack(_) => Self::Stack,
            ComposerScope::Description(_) => Self::Description,
        }
    }
}

/// Status hint surfaced when Alt+K is pressed without a stack availability
/// snapshot (composer opened in single-change mode where the jj stack cannot
/// be read).
pub const STATUS_STACK_UNAVAILABLE: &str = "stack scope unavailable in single-change mode";

/// Status hint surfaced when Alt+D is pressed without a description
/// availability snapshot (composer not opened from a description line).
pub const STATUS_DESCRIPTION_UNAVAILABLE: &str =
    "description scope unavailable: open from a description line";

/// Status hint surfaced when Alt+L is pressed without a line availability
/// snapshot (composer opened from a non-commentable cursor).
pub const STATUS_LINE_UNAVAILABLE: &str =
    "line scope unavailable: cursor is not on a commentable line";

/// All data needed to build a `LineAnchor` once the composer saves.
#[derive(Debug, Clone)]
pub struct LineTarget {
    pub file: PathBuf,
    pub rendered_index: usize,
    pub source_line: Option<u32>,
    pub target_line: Option<u32>,
    pub target_text: String,
    pub hunk_header: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

/// Snapshot of stack-level context captured at composer open time.
#[derive(Debug, Clone)]
pub struct StackContextSnapshot {
    pub revset: String,
    pub revset_hash: RevsetHash,
}

/// Description-scope context captured at composer open time.
#[derive(Debug, Clone)]
pub struct DescriptionContext {
    pub change_id: ChangeId,
    pub target_line: Option<u32>,
    pub target_text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

/// Construction data for a new composer.
pub struct ComposerInit {
    pub scope: ComposerScope,
    pub severity: Severity,
    pub change_id: ChangeId,
    pub change_description: String,
    pub line_available: Option<LineTarget>,
    pub stack_available: Option<StackContextSnapshot>,
    pub description_available: Option<DescriptionContext>,
}

/// Edit-mode coupling for `Composer`. When the composer is in edit mode
/// (vs. composing a new comment), this records the comment's identity so the
/// surface can look it up and update/delete it. The `comment_index` is the
/// index into the surface's loaded-comments list at compose-open time; this
/// index is only valid for edits from the main view (where `InlineCommentMeta`
/// rows carry it). The surface is responsible for resolving the actual record.
pub struct EditingContext {
    /// `created_at` timestamp used as the opaque on-disk key.
    pub identity: OffsetDateTime,
    /// Index into the surface's loaded-comments at compose-open time.
    /// `None` when the composer was opened from outside the main view
    /// (e.g., the stack overview), where there is no `InlineCommentMeta`
    /// row to carry the index.
    pub comment_index: Option<usize>,
}

/// State for the comment composer modal.
pub struct Composer {
    pub(crate) scope: ComposerScope,
    pub(crate) severity: Severity,
    pub(crate) body: TextArea,
    /// `Some` when the composer is in edit mode; `None` for new comments.
    pub(crate) editing: Option<EditingContext>,
    /// In-modal status hint, set when a chord is refused. Cleared on the next
    /// keypress so the hint doesn't linger after the user moves on.
    pub(crate) refusal_status: Option<&'static str>,
    /// Target change for `Change`-scope save and the picker-row's short-id
    /// label.
    pub(crate) change_id: ChangeId,
    /// Description text rendered as chrome on the Change-scope context block.
    pub(crate) change_description: String,
    pub(crate) line_available: Option<LineTarget>,
    pub(crate) stack_available: Option<StackContextSnapshot>,
    pub(crate) description_available: Option<DescriptionContext>,
}

/// Bundle of fields drawn from a single comment to seed an edit-mode composer.
pub struct EditedComment {
    pub init: ComposerInit,
    pub body: String,
    pub identity: OffsetDateTime,
    /// `comment_index` into the surface's loaded-comments. `None` when the
    /// edit was initiated outside the main-view inline list.
    pub comment_index: Option<usize>,
}

impl Composer {
    pub fn new(init: ComposerInit) -> Self {
        Self {
            scope: init.scope,
            severity: init.severity,
            body: TextArea::default(),
            editing: None,
            refusal_status: None,
            change_id: init.change_id,
            change_description: init.change_description,
            line_available: init.line_available,
            stack_available: init.stack_available,
            description_available: init.description_available,
        }
    }

    pub fn for_edit(edited: EditedComment) -> Self {
        let mut textarea = TextArea::default();
        for (i, line) in edited.body.lines().enumerate() {
            if i > 0 {
                textarea.insert_newline();
            }
            textarea.insert_str(line);
        }
        Self {
            scope: edited.init.scope,
            severity: edited.init.severity,
            body: textarea,
            editing: Some(EditingContext {
                identity: edited.identity,
                comment_index: edited.comment_index,
            }),
            refusal_status: None,
            change_id: edited.init.change_id,
            change_description: edited.init.change_description,
            line_available: edited.init.line_available,
            stack_available: edited.init.stack_available,
            description_available: edited.init.description_available,
        }
    }

    /// Modal title, reflecting scope and edit vs. new-comment mode.
    pub fn title(&self) -> String {
        let prefix = if self.editing.is_some() {
            "Edit comment"
        } else {
            "Comment"
        };
        match &self.scope {
            ComposerScope::Line(line) => {
                let file = line.file.display();
                let line_no = line.target_line.or(line.source_line).unwrap_or(0);
                format!("{prefix} · {file}:{line_no}")
            }
            ComposerScope::Change => {
                let id = self.change_id.as_str();
                format!("{prefix} · change {id}")
            }
            ComposerScope::Stack(_) => format!("{prefix} · stack"),
            ComposerScope::Description(ctx) => {
                let line = ctx.target_line.unwrap_or(0);
                format!("{prefix} · description:{line}")
            }
        }
    }

    pub fn body_text(&self) -> String {
        self.body.lines().join("\n")
    }

    pub fn scope(&self) -> &ComposerScope {
        &self.scope
    }

    pub fn severity(&self) -> Severity {
        self.severity
    }

    pub fn editing(&self) -> Option<&EditingContext> {
        self.editing.as_ref()
    }

    pub fn change_id(&self) -> &ChangeId {
        &self.change_id
    }

    pub fn body_input(&mut self, key: KeyEvent) {
        self.body.input(key);
    }

    pub fn reset_body(&mut self) {
        self.body = TextArea::default();
    }

    pub fn body_insert_newline(&mut self) {
        self.body.insert_newline();
    }

    pub fn body_insert_str(&mut self, s: &str) {
        self.body.insert_str(s);
    }

    pub fn set_severity(&mut self, severity: Severity) {
        self.severity = severity;
    }

    /// Overwrite the active scope.
    pub fn set_scope(&mut self, scope: ComposerScope) {
        self.scope = scope;
    }
}

/// Abstracts over any `Composer` implementation so [`handle_composer_key`] can
/// be generic without importing surface-specific types.
pub trait ComposerOps {
    fn editing_is_some(&self) -> bool;
    fn line_available_clone(&self) -> Option<LineTarget>;
    fn stack_available_clone(&self) -> Option<StackContextSnapshot>;
    fn description_available_clone(&self) -> Option<DescriptionContext>;
    fn set_scope(&mut self, scope: ComposerScope);
    fn set_severity(&mut self, severity: Severity);
    fn clear_refusal_status(&mut self);
    fn set_refusal_status(&mut self, status: &'static str);
    fn body_input(&mut self, key: KeyEvent);
}

impl ComposerOps for Composer {
    fn editing_is_some(&self) -> bool {
        self.editing.is_some()
    }

    fn line_available_clone(&self) -> Option<LineTarget> {
        self.line_available.clone()
    }

    fn stack_available_clone(&self) -> Option<StackContextSnapshot> {
        self.stack_available.clone()
    }

    fn description_available_clone(&self) -> Option<DescriptionContext> {
        self.description_available.clone()
    }

    fn set_scope(&mut self, scope: ComposerScope) {
        self.scope = scope;
    }

    fn set_severity(&mut self, severity: Severity) {
        self.severity = severity;
    }

    fn clear_refusal_status(&mut self) {
        self.refusal_status = None;
    }

    fn set_refusal_status(&mut self, status: &'static str) {
        self.refusal_status = Some(status);
    }

    fn body_input(&mut self, key: KeyEvent) {
        self.body.input(key);
    }
}

/// `jjr` stores `Composer` as `Screen::Composer(Box<Composer>)`, so
/// `handle_composer_key` is called with `&mut Box<Composer>`; without this
/// blanket, `Box<Composer>` does not satisfy `ComposerOps`.
impl<C: ComposerOps> ComposerOps for Box<C> {
    fn editing_is_some(&self) -> bool {
        (**self).editing_is_some()
    }

    fn line_available_clone(&self) -> Option<LineTarget> {
        (**self).line_available_clone()
    }

    fn stack_available_clone(&self) -> Option<StackContextSnapshot> {
        (**self).stack_available_clone()
    }

    fn description_available_clone(&self) -> Option<DescriptionContext> {
        (**self).description_available_clone()
    }

    fn set_scope(&mut self, scope: ComposerScope) {
        (**self).set_scope(scope);
    }

    fn set_severity(&mut self, severity: Severity) {
        (**self).set_severity(severity);
    }

    fn clear_refusal_status(&mut self) {
        (**self).clear_refusal_status();
    }

    fn set_refusal_status(&mut self, status: &'static str) {
        (**self).set_refusal_status(status);
    }

    fn body_input(&mut self, key: KeyEvent) {
        (**self).body_input(key);
    }
}

/// Result of processing a key inside the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerAction {
    /// Key was consumed; composer remains open.
    Continue,
    /// `^X` pressed; caller should save and close.
    Save,
    /// `Esc` pressed; caller should discard and close.
    Cancel,
    /// `^D` pressed while in edit mode; caller should delete the original
    /// comment and close the composer.
    Delete,
    /// Scope chord pressed without a backing availability snapshot. The
    /// payload is the status string the dispatcher should surface.
    RefusedScopeChord(&'static str),
}

/// Handle a key event inside the composer.
///
/// Scope chords (`Alt+L`, `Alt+C`, `Alt+K`, `Alt+D`) and severity chords
/// (`Alt+R`, `Alt+S`, `Alt+N`) are Alt-chorded; save/delete (`^X`, `^D`)
/// remain Ctrl-chorded. All intercepted keys are consumed before being
/// forwarded to the body editor; everything else flows through.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled Alt+/Ctrl+ KeyCode variants are intentionally ignored; forwarded to textarea"
)]
pub fn handle_composer_key<C: ComposerOps>(composer: &mut C, key: KeyEvent) -> ComposerAction {
    composer.clear_refusal_status();

    if key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::Char('x') => {
                return ComposerAction::Save;
            }
            KeyCode::Char('d') if composer.editing_is_some() => {
                return ComposerAction::Delete;
            }
            _ => {}
        }
    }

    if key.modifiers == KeyModifiers::ALT {
        match key.code {
            KeyCode::Char('l' | 'L') => {
                if let Some(line) = composer.line_available_clone() {
                    composer.set_scope(ComposerScope::Line(line));
                    return ComposerAction::Continue;
                }
                composer.set_refusal_status(STATUS_LINE_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(STATUS_LINE_UNAVAILABLE);
            }
            KeyCode::Char('c' | 'C') => {
                composer.set_scope(ComposerScope::Change);
                return ComposerAction::Continue;
            }
            KeyCode::Char('k' | 'K') => {
                if let Some(stack) = composer.stack_available_clone() {
                    composer.set_scope(ComposerScope::Stack(stack));
                    return ComposerAction::Continue;
                }
                composer.set_refusal_status(STATUS_STACK_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(STATUS_STACK_UNAVAILABLE);
            }
            KeyCode::Char('r' | 'R') => {
                composer.set_severity(Severity::Required);
                return ComposerAction::Continue;
            }
            KeyCode::Char('s' | 'S') => {
                composer.set_severity(Severity::Suggestion);
                return ComposerAction::Continue;
            }
            KeyCode::Char('n' | 'N') => {
                composer.set_severity(Severity::Note);
                return ComposerAction::Continue;
            }
            KeyCode::Char('d' | 'D') => {
                if let Some(desc) = composer.description_available_clone() {
                    composer.set_scope(ComposerScope::Description(desc));
                    return ComposerAction::Continue;
                }
                composer.set_refusal_status(STATUS_DESCRIPTION_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(STATUS_DESCRIPTION_UNAVAILABLE);
            }
            _ => {}
        }
    }

    if key.code == KeyCode::Esc {
        return ComposerAction::Cancel;
    }

    composer.body_input(key);
    ComposerAction::Continue
}

/// Choose the opening severity for a new comment.
///
/// - If the reviewer has made a previous choice this session, reuse it.
/// - Otherwise default to `Suggestion`.
#[must_use]
pub fn default_severity(last: Option<Severity>) -> Severity {
    last.unwrap_or(Severity::Suggestion)
}

/// Negative durations are clamped to zero.
#[must_use]
pub fn format_age(now: OffsetDateTime, created_at: OffsetDateTime) -> String {
    let elapsed_i64 = (now - created_at).whole_seconds();
    let elapsed_secs = u64::try_from(elapsed_i64.max(0)).unwrap_or(0);
    format_age_secs(elapsed_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn make_init(severity: Severity) -> ComposerInit {
        let target = make_target();
        ComposerInit {
            scope: ComposerScope::Line(target.clone()),
            severity,
            change_id: ChangeId::parse("abc12345").unwrap(),
            change_description: "Add retry policy".to_owned(),
            line_available: Some(target),
            stack_available: Some(StackContextSnapshot {
                revset: "trunk()..@".to_owned(),
                revset_hash: RevsetHash::from_revset("trunk()..@"),
            }),
            description_available: None,
        }
    }

    fn make_composer(severity: Severity) -> Composer {
        Composer::new(make_init(severity))
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
    fn format_age_secs_just_now() {
        assert_eq!(format_age_secs(0), "just now");
        assert_eq!(format_age_secs(15), "just now");
        assert_eq!(format_age_secs(30), "just now");
        assert_eq!(format_age_secs(59), "just now");
    }

    #[test]
    fn format_age_secs_minutes() {
        assert_eq!(format_age_secs(60), "1 min ago");
        assert_eq!(format_age_secs(90), "1 min ago");
        assert_eq!(format_age_secs(120), "2 min ago");
        assert_eq!(format_age_secs(600), "10 min ago");
        assert_eq!(format_age_secs(3_599), "59 min ago");
    }

    #[test]
    fn format_age_secs_one_hour_boundary() {
        assert_eq!(format_age_secs(3_600), "1 hour ago");
        assert_eq!(format_age_secs(7_199), "1 hour ago");
    }

    #[test]
    fn format_age_secs_hours() {
        assert_eq!(format_age_secs(7_200), "2 hours ago");
        assert_eq!(format_age_secs(10_800), "3 hours ago");
    }

    #[test]
    fn format_age_secs_one_day() {
        assert_eq!(format_age_secs(86_400), "1 day ago");
        assert_eq!(format_age_secs(172_799), "1 day ago");
    }

    #[test]
    fn format_age_secs_days() {
        assert_eq!(format_age_secs(172_800), "2 days ago");
        assert_eq!(format_age_secs(864_000), "10 days ago");
    }

    #[test]
    fn handle_composer_key_alt_l_sets_line_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Line(_)));
    }

    #[test]
    fn handle_composer_key_alt_c_sets_change_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn handle_composer_key_alt_k_sets_stack_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Stack(_)));
    }

    #[test]
    fn handle_composer_key_ctrl_k_does_not_switch_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    /// Ctrl+L is a common terminal "redraw" sequence and must be forwarded
    /// to the textarea.
    #[test]
    fn handle_composer_key_ctrl_l_does_not_switch_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(
            matches!(c.scope, ComposerScope::Change),
            "Ctrl+L must not switch to Line scope"
        );
    }

    /// Ctrl+C is SIGINT in most terminals; at minimum it must not change scope.
    #[test]
    fn handle_composer_key_ctrl_c_does_not_switch_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Line(make_target());
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(
            matches!(c.scope, ComposerScope::Line(_)),
            "Ctrl+C must not switch to Change scope"
        );
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
        let stack = c
            .stack_available
            .clone()
            .expect("test fixture sets stack_available");
        c.scope = ComposerScope::Stack(stack);
        assert_eq!(c.title(), "Comment · stack");
    }

    #[test]
    fn composer_title_edit_mode() {
        let c = Composer::for_edit(EditedComment {
            init: make_init(Severity::Required),
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            comment_index: Some(0),
        });
        assert_eq!(c.title(), "Edit comment · src/client.rs:142");
    }

    #[test]
    fn for_edit_prepopulates_body_and_severity() {
        let c = Composer::for_edit(EditedComment {
            init: make_init(Severity::Required),
            body: "line one\nline two".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            comment_index: None,
        });
        assert_eq!(c.body_text(), "line one\nline two");
        assert_eq!(c.severity, Severity::Required);
        let ctx = c
            .editing
            .as_ref()
            .expect("for_edit-built composer is in edit mode");
        assert_eq!(ctx.identity, OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(c.scope, ComposerScope::Line(_)));
    }

    #[test]
    fn ctrl_d_in_edit_mode_returns_delete() {
        let mut c = Composer::for_edit(EditedComment {
            init: make_init(Severity::Note),
            body: "body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            comment_index: Some(0),
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

    #[test]
    fn handle_composer_key_alt_d_sets_description_scope_when_snapshot_present() {
        let mut c = make_composer(Severity::Suggestion);
        c.description_available = Some(DescriptionContext {
            change_id: ChangeId::parse("abc12345").unwrap(),
            target_line: Some(1),
            target_text: "summary".to_owned(),
            context_before: vec![],
            context_after: vec![],
        });
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Description(_)));
    }

    #[test]
    fn handle_composer_key_alt_d_emits_refusal_status_when_snapshot_absent() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        assert!(c.description_available.is_none());
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(STATUS_DESCRIPTION_UNAVAILABLE)
        );
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn handle_composer_key_alt_k_emits_refusal_status_when_stack_unavailable() {
        let mut c = make_composer(Severity::Suggestion);
        c.stack_available = None;
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(STATUS_STACK_UNAVAILABLE)
        );
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn handle_composer_key_alt_l_emits_refusal_status_when_line_unavailable() {
        let mut c = make_composer(Severity::Suggestion);
        c.line_available = None;
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(STATUS_LINE_UNAVAILABLE)
        );
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn composer_refusal_status_visible_in_modal_when_alt_d_pressed_without_snapshot() {
        let mut c = make_composer(Severity::Suggestion);
        assert!(c.description_available.is_none());
        assert!(c.refusal_status.is_none(), "starts unset");
        handle_composer_key(&mut c, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(c.refusal_status, Some(STATUS_DESCRIPTION_UNAVAILABLE));
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(c.refusal_status, None);
    }

    /// Refusal status is cleared on the very next keypress, regardless of which
    /// key is sent. The status must not linger after the user moves on.
    #[test]
    fn refusal_status_clears_on_next_keypress() {
        let mut c = make_composer(Severity::Suggestion);
        c.stack_available = None;
        // Alt+K with no stack available sets refusal_status.
        handle_composer_key(&mut c, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
        assert_eq!(
            c.refusal_status,
            Some(STATUS_STACK_UNAVAILABLE),
            "refusal_status must be set after refused chord"
        );
        // Any subsequent key clears it.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(
            c.refusal_status, None,
            "refusal_status must clear after the next keypress"
        );
    }

    #[test]
    fn format_age_future_timestamp_treated_as_just_now() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let future = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(3600);
        assert_eq!(format_age(now, future), "just now");
    }

    /// `Box<Composer>` satisfies `ComposerOps` via the blanket impl; exercise
    /// the boxed path end-to-end so it is not dead from the test perspective.
    #[test]
    fn handle_composer_key_through_box_mutates_severity() {
        let mut boxed: Box<Composer> = Box::new(make_composer(Severity::Note));
        assert_eq!(boxed.severity, Severity::Note);
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut boxed, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(
            boxed.severity,
            Severity::Required,
            "Alt+R through Box<Composer> must set severity to Required"
        );
    }
}

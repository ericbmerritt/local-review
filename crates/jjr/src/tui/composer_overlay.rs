//! jjr composer overlay rendering.
//!
//! Re-exports shared constants and helper functions from
//! `local_review_core::tui::composer_overlay`. Rendering delegates to the
//! core implementation via `ComposerRenderView`, which flattens the
//! jjr-vs-core `EditingContext` difference to the single `editing_is_some`
//! boolean that the renderer needs.

pub(super) use local_review_core::tui::composer_overlay::{centered_rect, ComposerRenderView};

use local_review_core::tui::composer_overlay::render_composer_overlay_view;
use ratatui::Frame;

use super::composer::Composer;
use super::diff_view::DiffView;

/// Render the composer overlay for the jjr-specific `Composer`.
///
/// Constructs [`ComposerRenderView`] as an inline struct literal rather than
/// calling [`ComposerRenderView::from_composer`] because jjr's `Composer.editing`
/// is `Option<jjr::EditingContext>`, not `Option<core::EditingContext>`.
pub(super) fn render_composer_overlay(
    frame: &mut Frame<'_>,
    composer: &Composer,
    current_view: Option<&DiffView>,
) {
    let view = ComposerRenderView {
        title: composer.title(),
        scope: &composer.scope,
        severity: composer.severity,
        body: &composer.body,
        refusal_status: composer.refusal_status,
        change_id: composer.change_id.as_str(),
        change_description: &composer.change_description,
        editing_is_some: composer.editing.is_some(),
        focus: composer.focus,
    };
    render_composer_overlay_view(frame, &view, current_view);
}

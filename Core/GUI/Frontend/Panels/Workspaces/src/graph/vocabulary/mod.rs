//! Graph-side vocabulary overlay (Slice N, stage N-3): project the saved
//! anchors onto the code-graph canvas.
//!
//!   * [`projection`] — PURE anchors → nodes/edges/positions (resolve once
//!     per load, project per frame; code-target anchors orbit their symbol,
//!     concepts spiral at the graph edge).
//!   * [`overlay`]    — the display-graph transform (C-cluster precedent):
//!     derive what the renderer draws for the active `ViewMode`; the real
//!     graph + physics are untouched.
//!
//! `GraphView` holds the fetched/resolved anchors and the active mode; the
//! `V` key cycles CodeGraph → Overlay → VocabularyGraph. Anchor styling
//! comes from the pre-provisioned Theme keys (`node_types.anchor_concept`,
//! `edges.related_to`) — nothing visual is decided here.

pub mod overlay;
pub mod projection;

use gpui::{AsyncApp, Context};

use super::GraphView;
use crate::graph::model::ViewMode;
use crate::vocabulary::ipc as vocab_ipc;
use projection::AnchorSpec;

impl GraphView {
    /// `V` — cycle the vocabulary layer (CodeGraph → Overlay →
    /// VocabularyGraph). Survives reloads; an empty vocabulary keeps drawing
    /// the base graph regardless of mode.
    pub(crate) fn cycle_view_mode(&mut self, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::CodeGraph => ViewMode::Overlay,
            ViewMode::Overlay => ViewMode::VocabularyGraph,
            ViewMode::VocabularyGraph => ViewMode::CodeGraph,
        };
        cx.notify();
    }

    pub(crate) fn view_mode_label(&self) -> &'static str {
        match self.view_mode {
            ViewMode::CodeGraph => "code",
            ViewMode::Overlay => "overlay",
            ViewMode::VocabularyGraph => "vocabulary",
        }
    }

    /// Fetch both anchor stores and resolve targets against the loaded
    /// graph. Best-effort (an unreachable store projects as empty — OI-1);
    /// archived anchors stay off the canvas (OI-21).
    pub(crate) fn spawn_anchor_load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let ws = vocab_ipc::active_workspace().await.ok().flatten();
            let mut specs: Vec<AnchorSpec> = Vec::new();
            if let Some(id) = &ws {
                if let Ok(list) = vocab_ipc::list_workspace_anchors(id).await {
                    specs.extend(list.iter().filter(|a| !a.archived).map(to_spec));
                }
            }
            if let Ok(list) = vocab_ipc::list_global_anchors().await {
                specs.extend(list.iter().filter(|a| !a.archived).map(to_spec));
            }
            let _ = this.update(app_cx, |view, cx| {
                view.vocab_anchors = projection::resolve(&specs, &view.graph);
                cx.notify();
            });
        })
        .detach();
    }
}

fn to_spec(a: &vocab_ipc::AnchorView) -> AnchorSpec {
    AnchorSpec {
        identifier: a.identifier.clone(),
        target_symbol: a.target_symbol().map(str::to_owned),
        related_to: a.related_to.clone(),
    }
}

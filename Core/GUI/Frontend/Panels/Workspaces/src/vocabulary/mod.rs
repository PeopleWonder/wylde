//! The Vocabulary tab (Slice N, Plan v2 §4 / Build Order §4
//! `vocabulary/`): the user-visible anchor system — browse and curate the
//! workspace + global vocabularies, edit definitions/aliases/domains/
//! hierarchy, create concept anchors, and promote workspace anchors to
//! global with the OI-5 collision dialog (rename-with-suffix default /
//! keep-workspace-only / replace-with-explicit-confirm).
//!
//!   * [`ipc`]       — pipe calls to both stores (`workspaces.anchors.*` +
//!     the harness `anchors.*`), OI-1 degrade.
//!   * [`list_view`] — the pure merge/filter/sort row model.
//!   * [`editor`]    — alias parsing + the promotion-dialog state machine.
//!
//! Later N stages add `proposals.rs` (the LLM-proposed-anchor review
//! surface) and the graph-side vocabulary overlay; the shared
//! `anchor_actions` edit affordances (Plan §6) land with the bubble/graph
//! consumers that share them.

pub mod editor;
pub mod ipc;
pub mod list_view;

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, FontWeight, IntoElement, MouseButton,
    MouseDownEvent, Render, SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{
    BORDER_SUBTLE, BRAND, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;
use editor::PromotionDialog;
use ipc::{AnchorScopeTag, AnchorView};
use list_view::ScopeFilter;

/// The Vocabulary tab view.
pub struct VocabularyTab {
    workspace_id: Option<String>,
    ws_anchors: Vec<AnchorView>,
    global_anchors: Vec<AnchorView>,
    loading: bool,
    error: Option<String>,
    /// Inline status from the last action (`Ok` info / `Err` failure).
    status: Option<Result<String, String>>,
    scope_filter: ScopeFilter,
    search: Entity<TextInput>,
    _search_sub: gpui::Subscription,
    /// The selected row: which store + which identifier.
    selected: Option<(AnchorScopeTag, String)>,
    // Editor fields (filled on select).
    desc_input: Entity<TextInput>,
    aliases_input: Entity<TextInput>,
    domain_input: Entity<TextInput>,
    parent_input: Entity<TextInput>,
    // Create-anchor fields.
    show_create: bool,
    create_id_input: Entity<TextInput>,
    create_desc_input: Entity<TextInput>,
    // OI-5 promotion dialog.
    promotion: PromotionDialog,
    rename_input: Entity<TextInput>,
}

impl VocabularyTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let field = |cx: &mut Context<Self>, key: &'static str, placeholder: &'static str| {
            cx.new(|c| {
                TextInput::single_line(c)
                    .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                    .with_element_key(key)
                    .with_placeholder(placeholder)
            })
        };
        let search = field(
            cx,
            "vocab-search",
            "Search identifier, alias, description, domain…",
        );
        let search_sub = cx.subscribe(
            &search,
            |_this: &mut Self, _e, event: &wylde_gpui_input::InputEvent, cx| {
                if matches!(event, wylde_gpui_input::InputEvent::Changed(_)) {
                    cx.notify(); // re-filter on render
                }
            },
        );
        let tab = Self {
            workspace_id: None,
            ws_anchors: Vec::new(),
            global_anchors: Vec::new(),
            loading: true,
            error: None,
            status: None,
            scope_filter: ScopeFilter::All,
            search,
            _search_sub: search_sub,
            selected: None,
            desc_input: field(cx, "vocab-desc", "description"),
            aliases_input: field(cx, "vocab-aliases", "aliases, comma, separated"),
            domain_input: field(cx, "vocab-domain", "domain (e.g. Networking)"),
            parent_input: field(cx, "vocab-parent", "parent anchor (hierarchy)"),
            show_create: false,
            create_id_input: field(cx, "vocab-new-id", "identifier_like_this"),
            create_desc_input: field(cx, "vocab-new-desc", "what this concept means"),
            promotion: PromotionDialog::Idle,
            rename_input: field(cx, "vocab-rename", "renamed_identifier"),
        };
        Self::spawn_load(cx);
        tab
    }

    /// (Re)load the active workspace + both anchor stores.
    pub fn spawn_load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws = ipc::active_workspace().await;
            let (ws_id, ws_anchors, mut error) = match ws {
                Ok(Some(id)) => match ipc::list_workspace_anchors(&id).await {
                    Ok(list) => (Some(id), list, None),
                    Err(e) => (Some(id), Vec::new(), Some(e)),
                },
                Ok(None) => (None, Vec::new(), None),
                Err(e) => (None, Vec::new(), Some(e)),
            };
            let global = match ipc::list_global_anchors().await {
                Ok(list) => list,
                Err(e) => {
                    error.get_or_insert(e);
                    Vec::new()
                }
            };
            let _ = this.update(app_cx, |tab, cx| {
                tab.loading = false;
                tab.workspace_id = ws_id;
                tab.ws_anchors = ws_anchors;
                tab.global_anchors = global;
                tab.error = error;
                // Drop a selection that no longer resolves.
                if let Some((scope, id)) = tab.selected.clone() {
                    if tab.find(scope, &id).is_none() {
                        tab.selected = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn find(&self, scope: AnchorScopeTag, identifier: &str) -> Option<&AnchorView> {
        let pool = match scope {
            AnchorScopeTag::Workspace => &self.ws_anchors,
            AnchorScopeTag::Global => &self.global_anchors,
        };
        pool.iter().find(|a| a.identifier == identifier)
    }

    /// Select a row and fill the editor fields from it.
    fn select(&mut self, scope: AnchorScopeTag, identifier: &str, cx: &mut Context<Self>) {
        let Some(a) = self.find(scope, identifier).cloned() else {
            return;
        };
        self.selected = Some((scope, identifier.to_owned()));
        self.show_create = false;
        self.promotion = PromotionDialog::Idle;
        let set = |input: &Entity<TextInput>, v: String, cx: &mut Context<Self>| {
            input.update(cx, |i, c| i.set_text_silent(v, c));
        };
        set(&self.desc_input, a.description.clone(), cx);
        set(&self.aliases_input, a.aliases.join(", "), cx);
        set(&self.domain_input, a.domain.clone().unwrap_or_default(), cx);
        set(
            &self.parent_input,
            a.parent_anchor.clone().unwrap_or_default(),
            cx,
        );
        cx.notify();
    }

    /// Persist the editor fields onto the selected anchor.
    fn save_selected(&mut self, cx: &mut Context<Self>) {
        let Some((scope, id)) = self.selected.clone() else {
            return;
        };
        let ws = self.workspace_id.clone().unwrap_or_default();
        let description = self.desc_input.read(cx).text().trim().to_owned();
        let aliases = editor::parse_aliases(self.aliases_input.read(cx).text());
        let domain = {
            let d = self.domain_input.read(cx).text().trim().to_owned();
            (!d.is_empty()).then_some(d)
        };
        let parent = {
            let p = self.parent_input.read(cx).text().trim().to_owned();
            (!p.is_empty()).then_some(p)
        };
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::update_anchor(
                scope,
                &ws,
                &id,
                &description,
                &aliases,
                domain.as_deref(),
                parent.as_deref(),
            )
            .await;
            let _ = this.update(app_cx, |tab, cx| {
                tab.status = Some(match outcome {
                    Ok(_) => Ok(format!("Saved {{{{{id}}}}}")),
                    Err(e) => Err(format!("Save failed: {e}")),
                });
                Self::spawn_load(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some((scope, id)) = self.selected.clone() else {
            return;
        };
        let ws = self.workspace_id.clone().unwrap_or_default();
        self.selected = None;
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::delete_anchor(scope, &ws, &id).await;
            let _ = this.update(app_cx, |tab, cx| {
                tab.status = Some(match outcome {
                    Ok(_) => Ok(format!("Deleted {{{{{id}}}}}")),
                    Err(e) => Err(format!("Delete failed: {e}")),
                });
                Self::spawn_load(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// "New anchor" — a workspace-scope concept anchor (symbol anchors are
    /// minted from the composer/graph flows where a symbol is in hand).
    fn create_anchor(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            self.status = Some(Err("No active workspace to anchor into".to_owned()));
            cx.notify();
            return;
        };
        let identifier = self.create_id_input.read(cx).text().trim().to_owned();
        let description = self.create_desc_input.read(cx).text().trim().to_owned();
        if identifier.is_empty() || description.is_empty() {
            self.status = Some(Err(
                "Identifier and description are both required".to_owned()
            ));
            cx.notify();
            return;
        }
        let id_input = self.create_id_input.clone();
        let desc_input = self.create_desc_input.clone();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            // The concept's definition text doubles as its description in
            // the tab's create flow (the editor can diverge them later).
            let outcome =
                ipc::create_workspace_anchor(&ws, &identifier, &description, &description).await;
            let _ = this.update(app_cx, |tab, cx| {
                match outcome {
                    Ok(_) => {
                        tab.status = Some(Ok(format!("Created {{{{{identifier}}}}}")));
                        tab.show_create = false;
                        id_input.update(cx, |i, c| i.clear(c));
                        desc_input.update(cx, |i, c| i.clear(c));
                    }
                    Err(e) => tab.status = Some(Err(format!("Create failed: {e}"))),
                }
                Self::spawn_load(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Promote the selected workspace anchor to global (Plan §4.4). A
    /// collision (OI-5) opens the rename/keep/replace dialog.
    fn promote_selected(&mut self, rename_to: Option<String>, cx: &mut Context<Self>) {
        let Some((AnchorScopeTag::Workspace, id)) = self.selected.clone() else {
            return;
        };
        let Some(anchor) = self.find(AnchorScopeTag::Workspace, &id).cloned() else {
            return;
        };
        let ws = self.workspace_id.clone().unwrap_or_default();
        self.promotion = PromotionDialog::Idle;
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::promote_to_global(&anchor, rename_to.as_deref()).await;
            let _ = this.update(app_cx, |tab, cx| {
                match outcome {
                    Ok(_) => {
                        let landed = rename_to.as_deref().unwrap_or(&id);
                        tab.status = Some(Ok(format!("Promoted {{{{{landed}}}}} to global")));
                    }
                    Err(e) if ipc::is_global_collision(&e) => {
                        // OI-5: surface the dialog with the existing
                        // definition + the spec's suffix-rename default.
                        let existing = editor::existing_definition_from_error(&e);
                        tab.promotion = PromotionDialog::collision(&id, &existing, &ws);
                        if let PromotionDialog::Collision { rename_to, .. } = &tab.promotion {
                            let v = rename_to.clone();
                            tab.rename_input.update(cx, |i, c| i.set_text_silent(v, c));
                        }
                    }
                    Err(e) => tab.status = Some(Err(format!("Promotion failed: {e}"))),
                }
                Self::spawn_load(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The OI-5 "Replace the global definition" choice — runs only after the
    /// explicit second confirmation.
    fn replace_global_confirmed(&mut self, cx: &mut Context<Self>) {
        let Some((AnchorScopeTag::Workspace, id)) = self.selected.clone() else {
            return;
        };
        let Some(anchor) = self.find(AnchorScopeTag::Workspace, &id).cloned() else {
            return;
        };
        self.promotion = PromotionDialog::Idle;
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::replace_global(&anchor).await;
            let _ = this.update(app_cx, |tab, cx| {
                tab.status = Some(match outcome {
                    Ok(_) => Ok(format!("Replaced global {{{{{id}}}}}")),
                    Err(e) => Err(format!("Replace failed: {e}")),
                });
                Self::spawn_load(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ── element helpers (settings-tab idioms) ────────────────────────────

    fn heading(text: &str) -> gpui::Div {
        div()
            .text_size(px(size::SM))
            .font_weight(FontWeight(weight::SEMIBOLD as f32))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(text.to_owned()))
    }

    fn hint(text: String) -> gpui::Div {
        div()
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(text))
    }

    fn button<F>(
        id: (&'static str, usize),
        label: &str,
        accent: bool,
        cx: &mut Context<Self>,
        on_click: F,
    ) -> gpui::Stateful<gpui::Div>
    where
        F: Fn(&mut Self, &mut Context<Self>) + 'static,
    {
        let bg = if accent { BRAND } else { SURFACE_800 };
        div()
            .id(id)
            .px_2()
            .py_0p5()
            .rounded(px(4.0))
            .bg(rgb(pack(bg)))
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .cursor_pointer()
            .child(SharedString::from(label.to_owned()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| on_click(this, cx)),
            )
    }

    fn labelled_input(label: &str, input: &Entity<TextInput>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .w(px(220.0))
            .child(Self::hint(label.to_owned()))
            .child(div().child(input.clone()))
    }
}

impl Render for VocabularyTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).text().to_owned();
        let rows = list_view::rows(
            &self.ws_anchors,
            &self.global_anchors,
            self.scope_filter,
            &query,
        );

        let mut root = div()
            .id("workspaces-vocabulary-tab")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .font_family(FAMILY_INTER);

        // ── header ──────────────────────────────────────────────────────
        let ws_label = self
            .workspace_id
            .clone()
            .unwrap_or_else(|| "no active workspace".to_owned());
        let next_filter = {
            let i = ScopeFilter::CYCLE
                .iter()
                .position(|f| *f == self.scope_filter)
                .unwrap_or(0);
            ScopeFilter::CYCLE[(i + 1) % ScopeFilter::CYCLE.len()]
        };
        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(Self::heading("Vocabulary"))
                .child(Self::hint(format!(
                    "{ws_label} · {} workspace · {} global",
                    self.ws_anchors.len(),
                    self.global_anchors.len()
                )))
                .child(div().flex_1())
                .child(Self::button(
                    ("vocab-filter", 0),
                    self.scope_filter.label(),
                    false,
                    cx,
                    move |this, cx| {
                        this.scope_filter = next_filter;
                        cx.notify();
                    },
                ))
                .child(Self::button(
                    ("vocab-refresh", 0),
                    "Refresh",
                    false,
                    cx,
                    |_this, cx| Self::spawn_load(cx),
                ))
                .child(Self::button(
                    ("vocab-new", 0),
                    "New anchor",
                    true,
                    cx,
                    |this, cx| {
                        this.show_create = !this.show_create;
                        this.selected = None;
                        cx.notify();
                    },
                )),
        );
        root = root.child(div().child(self.search.clone()));

        if self.loading {
            return root.child(Self::hint("Loading vocabularies…".to_owned()));
        }
        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(0xE57373))
                    .child(SharedString::from(format!(
                        "A store was unreachable — showing what loaded. {err}"
                    ))),
            );
        }

        // ── create card ─────────────────────────────────────────────────
        if self.show_create {
            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded(px(6.0))
                    .bg(rgb(pack(SURFACE_800)))
                    .child(Self::heading("New concept anchor"))
                    .child(Self::hint(
                        "Workspace-scoped; promote it to global once it earns its keep. \
                         Symbol anchors are minted from the composer's \"Anchor this?\" flow."
                            .to_owned(),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_end()
                            .gap_2()
                            .child(Self::labelled_input("Identifier", &self.create_id_input))
                            .child(Self::labelled_input(
                                "Definition / description",
                                &self.create_desc_input,
                            ))
                            .child(Self::button(
                                ("vocab-create", 0),
                                "Create",
                                true,
                                cx,
                                |this, cx| this.create_anchor(cx),
                            )),
                    ),
            );
        }

        // ── list ────────────────────────────────────────────────────────
        let mut list = div().flex().flex_col().gap_1();
        if rows.is_empty() {
            list = list.child(Self::hint(
                "No anchors match — create one, or accept the LLM's proposals as they arrive."
                    .to_owned(),
            ));
        }
        for (i, row) in rows.iter().enumerate() {
            let a = &row.anchor;
            let is_selected = self
                .selected
                .as_ref()
                .is_some_and(|(s, id)| *s == row.scope && *id == a.identifier);
            let scope = row.scope;
            let ident = a.identifier.clone();
            let bg = if is_selected {
                SURFACE_700
            } else {
                SURFACE_800
            };
            let mut line = format!(
                "{{{{{}}}}} · {} · {}",
                a.identifier,
                a.kind_label(),
                row.scope_label()
            );
            if let Some(d) = &a.domain {
                line.push_str(&format!(" · {d}"));
            }
            if !a.aliases.is_empty() {
                line.push_str(&format!(" · {} alias(es)", a.aliases.len()));
            }
            list = list.child(
                div()
                    .id(("vocab-row", i))
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .bg(rgb(pack(bg)))
                    .cursor_pointer()
                    .child(
                        div()
                            .text_size(px(size::XS))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(line)),
                    )
                    .child(Self::hint(a.description.clone()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                            this.select(scope, &ident, cx);
                        }),
                    ),
            );
        }
        root = root.child(list);

        // ── editor card ─────────────────────────────────────────────────
        if let Some((scope, id)) = self.selected.clone() {
            if let Some(a) = self.find(scope, &id).cloned() {
                let mut card = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded(px(6.0))
                    .bg(rgb(pack(SURFACE_800)))
                    .child(Self::heading(&format!("Edit {{{{{id}}}}}")));
                let target_line = match (a.target_symbol(), a.target_text()) {
                    (Some(sym), _) => format!("targets symbol `{sym}`"),
                    (_, Some(text)) => format!("concept: {text}"),
                    _ => "unknown target".to_owned(),
                };
                card = card.child(Self::hint(format!(
                    "{} · used {} time(s) · {target_line}",
                    match scope {
                        AnchorScopeTag::Workspace => "workspace-scoped",
                        AnchorScopeTag::Global => "global",
                    },
                    a.usage_count
                )));
                if !a.related_to.is_empty() {
                    card = card.child(Self::hint(format!(
                        "related: {} (connection editing arrives with the graph overlay stage)",
                        a.related_to.join(", ")
                    )));
                }
                card = card.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_2()
                        .child(Self::labelled_input("Description", &self.desc_input))
                        .child(Self::labelled_input("Aliases (comma)", &self.aliases_input))
                        .child(Self::labelled_input("Domain", &self.domain_input))
                        .child(Self::labelled_input("Parent anchor", &self.parent_input)),
                );
                let mut actions = div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(Self::button(
                        ("vocab-save", 0),
                        "Save",
                        true,
                        cx,
                        |this, cx| this.save_selected(cx),
                    ))
                    .child(Self::button(
                        ("vocab-delete", 0),
                        "Delete",
                        false,
                        cx,
                        |this, cx| this.delete_selected(cx),
                    ));
                if scope == AnchorScopeTag::Workspace {
                    actions = actions.child(Self::button(
                        ("vocab-promote", 0),
                        "Promote to global…",
                        false,
                        cx,
                        |this, cx| this.promote_selected(None, cx),
                    ));
                }
                card = card.child(actions);
                root = root.child(card);
            }
        }

        // ── OI-5 promotion dialog ───────────────────────────────────────
        match self.promotion.clone() {
            PromotionDialog::Idle => {}
            PromotionDialog::Collision {
                identifier,
                existing_definition,
                ..
            } => {
                root = root.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .rounded(px(6.0))
                        .bg(rgb(pack(SURFACE_700)))
                        .border_1()
                        .border_color(rgb(pack(BORDER_SUBTLE)))
                        .child(Self::heading(&format!(
                            "{{{{{identifier}}}}} already exists globally"
                        )))
                        .child(Self::hint(format!(
                            "Existing definition: {existing_definition}"
                        )))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .items_end()
                                .gap_2()
                                .child(Self::labelled_input(
                                    "Rename your version (default)",
                                    &self.rename_input,
                                ))
                                .child(Self::button(
                                    ("vocab-promo-rename", 0),
                                    "Promote renamed",
                                    true,
                                    cx,
                                    |this, cx| {
                                        let name =
                                            this.rename_input.read(cx).text().trim().to_owned();
                                        if !name.is_empty() {
                                            this.promote_selected(Some(name), cx);
                                        }
                                    },
                                ))
                                .child(Self::button(
                                    ("vocab-promo-keep", 0),
                                    "Keep workspace-only",
                                    false,
                                    cx,
                                    |this, cx| {
                                        this.promotion = PromotionDialog::Idle;
                                        cx.notify();
                                    },
                                ))
                                .child(Self::button(
                                    ("vocab-promo-replace", 0),
                                    "Replace the global definition…",
                                    false,
                                    cx,
                                    move |this, cx| {
                                        if let PromotionDialog::Collision { identifier, .. } =
                                            this.promotion.clone()
                                        {
                                            this.promotion =
                                                PromotionDialog::ConfirmReplace { identifier };
                                            cx.notify();
                                        }
                                    },
                                )),
                        ),
                );
            }
            PromotionDialog::ConfirmReplace { identifier } => {
                root = root.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .p_2()
                        .rounded(px(6.0))
                        .bg(rgb(pack(SURFACE_700)))
                        .child(Self::hint(format!(
                            "Really overwrite the GLOBAL definition of {{{{{identifier}}}}}? \
                             Every workspace sees the replacement."
                        )))
                        .child(Self::button(
                            ("vocab-promo-replace-yes", 0),
                            "Yes, replace",
                            true,
                            cx,
                            |this, cx| this.replace_global_confirmed(cx),
                        ))
                        .child(Self::button(
                            ("vocab-promo-replace-no", 0),
                            "Cancel",
                            false,
                            cx,
                            |this, cx| {
                                this.promotion = PromotionDialog::Idle;
                                cx.notify();
                            },
                        )),
                );
            }
        }

        // ── status strip ────────────────────────────────────────────────
        if let Some(status) = &self.status {
            root = root.child(match status {
                Ok(msg) => Self::hint(msg.clone()),
                Err(msg) => div()
                    .text_size(px(size::XS))
                    .text_color(rgb(0xE57373))
                    .child(SharedString::from(msg.clone())),
            });
        }

        root.border_t_1().border_color(rgb(pack(BORDER_SUBTLE)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<VocabularyTab>();
    }
}

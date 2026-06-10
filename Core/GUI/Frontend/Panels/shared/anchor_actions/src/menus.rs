//! The shared right-click menu *definition* (Plan v2 §6 `menus`).
//!
//! "Right-click a bubble → same menu as right-click a graph node." The menu
//! is data here — an ordered [`MenuAction`] list derived from the anchor's
//! state — and each surface renders it with its own chrome and routes the
//! chosen action through its own IPC. Plan §5.5's context-menu rows map
//! onto these actions; entries whose backing flow hasn't shipped yet are
//! simply not emitted (no greyed-out stubs).

use crate::exclude_ignore::IgnoreTier;

/// Everything the menu needs to know about the anchor under the cursor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuContext {
    pub identifier: String,
    /// Per-message excluded right now (✕ shown as Restore)?
    pub excluded: bool,
    /// Durable tiers currently covering it.
    pub ignored_tiers: Vec<IgnoreTier>,
    /// Workspace-scoped (promotable) vs global.
    pub is_workspace_scoped: bool,
    /// Pinned to the conversation (📌 toggles).
    pub pinned: bool,
}

/// One menu row.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    /// ✕ / ↺ — exclude for this message, or restore.
    ToggleExclude { currently_excluded: bool },
    /// 📌 — pin/unpin to the conversation.
    TogglePin { currently_pinned: bool },
    /// Ignore in / stop ignoring a durable tier.
    ToggleIgnore { tier: IgnoreTier, currently: bool },
    /// Add Connection (opens the surface's picker / drawing mode).
    AddConnection,
    /// Edit the definition (opens the Vocabulary editor).
    EditDefinition,
    /// Promote to global (workspace-scoped anchors only; OI-5 dialog
    /// downstream).
    PromoteToGlobal,
}

impl MenuAction {
    /// The row label every surface shows (chrome differs, words don't).
    pub fn label(&self) -> String {
        match self {
            MenuAction::ToggleExclude {
                currently_excluded: false,
            } => "Exclude for this message".to_owned(),
            MenuAction::ToggleExclude {
                currently_excluded: true,
            } => "Restore to active".to_owned(),
            MenuAction::TogglePin {
                currently_pinned: false,
            } => "Pin to this conversation".to_owned(),
            MenuAction::TogglePin {
                currently_pinned: true,
            } => "Unpin from this conversation".to_owned(),
            MenuAction::ToggleIgnore { tier, currently } => {
                if *currently {
                    format!("Stop ignoring in this {}", tier.label())
                } else {
                    format!("Ignore in this {}", tier.label())
                }
            }
            MenuAction::AddConnection => "Add connection…".to_owned(),
            MenuAction::EditDefinition => "Edit definition".to_owned(),
            MenuAction::PromoteToGlobal => "Promote to global…".to_owned(),
        }
    }
}

/// Build the canonical menu for an anchor (Plan §5.5 ordering: per-message
/// state first, durable ignores, then the editing/promotion verbs).
pub fn anchor_menu(ctx: &MenuContext) -> Vec<MenuAction> {
    let mut out = vec![
        MenuAction::ToggleExclude {
            currently_excluded: ctx.excluded,
        },
        MenuAction::TogglePin {
            currently_pinned: ctx.pinned,
        },
    ];
    for tier in [
        IgnoreTier::Conversation,
        IgnoreTier::Workspace,
        IgnoreTier::Global,
    ] {
        out.push(MenuAction::ToggleIgnore {
            tier,
            currently: ctx.ignored_tiers.contains(&tier),
        });
    }
    out.push(MenuAction::AddConnection);
    out.push(MenuAction::EditDefinition);
    if ctx.is_workspace_scoped {
        out.push(MenuAction::PromoteToGlobal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_reflects_state_and_scope() {
        let ctx = MenuContext {
            identifier: "the_pipe".to_owned(),
            excluded: true,
            ignored_tiers: vec![IgnoreTier::Workspace],
            is_workspace_scoped: true,
            pinned: false,
        };
        let menu = anchor_menu(&ctx);
        let labels: Vec<String> = menu.iter().map(MenuAction::label).collect();
        assert_eq!(labels[0], "Restore to active");
        assert_eq!(labels[1], "Pin to this conversation");
        assert!(labels.contains(&"Stop ignoring in this workspace".to_owned()));
        assert!(labels.contains(&"Ignore in this global".to_owned()));
        assert!(labels.contains(&"Promote to global…".to_owned()));

        // A global anchor loses the promotion row.
        let global = MenuContext {
            is_workspace_scoped: false,
            ..ctx
        };
        assert!(!anchor_menu(&global)
            .iter()
            .any(|a| matches!(a, MenuAction::PromoteToGlobal)));
    }

    #[test]
    fn ordering_is_stable() {
        let menu = anchor_menu(&MenuContext {
            is_workspace_scoped: true,
            ..MenuContext::default()
        });
        assert!(matches!(menu[0], MenuAction::ToggleExclude { .. }));
        assert!(matches!(menu[1], MenuAction::TogglePin { .. }));
        assert!(matches!(menu[2], MenuAction::ToggleIgnore { .. }));
        assert!(matches!(menu.last(), Some(MenuAction::PromoteToGlobal)));
    }
}

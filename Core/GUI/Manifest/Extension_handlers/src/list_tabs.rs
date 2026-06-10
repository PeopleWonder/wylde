//! `gui.list_tabs` action handler.
//!
//! The Shell hosts a tiny in-process action surface for verbs that
//! belong to the GUI itself — `gui.list_tabs` is the canonical one.
//! Sub-windows and (later) panels that need to know the navigation
//! state call this verb rather than reaching into the registry
//! directly, so the wire shape stays the source of truth.
//!
//! The handler is async because the extension overlay involves an
//! `extensions.list_panels` pipe call.  The default implementation
//! takes a closure for that lookup so unit tests don't need a live
//! extension bridge.

use std::future::Future;

use crate::manifest::ExtensionPanel;
use crate::overlay::union_for_runtime;
use crate::registry::PanelRegistry;

/// Build the `gui.list_tabs` reply payload.
///
/// `extensions_lookup` is the async hook that produces the current
/// extension-panel list — the Shell passes a closure that calls
/// `extensions.list_panels` through the pipe; tests pass a closure
/// that returns a fixed `Vec` for deterministic assertions.
pub async fn list_tabs<F, Fut>(registry: &PanelRegistry, extensions_lookup: F) -> serde_json::Value
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Vec<ExtensionPanel>>,
{
    let exts = extensions_lookup().await;
    union_for_runtime(registry, &exts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PanelEntry, PanelOrigin, PanelSource};
    use crate::registry::{PanelRegistry, RegistryRow};

    fn row(service: &str, id: &str, order: i32) -> RegistryRow {
        RegistryRow {
            origin: PanelOrigin::FirstParty {
                service: service.into(),
            },
            entry: PanelEntry {
                id: id.into(),
                title: id.into(),
                icon: None,
                order,
                version: "0.1.0".into(),
                required_services: vec![],
                source: PanelSource::GpuiView {
                    factory: format!("c::T::{id}"),
                },
            },
            factory: None,
        }
    }

    #[test]
    fn list_tabs_unions_static_and_extension() {
        let mut r = PanelRegistry::new();
        r.register_internal(row("core", "settings", 95)).unwrap();
        let ext = ExtensionPanel {
            extension_id: "n8n".into(),
            id: "editor".into(),
            title: "Workflows".into(),
            icon: None,
            order: 50,
            version: "0.0.1".into(),
            url: "http://127.0.0.1:5678".into(),
        };
        let fut = list_tabs(&r, || async move { vec![ext] });
        let v = futures_lite_block_on(fut);
        let tabs = v["tabs"].as_array().expect("tabs is array");
        assert_eq!(tabs.len(), 2);
        // Ordering: order 50 before order 95.
        assert_eq!(tabs[0]["registry_key"], "ext:n8n/editor");
        assert_eq!(tabs[1]["registry_key"], "core/settings");
    }

    /// Minimal block-on for the async handler test.  Avoids a tokio
    /// runtime dep in tests — the future is a one-poll case.
    fn futures_lite_block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};

        fn noop_raw_waker() -> std::task::RawWaker {
            fn noop(_: *const ()) {}
            fn clone(_: *const ()) -> std::task::RawWaker {
                noop_raw_waker()
            }
            std::task::RawWaker::new(
                std::ptr::null(),
                &std::task::RawWakerVTable::new(clone, noop, noop, noop),
            )
        }

        // SAFETY: the future is only polled here on the test thread and
        // is dropped at the end of this function; pinning to the stack
        // is sound because we never move it after creating the Pin.
        let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
        let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        loop {
            match pinned.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => continue,
            }
        }
    }
}

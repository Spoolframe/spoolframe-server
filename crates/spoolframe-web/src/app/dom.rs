//! The few browser hooks the pages need beyond Dioxus' own events: a
//! listener on the window, and scrolling to a field. Never `eval`: the
//! server's Content-Security-Policy (`script-src 'self'
//! 'wasm-unsafe-eval'`) refuses JavaScript made from a string, so
//! `document::eval` throws in every real deployment.

use std::cell::Cell;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

/// A listener on the window, removed when this is dropped: keep it in a
/// hook, and it goes with the component.
pub(super) struct Listener {
    event: &'static str,
    closure: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback(
                self.event,
                self.closure.as_ref().unchecked_ref(),
            );
        }
    }
}

/// Call `f` on each `event` on the window, until the result is dropped.
pub(super) fn listen(event: &'static str, f: impl FnMut(web_sys::Event) + 'static) -> Listener {
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(f);
    if let Some(window) = web_sys::window() {
        let _ = window.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref());
    }
    Listener { event, closure }
}

/// Whether `e` is Ctrl K, or ⌘K on a Mac.
pub(super) fn is_palette_key(e: &web_sys::Event) -> bool {
    e.dyn_ref::<web_sys::KeyboardEvent>().is_some_and(|k| {
        (k.ctrl_key() || k.meta_key()) && !k.alt_key() && k.key().eq_ignore_ascii_case("k")
    })
}

thread_local! {
    /// The open settings area's unsaved edits, for closing the tab.
    static UNSAVED: Cell<usize> = const { Cell::new(0) };
}

/// Tell the unload guard how many edits would be lost.
pub(super) fn set_unsaved(n: usize) {
    UNSAVED.with(|u| u.set(n));
}

/// Ask before the tab closes or reloads while edits are unsaved.
pub(super) fn guard_unload() -> Listener {
    listen("beforeunload", |e| {
        if UNSAVED.with(Cell::get) > 0 {
            e.prevent_default();
            if let Some(e) = e.dyn_ref::<web_sys::BeforeUnloadEvent>() {
                e.set_return_value("");
            }
        }
    })
}

/// Scroll to the field `anchor` marks once it is on the page, and mark
/// it for a moment. The page may still be loading its settings: it is
/// looked for a few times.
pub(super) fn show_field(anchor: &str) {
    let anchor = anchor.to_owned();
    dioxus::prelude::spawn(async move {
        for _ in 0..40 {
            let found = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.get_element_by_id(&anchor));
            if let Some(at) = found {
                let field = at.parent_element().unwrap_or(at);
                let options = web_sys::ScrollIntoViewOptions::new();
                options.set_block(web_sys::ScrollLogicalPosition::Center);
                options.set_behavior(web_sys::ScrollBehavior::Smooth);
                field.scroll_into_view_with_scroll_into_view_options(&options);
                let _ = field.set_attribute("data-jumped", "");
                super::admin::sleep(2400).await;
                let _ = field.remove_attribute("data-jumped");
                return;
            }
            super::admin::sleep(100).await;
        }
    });
}

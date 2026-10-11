//! `transcode.video_codecs` (PGPU-06) as Server › Settings › Playback
//! orders it: a list a row of which is dragged by its grip to reorder,
//! ticked to be in the setting.
//!
//! The drag is pointer events, not HTML5 drag and drop: `dragstart` never
//! fires for a touch, and a phone is where an admin may be. The grip takes
//! the pointer (`setPointerCapture`), so the row follows a finger that
//! leaves it; the rows are one height ([`ROW_PX`]), so where it lands is
//! how far the pointer moved ([`setup::drag_target`]). The keyboard moves
//! a row with the arrow keys on its grip, or with its up and down buttons.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::icons;
use crate::setup::{self, CodecRow};

/// Each row's height in pixels, as `.codec-row` draws it.
const ROW_PX: f64 = 52.0;

/// A drag under way: the row it began on, where the pointer was, and the
/// row it would land on now.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    from: usize,
    start_y: f64,
    over: usize,
}

/// The list. `gpu_encodes` says, per codec, what a GPU here encodes it
/// with, or `None` where none does.
#[component]
pub fn CodecOrder(
    rows: Signal<Vec<CodecRow>>,
    locked: bool,
    gpu_encodes: Vec<(&'static str, Option<String>)>,
) -> Element {
    let mut drag = use_signal(|| None::<Drag>);
    let count = rows().len();
    let mut move_row = move |from: usize, to: usize| {
        let now = rows();
        rows.set(setup::reordered(&now, from, to));
    };
    rsx! {
        div { class: "codec-list", role: "list", aria_label: "Codecs, the first preferred",
            for (i, row) in rows().into_iter().enumerate() {
                {
                    let (name, what) = setup::VIDEO_CODECS
                        .iter()
                        .find(|(codec, ..)| *codec == row.codec)
                        .map(|(_, name, what)| (*name, *what))
                        .unwrap_or((row.codec, ""));
                    let found = gpu_encodes
                        .iter()
                        .find(|(codec, _)| *codec == row.codec)
                        .and_then(|(_, by)| by.clone());
                    let now = drag();
                    let class = [
                        Some("codec-row"),
                        (!row.on).then_some("off"),
                        now.is_some_and(|d| d.from == i).then_some("dragged"),
                        now.filter(|d| d.over == i && d.from != i).map(|d| {
                            if d.from > i { "drop-above" } else { "drop-below" }
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                    rsx! {
                        div { key: "{row.codec}", class: "{class}", role: "listitem",
                            span {
                                class: if locked { "lib-grip" } else { "lib-grip lib-grip-on" },
                                role: "button",
                                tabindex: if locked { "-1" } else { "0" },
                                aria_label: "Move {name}: up and down arrow keys",
                                title: if !locked { "Drag to reorder" },
                                onpointerdown: move |e: PointerEvent| {
                                    if locked {
                                        return;
                                    }
                                    // The grip keeps the pointer, wherever it goes.
                                    if let Some(event) = e.data().downcast::<web_sys::PointerEvent>()
                                        && let Some(grip) = event
                                            .target()
                                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                                    {
                                        let _ = grip.set_pointer_capture(event.pointer_id());
                                    }
                                    drag.set(Some(Drag {
                                        from: i,
                                        start_y: e.client_coordinates().y,
                                        over: i,
                                    }));
                                },
                                onpointermove: move |e: PointerEvent| {
                                    if let Some(d) = drag() {
                                        let moved = e.client_coordinates().y - d.start_y;
                                        let over = setup::drag_target(d.from, count, moved, ROW_PX);
                                        if over != d.over {
                                            drag.set(Some(Drag { over, ..d }));
                                        }
                                    }
                                },
                                onpointerup: move |_| {
                                    if let Some(d) = drag.take() && d.over != d.from {
                                        move_row(d.from, d.over);
                                    }
                                },
                                onpointercancel: move |_| drag.set(None),
                                onkeydown: move |e: KeyboardEvent| {
                                    if locked {
                                        return;
                                    }
                                    match e.key() {
                                        Key::ArrowUp if i > 0 => {
                                            e.prevent_default();
                                            move_row(i, i - 1);
                                        }
                                        Key::ArrowDown if i + 1 < count => {
                                            e.prevent_default();
                                            move_row(i, i + 1);
                                        }
                                        _ => {}
                                    }
                                },
                                icons::Grip {}
                            }
                            span { class: "lib-move",
                                button {
                                    r#type: "button",
                                    aria_label: "Move {name} up",
                                    disabled: i == 0 || locked,
                                    onclick: move |_| move_row(i, i - 1),
                                    icons::ChevronUp {}
                                }
                                button {
                                    r#type: "button",
                                    aria_label: "Move {name} down",
                                    disabled: i + 1 == count || locked,
                                    onclick: move |_| move_row(i, i + 1),
                                    icons::ChevronDown {}
                                }
                            }
                            label { class: "codec-what",
                                input {
                                    r#type: "checkbox",
                                    checked: row.on,
                                    disabled: locked,
                                    onchange: move |e| {
                                        let on = e.checked();
                                        rows.write()[i].on = on;
                                    },
                                }
                                span { class: "flex flex-col gap-0.5",
                                    b { "{name}" }
                                    span { class: "text-xs text-ctp-subtext0",
                                        "{what}"
                                        {match found {
                                            Some(by) => format!(" Here: {by}."),
                                            None => " No GPU here encodes it: skipped.".to_owned(),
                                        }}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

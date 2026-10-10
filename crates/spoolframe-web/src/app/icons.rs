//! The design's icons: inline stroke SVGs, as the boards draw them, and
//! the Spoolframe mark.

use dioxus::prelude::*;

/// A 24-unit stroke icon of `size` pixels, in the current colour.
#[component]
fn Stroke(size: u32, children: Element) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            {children}
        }
    }
}

#[component]
pub fn Key(#[props(default = 17)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z" }
            circle { cx: "16.5", cy: "7.5", r: ".5", fill: "currentColor" }
        }
    }
}

#[component]
pub fn Fingerprint(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M12 10a2 2 0 0 0-2 2c0 1.02-.1 2.51-.26 4" }
            path { d: "M14 13.12c0 2.38 0 6.38-1 8.88" }
            path { d: "M17.29 21.02c.12-.6.43-2.3.5-3.02" }
            path { d: "M2 12a10 10 0 0 1 18-6" }
            path { d: "M2 16h.01" }
            path { d: "M21.8 16c.2-2 .131-5.354 0-6" }
            path { d: "M5 19.5C5.5 18 6 15 6 12a6 6 0 0 1 .34-2" }
            path { d: "M8.65 22c.21-.66.45-1.32.57-2" }
            path { d: "M9 6.8a6 6 0 0 1 9 5.2v2" }
        }
    }
}

/// A key on a ring: the password.
#[component]
pub fn KeyRound(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m2 21 9.6-9.6" }
            path { d: "m7.5 15.5 2.3 2.3a1 1 0 0 1 0 1.4l-2.1 2.1a1 1 0 0 1-1.4 0L4 19" }
            circle { cx: "15.5", cy: "7.5", r: "5.5" }
        }
    }
}

#[component]
pub fn Lock(#[props(default = 19)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { width: "18", height: "11", x: "3", y: "11", rx: "2", ry: "2" }
            path { d: "M7 11V7a5 5 0 0 1 10 0v4" }
        }
    }
}

#[component]
pub fn CircleCheck(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "m16 9-5.5 5.5L8 12" }
        }
    }
}

#[component]
pub fn ShieldCheck(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
            path { d: "m9 12 2 2 4-4" }
        }
    }
}

/// A device no longer trusted (P4-03).
#[component]
pub fn ShieldOff(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m2 2 20 20" }
            path { d: "M5 5a1 1 0 0 0-1 1v7c0 5 3.5 7.5 7.67 8.94a1 1 0 0 0 .67.01c2.35-.82 4.48-1.97 5.9-3.71" }
            path { d: "M9.309 3.652A12.252 12.252 0 0 0 11.24 2.28a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1v7a9.784 9.784 0 0 1-.08 1.264" }
        }
    }
}

/// Trust taken away (P4-03, WebTrustStates 4 and 5).
#[component]
pub fn ShieldAlert(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
            path { d: "M12 8v4" }
            path { d: "M12 16h.01" }
        }
    }
}

#[component]
pub fn ArrowRight(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M5 12h14" }
            path { d: "m12 5 7 7-7 7" }
        }
    }
}

#[component]
pub fn Pencil(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" }
            path { d: "m15 5 4 4" }
        }
    }
}

#[component]
pub fn Trash(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M10 11v6" }
            path { d: "M14 11v6" }
            path { d: "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" }
            path { d: "M3 6h18" }
            path { d: "M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" }
        }
    }
}

#[component]
pub fn Plus(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M5 12h14" }
            path { d: "M12 5v14" }
        }
    }
}

#[component]
pub fn Shield(#[props(default = 22)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
            path { d: "m9 12 2 2 4-4" }
        }
    }
}

#[component]
pub fn User(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2" }
            circle { cx: "12", cy: "7", r: "4" }
        }
    }
}

#[component]
pub fn Warning(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" }
            path { d: "M12 9v4" }
            path { d: "M12 17h.01" }
        }
    }
}

#[component]
pub fn LogOut(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m16 17 5-5-5-5" }
            path { d: "M21 12H9" }
            path { d: "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" }
        }
    }
}

#[component]
pub fn Monitor(#[props(default = 34)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { width: "20", height: "14", x: "2", y: "3", rx: "2" }
            line { x1: "8", x2: "16", y1: "21", y2: "21" }
            line { x1: "12", x2: "12", y1: "17", y2: "21" }
        }
    }
}

/// Dismiss.
#[component]
pub fn X(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M18 6 6 18" }
            path { d: "m6 6 12 12" }
        }
    }
}

/// Refused, or disabled.
#[component]
pub fn CircleX(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "m15 9-6 6" }
            path { d: "m9 9 6 6" }
        }
    }
}

#[component]
pub fn ChevronLeft(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m15 18-6-6 6-6" }
        }
    }
}

#[component]
pub fn ChevronRight(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m9 18 6-6-6-6" }
        }
    }
}

/// People: the users page's empty state.
#[component]
pub fn Users(#[props(default = 40)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }
            path { d: "M16 3.128a4 4 0 0 1 0 7.744" }
            path { d: "M22 21v-2a4 4 0 0 0-3-3.87" }
            circle { cx: "9", cy: "7", r: "4" }
        }
    }
}

/// A role change.
#[component]
pub fn UserCog(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m14.305 19.53.923-.382" }
            path { d: "m15.228 16.852-.923-.383" }
            path { d: "m16.852 15.228-.383-.923" }
            path { d: "m16.852 20.772-.383.924" }
            path { d: "m19.148 15.228.383-.923" }
            path { d: "m19.53 21.696-.382-.924" }
            path { d: "M2 21a8 8 0 0 1 10.434-7.62" }
            path { d: "m20.772 16.852.924-.383" }
            path { d: "m20.772 19.148.924.383" }
            circle { cx: "10", cy: "8", r: "5" }
            circle { cx: "18", cy: "18", r: "3" }
        }
    }
}

/// Try again.
#[component]
pub fn RotateCcw(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" }
            path { d: "M3 3v5h5" }
        }
    }
}

/// The phone bar's menu button, as the board draws it.
#[component]
pub fn Menu(#[props(default = 20)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M10 5H3" }
            path { d: "M12 19H3" }
            path { d: "M14 3v4" }
            path { d: "M16 17v4" }
            path { d: "M21 12h-9" }
            path { d: "M21 19h-5" }
            path { d: "M21 5h-7" }
            path { d: "M8 10v4" }
            path { d: "M8 12H3" }
        }
    }
}

/// The Server page's own card.
#[component]
pub fn Server(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { width: "20", height: "8", x: "2", y: "2", rx: "2", ry: "2" }
            rect { width: "20", height: "8", x: "2", y: "14", rx: "2", ry: "2" }
            line { x1: "6", x2: "6.01", y1: "6", y2: "6" }
            line { x1: "6", x2: "6.01", y1: "18", y2: "18" }
        }
    }
}

/// A GPU, for the Overview's GPUs (lucide's chip).
#[component]
pub fn Chip(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { width: "16", height: "16", x: "4", y: "4", rx: "2" }
            rect { width: "6", height: "6", x: "9", y: "9", rx: "1" }
            path { d: "M15 2v2" }
            path { d: "M15 20v2" }
            path { d: "M2 15h2" }
            path { d: "M2 9h2" }
            path { d: "M20 15h2" }
            path { d: "M20 9h2" }
            path { d: "M9 2v2" }
            path { d: "M9 20v2" }
        }
    }
}

/// Plex.
#[component]
pub fn Tv(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m17 2-5 5-5-5" }
            rect { width: "20", height: "15", x: "2", y: "7", rx: "2" }
        }
    }
}

/// Copy, as the boards draw it.
#[component]
pub fn Link(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" }
            path { d: "M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" }
        }
    }
}

/// A part of a circle, turned by `animate-spin`.
#[component]
pub fn Loader(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M21 12a9 9 0 1 1-6.219-8.56" }
        }
    }
}

/// The Spoolframe mark: a reel in a frame, at every size the pages show it
/// (the cards, the sidebar, the bars). The three-hole reel is only the
/// favicon, in public/.
#[component]
pub fn Mark(#[props(default = 56)] size: u32) -> Element {
    rsx! {
        svg { view_box: "0 0 100 100", width: "{size}", height: "{size}", "aria-hidden": "true",
            g { fill: "none", stroke: "currentColor", stroke_width: "4", stroke_linecap: "round", stroke_linejoin: "round",
                path { d: "M10 26 V15 a5 5 0 0 1 5 -5 H26" }
                path { d: "M74 10 H85 a5 5 0 0 1 5 5 V26" }
                path { d: "M90 74 V85 a5 5 0 0 1 -5 5 H74" }
                path { d: "M26 90 H15 a5 5 0 0 1 -5 -5 V74" }
            }
            circle { cx: "50", cy: "50", r: "31", fill: "var(--ctp-teal)" }
            circle { cx: "50", cy: "32", r: "7", fill: "#1d1d2b" }
            circle { cx: "67.12", cy: "44.44", r: "7", fill: "#1d1d2b" }
            circle { cx: "60.58", cy: "64.56", r: "7", fill: "#1d1d2b" }
            circle { cx: "39.42", cy: "64.56", r: "7", fill: "#1d1d2b" }
            circle { cx: "32.88", cy: "44.44", r: "7", fill: "#1d1d2b" }
            path { d: "M46.5 45 L55.5 50 L46.5 55 Z", fill: "#1d1d2b", stroke: "#1d1d2b", stroke_width: "1.5", stroke_linejoin: "round" }
        }
    }
}

#[component]
pub fn Check(#[props(default = 14)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M20 6 9 17l-5-5" }
        }
    }
}

/// A dashed circle: not there, or not used.
#[component]
pub fn CircleDashed(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M10.1 2.182a10 10 0 0 1 3.8 0" }
            path { d: "M13.9 21.818a10 10 0 0 1-3.8 0" }
            path { d: "M17.609 3.721a10 10 0 0 1 2.69 2.7" }
            path { d: "M2.182 13.9a10 10 0 0 1 0-3.8" }
            path { d: "M20.279 17.609a10 10 0 0 1-2.7 2.69" }
            path { d: "M21.818 10.1a10 10 0 0 1 0 3.8" }
            path { d: "M3.721 6.391a10 10 0 0 1 2.7-2.69" }
            path { d: "M6.391 20.279a10 10 0 0 1-2.69-2.7" }
        }
    }
}

#[component]
pub fn Info(#[props(default = 17)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "M12 16v-4" }
            path { d: "M12 8h.01" }
        }
    }
}

/// A clock turning back: a wizard picked up again.
#[component]
pub fn History(#[props(default = 20)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" }
            path { d: "M3 3v5h5" }
            path { d: "M12 7v5l4 2" }
        }
    }
}

/// A movie library (P3-11).
#[component]
pub fn Film(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { width: "18", height: "18", x: "3", y: "3", rx: "2" }
            path { d: "M7 3v18" }
            path { d: "M3 7.5h4" }
            path { d: "M3 12h18" }
            path { d: "M3 16.5h4" }
            path { d: "M17 3v18" }
            path { d: "M17 7.5h4" }
            path { d: "M17 16.5h4" }
        }
    }
}

/// A music library.
#[component]
pub fn Music(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M9 18V5l12-2v13" }
            circle { cx: "6", cy: "18", r: "3" }
            circle { cx: "18", cy: "16", r: "3" }
        }
    }
}

/// A row's handle for reordering.
#[component]
pub fn Grip(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            circle { cx: "9", cy: "12", r: "1" }
            circle { cx: "9", cy: "5", r: "1" }
            circle { cx: "9", cy: "19", r: "1" }
            circle { cx: "15", cy: "12", r: "1" }
            circle { cx: "15", cy: "5", r: "1" }
            circle { cx: "15", cy: "19", r: "1" }
        }
    }
}

#[component]
pub fn ChevronDown(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size, path { d: "m6 9 6 6 6-6" } }
    }
}

#[component]
pub fn ChevronUp(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size, path { d: "m18 15-6-6-6 6" } }
    }
}

/// A row's menu.
#[component]
pub fn Ellipsis(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            circle { cx: "12", cy: "12", r: "1" }
            circle { cx: "19", cy: "12", r: "1" }
            circle { cx: "5", cy: "12", r: "1" }
        }
    }
}

/// Plex's play mark, on the Plex account card (P3-12).
#[component]
pub fn Play(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" }
        }
    }
}

/// Opens in a new window: Plex's sign-in.
#[component]
pub fn ExternalLink(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M15 3h6v6" }
            path { d: "M10 14 21 3" }
            path { d: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" }
        }
    }
}

/// Unlinking a Plex account.
#[component]
pub fn Unlink(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m18.84 12.25 1.72-1.71h-.02a5.004 5.004 0 0 0-.12-7.07 5.006 5.006 0 0 0-6.95 0l-1.72 1.71" }
            path { d: "m5.17 11.75-1.71 1.71a5.004 5.004 0 0 0 .12 7.07 5.006 5.006 0 0 0 6.95 0l1.71-1.71" }
            line { x1: "8", x2: "8", y1: "2", y2: "5" }
            line { x1: "2", x2: "5", y1: "8", y2: "8" }
            line { x1: "16", x2: "16", y1: "19", y2: "22" }
            line { x1: "19", x2: "22", y1: "16", y2: "16" }
        }
    }
}

/// My profile's Playback card (P3-03).
#[component]
pub fn Captions(#[props(default = 18)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { width: "18", height: "14", x: "3", y: "5", rx: "2", ry: "2" }
            path { d: "M7 15h4M15 15h2M7 11h2M13 11h4" }
        }
    }
}

/// A laptop: a device of the user's.
#[component]
pub fn Laptop(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M18 5a2 2 0 0 1 2 2v8.526a2 2 0 0 0 .212.897l1.068 2.127a1 1 0 0 1-.9 1.45H3.62a1 1 0 0 1-.9-1.45l1.068-2.127A2 2 0 0 0 4 15.526V7a2 2 0 0 1 2-2z" }
            path { d: "M20.054 15.987H3.946" }
        }
    }
}

/// A sign-in provider (P3-04).
#[component]
pub fn Globe(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20" }
            path { d: "M2 12h20" }
        }
    }
}

/// Linking a sign-in provider (P3-04).
#[component]
pub fn LinkChain(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" }
            path { d: "M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" }
        }
    }
}

/// Seen, not changed: the catalog's "Read-only" (P3-16).
#[component]
pub fn Eye(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" }
            circle { cx: "12", cy: "12", r: "3" }
        }
    }
}

#[component]
pub fn Search(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "m21 21-4.34-4.34" }
            circle { cx: "11", cy: "11", r: "8" }
        }
    }
}

/// The catalog's Columns menu.
#[component]
pub fn Sliders(#[props(default = 14)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M10 5H3" }
            path { d: "M12 19H3" }
            path { d: "M14 3v4" }
            path { d: "M16 17v4" }
            path { d: "M21 12h-9" }
            path { d: "M21 19h-5" }
            path { d: "M21 5h-7" }
            path { d: "M8 10v4" }
            path { d: "M8 12H3" }
        }
    }
}

/// Playback debug (P3-15), on an item's Debug tab.
#[component]
pub fn Activity(#[props(default = 14)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2" }
        }
    }
}

/// Stops a page asking for new numbers or lines (P3-14).
#[component]
pub fn Pause(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { x: "14", y: "3", width: "5", height: "18", rx: "1" }
            rect { x: "5", y: "3", width: "5", height: "18", rx: "1" }
        }
    }
}

/// Saves what a page shows as a file (P3-14).
#[component]
pub fn Download(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M12 15V3" }
            path { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }
            path { d: "m7 10 5 5 5-5" }
        }
    }
}

/// An invite (P6-01): a ticket.
#[component]
pub fn Ticket(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M2 9a3 3 0 0 1 0 6v2a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-2a3 3 0 0 1 0-6V7a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2Z" }
            path { d: "M13 5v2" }
            path { d: "M13 17v2" }
            path { d: "M13 11v2" }
        }
    }
}

/// Show a QR code.
#[component]
pub fn QrCode(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { x: "3", y: "3", width: "7", height: "7", rx: "1" }
            rect { x: "14", y: "3", width: "7", height: "7", rx: "1" }
            rect { x: "3", y: "14", width: "7", height: "7", rx: "1" }
            path { d: "M14 14h3v3h-3z" }
            path { d: "M21 14v.01" }
            path { d: "M21 21h-4v-4" }
            path { d: "M14 21v-.01" }
        }
    }
}

/// Copy to the clipboard.
#[component]
pub fn Copy(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            rect { x: "8", y: "8", width: "14", height: "14", rx: "2" }
            path { d: "M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" }
        }
    }
}

/// Send through the system's share sheet.
#[component]
pub fn Share(#[props(default = 16)] size: u32) -> Element {
    rsx! {
        Stroke { size,
            path { d: "M4 12v8a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-8" }
            path { d: "m16 6-4-4-4 4" }
            path { d: "M12 2v13" }
        }
    }
}

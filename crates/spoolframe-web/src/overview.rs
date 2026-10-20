//! What the admin's Overview says (WebAdminPhone 4, P3-02), apart from the
//! page: a playback's two lines.

use spoolframe_admin_api::{PlanKind, PlaybackRow};

/// A playback as "Playing now" lists it: "Alice · Severance S2 E4", then
/// "Living room Apple TV · direct".
pub fn playing_lines(row: &PlaybackRow) -> (String, String) {
    let who = if row.display_name.is_empty() {
        &row.username
    } else {
        &row.display_name
    };
    let plan = match row.plan {
        PlanKind::Direct => "direct",
        PlanKind::Remux => "remux",
        PlanKind::Transcode => "transcode",
        PlanKind::Unsupported => "cannot play",
    };
    let http = if row.over_http { " · over HTTP" } else { "" };
    (
        format!("{who} · {}", row.title.replace(" · ", " ")),
        format!("{} · {plan}{http}", row.device),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::{PlanLine, Platform};

    fn row(display_name: &str, title: &str, plan: PlanKind) -> PlaybackRow {
        serde_json::from_value::<PlaybackRow>(serde_json::json!({
            "id": "p1",
            "userId": "local:1",
            "username": "alice",
            "displayName": display_name,
            "device": "Living room Apple TV",
            "platform": Platform::Tvos,
            "item": "plex:episode:1",
            "title": title,
            "subtitle": null,
            "durationMs": null,
            "plan": plan,
            "line": serde_json::to_value(PlanLine {
                kind: "direct".into(),
                delivery: "range".into(),
                video: "copy:h264".into(),
                audio: "copy:eac3".into(),
                subtitle: "none".into(),
            })
            .unwrap(),
            "reasons": [],
            "epoch": 0,
            "revision": 0,
            "startedAt": 0,
            "positionMs": null,
            "readToMs": null,
            "furthest": null,
            "lastReadAt": null,
            "conversion": null,
            "made": {
                "decoder": { "kind": "none" },
                "encoder": null,
                "toneMap": "none",
                "toneMapper": null,
                "burnIn": "none",
                "width": 1920,
                "height": 1080,
                "targetBitrate": null,
                "measuredBitrate": null,
                "copy": null,
            },
            "start": {
                "planMs": 12,
                "keyframes": { "from": "none" },
                "firstSegment": { "state": "notApplicable" },
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_playback_says_who_what_where_and_how() {
        assert_eq!(
            playing_lines(&row("Alice", "Severance · S2 E4", PlanKind::Direct)),
            (
                "Alice · Severance S2 E4".to_owned(),
                "Living room Apple TV · direct".to_owned()
            )
        );
    }

    /// PX-49: one a player fetches from the HTTP playback port.
    #[test]
    fn a_playback_played_over_http_says_so() {
        let mut over_http = row("Alice", "Alien", PlanKind::Transcode);
        over_http.over_http = true;
        assert_eq!(
            playing_lines(&over_http).1,
            "Living room Apple TV · transcode · over HTTP"
        );
    }

    #[test]
    fn a_playback_without_a_display_name_names_the_username() {
        assert_eq!(
            playing_lines(&row("", "Alien", PlanKind::Transcode)).0,
            "alice · Alien"
        );
    }
}

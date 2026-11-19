//! Search and commands (the WebCommandPalette board): Ctrl K / ⌘K, or the
//! top bar's box, on every admin page. What it finds and how it ranks
//! and groups it is here, host-tested; the dialog is `app::palette`.
//!
//! Four groups: Settings (from `admin.settingsIndex`, with the value
//! each is set to), Library (`admin.catalogSearch`), Admin (its pages,
//! people, their devices and paired servers) and Actions
//! (`admin.adminActions`). Recents stay in the browser.

use serde::{Deserialize, Serialize};
use spoolframe_admin_api::{
    AdminAction, AdminActionKind, CatalogHit, DeviceEntry, SettingArea, SettingEntry,
    SettingIndexEntry, UserInfo,
};
use spoolframe_types::Item;

use crate::settings_marks;

/// A group of hits, in the order the palette lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Group {
    Settings,
    Library,
    Admin,
    Actions,
}

impl Group {
    pub const ALL: [Group; 4] = [
        Group::Settings,
        Group::Library,
        Group::Admin,
        Group::Actions,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Group::Settings => "Settings",
            Group::Library => "Library",
            Group::Admin => "Admin",
            Group::Actions => "Actions",
        }
    }

    /// The most hits the group shows while every group is (filter All).
    fn cap(self) -> usize {
        match self {
            Group::Settings => 6,
            Group::Library => 6,
            Group::Admin => 6,
            Group::Actions => 4,
        }
    }
}

/// The filter chips: every group, or one. Tab moves to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Only(Group),
}

impl Filter {
    pub const CHIPS: [Filter; 5] = [
        Filter::All,
        Filter::Only(Group::Settings),
        Filter::Only(Group::Library),
        Filter::Only(Group::Admin),
        Filter::Only(Group::Actions),
    ];

    pub fn name(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Only(group) => group.name(),
        }
    }

    /// The next chip, as Tab moves (Shift Tab: back), round.
    pub fn next(self, back: bool) -> Filter {
        let at = Self::CHIPS.iter().position(|f| *f == self).unwrap_or(0);
        let n = Self::CHIPS.len();
        Self::CHIPS[if back { (at + n - 1) % n } else { (at + 1) % n }]
    }

    fn keeps(self, group: Group) -> bool {
        match self {
            Filter::All => true,
            Filter::Only(only) => only == group,
        }
    }
}

/// An admin page the palette opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Overview,
    Users,
    Invites,
    Federation,
    Server,
    Activity,
    Playback,
    Optimized,
    Logs,
    Libraries,
    Playlists,
    Settings,
    Audit,
    Diagnostics,
    Licenses,
    Profile,
}

/// The admin pages, by name, with other words they are found by.
const PAGES: &[(Page, &str, &str)] = &[
    (Page::Overview, "Overview", "home dashboard"),
    (Page::Users, "Users and devices", "people accounts"),
    (Page::Invites, "Invites", "invite link code"),
    (Page::Federation, "Federation", "paired servers sharing"),
    (Page::Server, "Server", "status media paths"),
    (Page::Activity, "Activity", "streams now playing"),
    (Page::Playback, "Playback", "plays transcodes"),
    (Page::Optimized, "Optimized", "versions encodes"),
    (Page::Logs, "Logs", "log"),
    (Page::Libraries, "Libraries", "sources plex folders"),
    (Page::Playlists, "Playlists", "server playlists"),
    (Page::Settings, "Settings", "configuration"),
    (Page::Audit, "Audit", "who changed what"),
    (Page::Diagnostics, "Diagnostics", "reports"),
    (Page::Licenses, "Open source licenses", "licenses"),
    (Page::Profile, "My profile", "account password passkeys"),
];

/// Where a hit leads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// A setting's field: on `area`'s page (the server's settings
    /// schema says which), element `anchor`, in a collapsed Advanced
    /// section if `advanced`.
    Setting {
        key: String,
        area: SettingArea,
        anchor: String,
        advanced: bool,
    },
    /// A library item's page.
    Item {
        library: String,
        id: String,
    },
    /// A user's page (their devices are on it too).
    User {
        username: String,
    },
    /// A paired server's page.
    Peer {
        id: String,
    },
    Page(Page),
    /// An action; `item` is the library item it acts on, with its title.
    Action {
        kind: AdminActionKind,
        item: Option<(String, String, String)>,
    },
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub group: Group,
    /// What it is called: "App port", "Alien", "Alice".
    pub title: String,
    /// The line under it: "Network · The UDP port the apps connect to".
    pub detail: String,
    /// At the end of the row: a setting's value, an item's library, "User".
    pub tag: String,
    pub target: Target,
    /// Ranking within the group: lower first.
    #[serde(skip)]
    pub rank: u32,
}

/// How well `query` matches `fields`, the best first: `None` if it
/// matches none. A field's rank goes before how it matched: the same
/// text, its start, a word's start, anywhere in it. Letter case does not
/// count.
pub fn rank(query: &str, fields: &[&str]) -> Option<u32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    fields
        .iter()
        .enumerate()
        .filter_map(|(at, field)| {
            let field = field.to_lowercase();
            let how = if field == query {
                0
            } else if field.starts_with(&query) {
                1
            } else if field
                .match_indices(&query)
                .any(|(i, _)| !field[..i].ends_with(|c: char| c.is_alphanumeric()))
            {
                2
            } else if field.contains(&query) {
                3
            } else {
                return None;
            };
            Some(at as u32 * 10 + how)
        })
        .min()
}

/// The name of `area` as the settings pages give it.
fn area_name(area: SettingArea) -> &'static str {
    settings_marks::AREAS
        .iter()
        .find(|(a, _)| *a == area)
        .map_or("Settings", |(_, name)| name)
}

/// The settings `query` finds, each with what it is set to now.
pub fn setting_hits(
    query: &str,
    index: &[SettingIndexEntry],
    entries: &[SettingEntry],
) -> Vec<Hit> {
    index
        .iter()
        .filter_map(|i| {
            let rank = rank(query, &[&i.label, &i.key, &i.help])?;
            let entry = entries.iter().find(|e| e.key == i.key);
            let tag = match entry {
                Some(e) if e.secret => if e.set { "Set" } else { "Not set" }.to_owned(),
                Some(e) => e
                    .value
                    .as_ref()
                    .map_or_else(String::new, |v| settings_marks::shown(&e.key, v)),
                None => String::new(),
            };
            Some(Hit {
                group: Group::Settings,
                title: i.label.clone(),
                detail: format!("{} · {}", area_name(i.area), i.help),
                tag,
                target: Target::Setting {
                    key: i.key.clone(),
                    area: i.area,
                    anchor: i.anchor.clone(),
                    advanced: i.advanced,
                },
                rank,
            })
        })
        .collect()
}

/// The line under a library hit: "Movies · 1979", "The Bear · S1E2".
pub fn item_detail(hit: &CatalogHit) -> String {
    let year = |y: Option<u16>| y.map(|y| format!(" · {y}")).unwrap_or_default();
    match &hit.row.item {
        Item::Movie(m) => format!("{}{}", hit.library_title, year(m.year)),
        Item::Show(s) => match s.season_count {
            Some(1) => format!("{}{} · 1 season", hit.library_title, year(s.year)),
            Some(n) => format!("{}{} · {n} seasons", hit.library_title, year(s.year)),
            None => format!("{}{}", hit.library_title, year(s.year)),
        },
        Item::Season(s) => s
            .show_title
            .clone()
            .unwrap_or_else(|| hit.library_title.clone()),
        Item::Episode(e) => format!(
            "{} · S{}E{}",
            e.show_title.as_deref().unwrap_or(&hit.library_title),
            e.season,
            e.episode
        ),
        Item::Album(a) => format!(
            "{}{}",
            a.artist_name.as_deref().unwrap_or(&hit.library_title),
            year(a.year)
        ),
        Item::Track(t) => [t.artist_name.as_deref(), t.album_title.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        Item::Artist(_) => hit.library_title.clone(),
    }
}

/// `catalogSearch`'s hits, in its order.
pub fn library_hits(hits: &[CatalogHit]) -> Vec<Hit> {
    hits.iter()
        .enumerate()
        .map(|(at, hit)| Hit {
            group: Group::Library,
            title: hit.row.item.title().to_owned(),
            detail: item_detail(hit),
            tag: hit.library_title.clone(),
            target: Target::Item {
                library: hit.library.clone(),
                id: hit.row.item.id().to_string(),
            },
            rank: at as u32,
        })
        .collect()
}

/// What the Admin group searches: the people, each with their devices,
/// and the paired servers by id and name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct People {
    pub users: Vec<(UserInfo, Vec<DeviceEntry>)>,
    pub peers: Vec<(String, String)>,
}

/// The admin pages, people, devices and paired servers `query` finds.
pub fn admin_hits(query: &str, people: &People) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (page, name, words) in PAGES {
        if let Some(rank) = rank(query, &[name, words]) {
            hits.push(Hit {
                group: Group::Admin,
                title: (*name).to_owned(),
                detail: "Admin page".into(),
                tag: String::new(),
                target: Target::Page(*page),
                rank: rank + 20,
            });
        }
    }
    for (user, devices) in &people.users {
        let shown = user.display_name.as_deref().unwrap_or(&user.username);
        if let Some(rank) = rank(query, &[shown, &user.username]) {
            let devices_line = match devices.len() {
                1 => "1 device".to_owned(),
                n => format!("{n} devices"),
            };
            hits.push(Hit {
                group: Group::Admin,
                title: shown.to_owned(),
                detail: format!("User · {devices_line}"),
                tag: "User".into(),
                target: Target::User {
                    username: user.username.clone(),
                },
                rank,
            });
        }
        for device in devices {
            let Some(name) = device.name.as_deref() else {
                continue;
            };
            if let Some(rank) = rank(query, &[name]) {
                let trusted = if device.trusted { " · trusted" } else { "" };
                hits.push(Hit {
                    group: Group::Admin,
                    title: name.to_owned(),
                    detail: format!("Device of {shown}{trusted}"),
                    tag: "Device".into(),
                    target: Target::User {
                        username: user.username.clone(),
                    },
                    rank: rank + 1,
                });
            }
        }
    }
    for (id, name) in &people.peers {
        if let Some(rank) = rank(query, &[name]) {
            hits.push(Hit {
                group: Group::Admin,
                title: name.clone(),
                detail: "Paired server · Federation".into(),
                tag: "Server".into(),
                target: Target::Peer { id: id.clone() },
                rank: rank + 2,
            });
        }
    }
    hits
}

/// Whether a library hit is a film or an episode: what has subtitles
/// of its own to find.
fn has_subtitles(hit: &Hit) -> bool {
    matches!(&hit.target, Target::Item { id, .. }
        if matches!(id.split(':').nth(1), Some("movie" | "episode")))
}

/// The actions `query` finds; an action on an item is offered for the
/// first film or episode of `library`, the library hits, named in its
/// title.
pub fn action_hits(query: &str, actions: &[AdminAction], library: &[Hit]) -> Vec<Hit> {
    let item = library.iter().find(|h| has_subtitles(h));
    actions
        .iter()
        .filter_map(|action| {
            let on = match (&action.needs_item, item) {
                (
                    true,
                    Some(Hit {
                        title,
                        target: Target::Item { library, id },
                        ..
                    }),
                ) => Some((library.clone(), id.clone(), title.clone())),
                (true, _) => return None,
                (false, _) => None,
            };
            let title = match &on {
                Some((_, _, item)) => {
                    format!("{} for “{item}”…", action.label.trim_end_matches('…'))
                }
                None => action.label.clone(),
            };
            // An action on an item comes with the item: the search found
            // it, not the action's words.
            let rank = if on.is_some() {
                Some(5)
            } else {
                rank(query, &[&action.label, &action.description])
            }?;
            Some(Hit {
                group: Group::Actions,
                title,
                detail: action.description.clone(),
                tag: String::new(),
                target: Target::Action {
                    kind: action.kind,
                    item: on,
                },
                rank,
            })
        })
        .collect()
}

/// The actions offered before anything is typed: those that need no
/// item.
pub fn suggested(actions: &[AdminAction]) -> Vec<Hit> {
    actions
        .iter()
        .filter(|a| !a.needs_item)
        .map(|a| Hit {
            group: Group::Actions,
            title: a.label.clone(),
            detail: a.description.clone(),
            tag: String::new(),
            target: Target::Action {
                kind: a.kind,
                item: None,
            },
            rank: 0,
        })
        .collect()
}

/// The list as shown: each group's hits, best first, `filter`'s groups
/// only, every group cut to its few while all are shown; and each
/// group's whole count, for its chip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Results {
    pub groups: Vec<(Group, Vec<Hit>)>,
    pub counts: Vec<(Group, usize)>,
}

impl Results {
    pub fn of(mut hits: Vec<Hit>, filter: Filter) -> Self {
        hits.sort_by_key(|h| h.rank);
        let mut groups = Vec::new();
        let mut counts = Vec::new();
        for group in Group::ALL {
            let mine: Vec<Hit> = hits.iter().filter(|h| h.group == group).cloned().collect();
            counts.push((group, mine.len()));
            if mine.is_empty() || !filter.keeps(group) {
                continue;
            }
            let cap = match filter {
                Filter::All => group.cap(),
                Filter::Only(_) => usize::MAX,
            };
            groups.push((group, mine.into_iter().take(cap).collect()));
        }
        Results { groups, counts }
    }

    /// Every hit's count, for All's chip.
    pub fn total(&self) -> usize {
        self.counts.iter().map(|(_, n)| n).sum()
    }

    /// The hits in the order Up and Down move through them.
    pub fn flat(&self) -> Vec<&Hit> {
        self.groups.iter().flat_map(|(_, hits)| hits).collect()
    }
}

/// `text` cut where `query` matches it (letter case aside), each piece
/// with whether it is the match, which the list shows in bold. The first
/// match only; all of `text`, unmarked, when it does not match.
pub fn marked(text: &str, query: &str) -> Vec<(String, bool)> {
    let query = query.trim();
    let found = (!query.is_empty())
        .then(|| {
            // By characters, so a match is cut where `text`'s own
            // characters are, whatever lower case does to their lengths.
            let chars: Vec<(usize, char)> = text.char_indices().collect();
            let wanted: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
            (0..chars.len()).find_map(|start| {
                let mut lowered = Vec::new();
                for (end, &(_, c)) in chars.iter().enumerate().skip(start) {
                    lowered.extend(c.to_lowercase());
                    if lowered.len() >= wanted.len() {
                        let to = chars.get(end + 1).map_or(text.len(), |(i, _)| *i);
                        return (lowered == wanted).then_some((chars[start].0, to));
                    }
                }
                None
            })
        })
        .flatten();
    match found {
        Some((from, to)) => [
            (text[..from].to_owned(), false),
            (text[from..to].to_owned(), true),
            (text[to..].to_owned(), false),
        ]
        .into_iter()
        .filter(|(piece, _)| !piece.is_empty())
        .collect(),
        None => vec![(text.to_owned(), false)],
    }
}

/// The shortcut as this computer's keyboard names it: ⌘K on a Mac (its
/// `navigator.platform`), Ctrl K elsewhere.
pub fn shortcut(platform: &str) -> &'static str {
    if platform.starts_with("Mac") || platform.starts_with("iP") {
        "⌘K"
    } else {
        "Ctrl K"
    }
}

/// Where this browser keeps the recent hits.
pub const RECENT_KEY: &str = "spoolframe.palette.recent";

/// The most recent hits kept.
pub const RECENT_MAX: usize = 5;

/// `kept` with `opened` first: once, and at most [`RECENT_MAX`]. An
/// action is not kept: it is offered anyway.
pub fn remember(mut kept: Vec<Hit>, opened: &Hit) -> Vec<Hit> {
    if matches!(opened.target, Target::Action { .. }) {
        return kept;
    }
    kept.retain(|h| h.target != opened.target);
    kept.insert(0, opened.clone());
    kept.truncate(RECENT_MAX);
    kept
}

/// The recent hits as this browser keeps them. One that no longer reads
/// (kept by an older web UI) is left out, and the rest kept.
pub fn recent_from(kept: Option<&str>) -> Vec<Hit> {
    kept.and_then(|k| serde_json::from_str::<Vec<serde_json::Value>>(k).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|hit| serde_json::from_value(hit).ok())
        .collect()
}

/// The recent hits as they are kept.
pub fn recent_to(recent: &[Hit]) -> String {
    serde_json::to_string(recent).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spoolframe_admin_api::{CatalogRow, Role, SettingApply, SettingLayer};
    use spoolframe_types::Show;

    use super::*;

    fn index(key: &str, label: &str, help: &str, area: SettingArea) -> SettingIndexEntry {
        SettingIndexEntry {
            key: key.into(),
            label: label.into(),
            help: help.into(),
            area,
            anchor: format!("setting-{}", key.replace('.', "-")),
            advanced: false,
        }
    }

    fn entry(key: &str, value: serde_json::Value) -> SettingEntry {
        SettingEntry {
            key: key.into(),
            value: Some(value),
            secret: false,
            set: true,
            layer: Some(SettingLayer::Runtime),
            locked: false,
            overrides: None,
            apply: SettingApply::Restart,
            restart_pending: false,
            running: None,
            default: None,
        }
    }

    fn settings() -> (Vec<SettingIndexEntry>, Vec<SettingEntry>) {
        (
            vec![
                index(
                    "network.port",
                    "App port",
                    "The UDP port the apps connect to.",
                    SettingArea::Network,
                ),
                index(
                    "network.plex_discovery_port",
                    "Plex discovery port",
                    "Where Plex apps on this network look for the server.",
                    SettingArea::Network,
                ),
                index(
                    "transcode.max_jobs",
                    "Encode budget",
                    "Software video encodes at once.",
                    SettingArea::Transcoding,
                ),
                index(
                    "admin.metrics",
                    "Serve /metrics",
                    "Prometheus text on the web UI's address. Its port is the web's.",
                    SettingArea::Monitoring,
                ),
            ],
            vec![
                entry("network.port", json!(7420)),
                entry("network.plex_discovery_port", json!(32415)),
            ],
        )
    }

    fn user(name: &str, devices: &[(&str, bool)]) -> (UserInfo, Vec<DeviceEntry>) {
        (
            UserInfo {
                username: name.to_lowercase(),
                display_name: Some(name.into()),
                role: Role::Consumer,
                created_at: 0,
                disabled: false,
                left_at: None,
                has_password: true,
                password_changed_at: None,
                password_must_change: false,
                password_expires_at: None,
                last_seen_at: None,
                has_passkey: false,
                has_sso: false,
                has_pin: false,
                device_count: devices.len() as u64,
                session_count: 0,
                rate_limited_secs: None,
                portable: Vec::new(),
            },
            devices
                .iter()
                .map(|(name, trusted)| DeviceEntry {
                    endpoint_id: format!("{name}-key"),
                    name: Some((*name).into()),
                    platform: None,
                    last_seen_at: 0,
                    pin_required: false,
                    trusted: *trusted,
                    auto_login: false,
                })
                .collect(),
        )
    }

    fn show(title: &str, seasons: u32) -> CatalogHit {
        CatalogHit {
            library: "plex:1".into(),
            library_title: "Shows".into(),
            row: CatalogRow {
                item: Item::Show(Show {
                    merge: Default::default(),
                    id: "plex:show:52011".parse().unwrap(),
                    title: title.into(),
                    year: Some(2024),
                    summary: None,
                    content_rating: None,
                    season_count: Some(seasons),
                    genres: Vec::new(),
                    last_aired: None,
                    cast: Vec::new(),
                    artwork: Vec::new(),
                }),
                matched: true,
                guid: None,
                agents: Vec::new(),
                added_at: None,
                updated_at: None,
                children: None,
                leaves: None,
                files: Vec::new(),
                missing: None,
            },
        }
    }

    fn actions() -> Vec<AdminAction> {
        let action = |kind, label: &str, needs_item| AdminAction {
            kind,
            label: label.into(),
            description: format!("what {label} does"),
            confirm: false,
            recent_sign_in: false,
            needs_item,
        };
        vec![
            action(AdminActionKind::Invite, "Invite someone…", false),
            action(AdminActionKind::CheckPaths, "Check media paths", false),
            action(AdminActionKind::Restart, "Restart server…", false),
            action(AdminActionKind::FindSubtitles, "Find subtitles…", true),
        ]
    }

    #[test]
    fn a_name_matched_whole_or_at_its_start_ranks_before_one_matched_inside() {
        assert_eq!(rank("port", &["port"]), Some(0));
        assert_eq!(rank("Port", &["Portable"]), Some(1));
        assert_eq!(rank("port", &["App port"]), Some(2));
        assert_eq!(rank("port", &["Airport"]), Some(3));
        assert_eq!(rank("port", &["Encode budget"]), None);
        // A later field (the key, then help) ranks after any match in an
        // earlier one.
        assert_eq!(
            rank("max_jobs", &["Encode budget", "transcode.max_jobs"]),
            Some(12)
        );
        assert_eq!(rank("  ", &["anything"]), None);
    }

    #[test]
    fn port_finds_the_settings_named_for_it_first_each_with_its_value() {
        let (index, entries) = settings();
        let results = Results::of(setting_hits("port", &index, &entries), Filter::All);
        let hits = results.flat();
        let titles: Vec<_> = hits.iter().map(|h| h.title.as_str()).collect();
        // The help that mentions a port comes after the names that do.
        assert_eq!(
            titles,
            ["App port", "Plex discovery port", "Serve /metrics"]
        );
        assert_eq!(hits[0].tag, "7420");
        assert_eq!(
            hits[0].detail,
            "Network · The UDP port the apps connect to."
        );
        assert_eq!(
            hits[0].target,
            Target::Setting {
                key: "network.port".into(),
                area: SettingArea::Network,
                anchor: "setting-network-port".into(),
                advanced: false,
            }
        );
    }

    #[test]
    fn a_setting_is_found_by_its_key_as_the_config_names_it() {
        let (index, entries) = settings();
        let hits = setting_hits("transcode.max_jobs", &index, &entries);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Encode budget");
    }

    #[test]
    fn a_person_comes_with_their_devices_and_a_paired_server_of_the_name() {
        let people = People {
            users: vec![
                user(
                    "Alice",
                    &[("Alice's laptop", false), ("Alice's iPhone", true)],
                ),
                user("Bob", &[("Bob's TV", false)]),
            ],
            peers: vec![("peer-1".into(), "Alice's server".into())],
        };
        let hits = Results::of(admin_hits("alice", &people), Filter::All);
        let rows: Vec<_> = hits
            .flat()
            .iter()
            .map(|h| (h.title.clone(), h.detail.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Alice".to_owned(), "User · 2 devices".to_owned()),
                ("Alice's laptop".to_owned(), "Device of Alice".to_owned()),
                (
                    "Alice's iPhone".to_owned(),
                    "Device of Alice · trusted".to_owned()
                ),
                (
                    "Alice's server".to_owned(),
                    "Paired server · Federation".to_owned()
                ),
            ]
        );
        let pages = admin_hits("invite", &People::default());
        assert_eq!(pages[0].target, Target::Page(Page::Invites));
    }

    #[test]
    fn library_hits_say_where_they_are() {
        let hits = library_hits(&[show("Alien: Earth", 1), show("Alien Worlds", 2)]);
        assert_eq!(hits[0].detail, "Shows · 2024 · 1 season");
        assert_eq!(hits[1].detail, "Shows · 2024 · 2 seasons");
        assert_eq!(hits[0].tag, "Shows");
        assert_eq!(
            hits[0].target,
            Target::Item {
                library: "plex:1".into(),
                id: "plex:show:52011".into(),
            }
        );
    }

    #[test]
    fn an_action_on_an_item_names_the_first_film_or_episode_found() {
        let mut library = library_hits(&[show("Alien: Earth", 1), show("Alien", 1)]);
        // A show has no subtitles of its own: nothing to find them for.
        assert!(action_hits("alien", &actions(), &library).is_empty());
        library[1].target = Target::Item {
            library: "plex:2".into(),
            id: "plex:movie:7".into(),
        };
        let hits = action_hits("alien", &actions(), &library);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Find subtitles for “Alien”…");
        assert!(matches!(
            &hits[0].target,
            Target::Action { kind: AdminActionKind::FindSubtitles, item: Some((_, id, t)) }
                if t == "Alien" && id == "plex:movie:7"
        ));
        // With no item, it is not offered at all.
        assert!(action_hits("subtitles", &actions(), &[]).is_empty());
        assert_eq!(
            action_hits("restart", &actions(), &[])[0].title,
            "Restart server…"
        );
    }

    #[test]
    fn before_anything_is_typed_the_actions_that_need_no_item_are_suggested() {
        let titles: Vec<_> = suggested(&actions()).into_iter().map(|h| h.title).collect();
        assert_eq!(
            titles,
            ["Invite someone…", "Check media paths", "Restart server…"]
        );
    }

    #[test]
    fn all_cuts_each_group_short_and_a_group_s_chip_shows_all_of_it() {
        let hits: Vec<Hit> = (0..10)
            .map(|n| Hit {
                group: Group::Library,
                title: format!("Alien {n}"),
                detail: String::new(),
                tag: String::new(),
                target: Target::Page(Page::Libraries),
                rank: n,
            })
            .collect();
        let all = Results::of(hits.clone(), Filter::All);
        assert_eq!(all.flat().len(), 6);
        assert_eq!(all.total(), 10);
        assert_eq!(all.counts[1], (Group::Library, 10));
        let library = Results::of(hits.clone(), Filter::Only(Group::Library));
        assert_eq!(library.flat().len(), 10);
        let settings = Results::of(hits, Filter::Only(Group::Settings));
        assert!(settings.groups.is_empty());
        // Every chip's count stays, whatever the filter.
        assert_eq!(settings.total(), 10);
    }

    #[test]
    fn the_match_is_cut_out_for_bold_whatever_its_case() {
        assert_eq!(
            marked("App port", "PORT"),
            [("App ".to_owned(), false), ("port".to_owned(), true)]
        );
        assert_eq!(marked("Alien", "alien"), [("Alien".to_owned(), true)]);
        assert_eq!(marked("Alien", "xqzv"), [("Alien".to_owned(), false)]);
        assert_eq!(marked("Alien", " "), [("Alien".to_owned(), false)]);
        // Characters whose lower case is longer cut where the text's are.
        assert_eq!(
            marked("İstanbul days", "days"),
            [("İstanbul ".to_owned(), false), ("days".to_owned(), true)]
        );
    }

    #[test]
    fn a_mac_names_the_shortcut_with_its_command_key() {
        assert_eq!(shortcut("MacIntel"), "⌘K");
        assert_eq!(shortcut("Linux x86_64"), "Ctrl K");
        assert_eq!(shortcut("Win32"), "Ctrl K");
    }

    #[test]
    fn tab_moves_through_the_chips_round() {
        assert_eq!(Filter::All.next(false), Filter::Only(Group::Settings));
        assert_eq!(Filter::Only(Group::Actions).next(false), Filter::All);
        assert_eq!(Filter::All.next(true), Filter::Only(Group::Actions));
    }

    #[test]
    fn recents_keep_the_latest_five_once_each_and_no_actions() {
        let (index, entries) = settings();
        let port = setting_hits("App port", &index, &entries).remove(0);
        let budget = setting_hits("Encode budget", &index, &entries).remove(0);
        let mut kept = remember(Vec::new(), &port);
        kept = remember(kept, &budget);
        kept = remember(kept, &port);
        assert_eq!(
            kept.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
            ["App port", "Encode budget"]
        );
        let restart = action_hits("restart", &actions(), &[]).remove(0);
        assert_eq!(remember(kept.clone(), &restart), kept);
        for n in 0..10 {
            let page = Hit {
                title: format!("page {n}"),
                target: Target::Peer { id: n.to_string() },
                ..port.clone()
            };
            kept = remember(kept, &page);
        }
        assert_eq!(kept.len(), RECENT_MAX);
        assert_eq!(
            recent_from(Some(&recent_to(&kept))),
            kept.iter()
                .map(|h| Hit {
                    rank: 0,
                    ..h.clone()
                })
                .collect::<Vec<_>>()
        );
        assert!(recent_from(Some("not json")).is_empty());
        assert!(recent_from(None).is_empty());
        // A setting kept as an older web UI kept it (its page's slug, not
        // its area) is left out; the rest stay.
        let mut older: Vec<serde_json::Value> =
            serde_json::from_str(&recent_to(&kept[..2])).unwrap();
        let mut stale = serde_json::to_value(&port).unwrap();
        let setting = stale["target"]["Setting"].as_object_mut().unwrap();
        setting.remove("area").unwrap();
        setting.insert("tab".into(), json!("network"));
        older.push(stale);
        let read = recent_from(Some(&serde_json::to_string(&older).unwrap()));
        assert_eq!(read.len(), 2);
    }
}

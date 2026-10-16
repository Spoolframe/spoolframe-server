//! An MCP server at `/mcp` on the admin listener (PX-42): tools an AI
//! agent (Claude, or any MCP client) calls for a user, with that user's
//! rights and only the scopes the user granted it.
//!
//! The transport is MCP's Streamable HTTP, stateless, each call answered
//! with JSON: every POST carries the client's OAuth access token
//! (`crate::oauth`), checked before the call reaches here, and nothing is
//! kept between calls. The token's user and scopes come in as
//! [`McpCaller`].
//!
//! Tools are thin: typed arguments, paged lists, ids and names, never a
//! file path. Each calls what the client protocol or the admin API calls,
//! so an agent gets the answers a user's app would. A tool the token's
//! scopes do not cover is not listed, and calling it anyway is refused.
//! Every call is counted for the client (its last call, its calls today)
//! and logged under [`AUDIT`], which Server › Logs shows; an admin
//! tool's call is kept in the audit list too (`crate::store::audit`).

use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_types::{Item, MediaId, SearchScope};

use jsonrpsee::types::ErrorObjectOwned;
use spoolframe_protocol::playlists::{Kinds, PlaylistEdit, PlaylistSummary, Visibility};

use crate::admin::AdminApi;
use crate::oauth::Scope;
use crate::principal::{Actor, Principal};
use crate::store::users::LocalUserId;

/// The log target every MCP call is logged under: the audit trail Server ›
/// Logs finds by searching "audit".
pub const AUDIT: &str = "spoolframe_server::audit";
/// The most items one page of a list holds.
pub const MAX_PAGE: u32 = 100;
/// A page's size when the agent does not say.
const DEFAULT_PAGE: u32 = 20;

/// How long a client may keep the tool list: a token's scopes never
/// change, and a new grant gets a new token, which lists afresh.
const TOOLS_TTL_MS: u64 = 5 * 60 * 1000;

/// What the library tools read: the catalog clients browse, each user's
/// watch state, and playlists. `None` in admin API tests without them,
/// where those tools say so.
#[derive(Clone)]
pub struct Library {
    pub catalog: Arc<dyn Catalog>,
    pub watch: Arc<crate::watch::Watch>,
    pub playlists: Arc<crate::playlists::Playlists>,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

/// Who a call is from: the access token's user, read at this call, and
/// what the user granted the client.
#[derive(Debug, Clone)]
pub struct McpCaller {
    pub user: LocalUserId,
    /// An admin now, not when they consented.
    pub admin: bool,
    /// The name the client registered.
    pub client_name: String,
    pub grant_id: String,
    pub scopes: Vec<Scope>,
}

impl McpCaller {
    fn principal(&self) -> Principal {
        Principal::LocalUser(self.user)
    }

    fn actor(&self) -> Actor {
        Actor {
            principal: self.principal(),
            admin: self.admin,
        }
    }

    /// Whether the token covers `scope`: granted, and `admin` only while
    /// its user is one.
    pub fn may(&self, scope: Scope) -> bool {
        let granted = self.scopes.contains(&scope)
            || (scope == Scope::PlaylistsRead && self.scopes.contains(&Scope::PlaylistsWrite));
        granted && (scope != Scope::Admin || self.admin)
    }
}

/// One tool: what it needs, and how it is described.
struct Entry {
    scope: Scope,
    tool: Tool,
}

/// The input schema of `T`.
fn schema<T: rmcp::schemars::JsonSchema + 'static>(tool: Tool) -> Tool {
    tool.with_input_schema::<T>()
}

fn tool(name: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, Arc::new(JsonObject::new()))
}

/// A tool that only reads.
fn reads() -> ToolAnnotations {
    ToolAnnotations::new().read_only(true).open_world(false)
}

/// Every tool, in the order they are listed.
static TOOLS: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    vec![
        Entry {
            scope: Scope::LibraryRead,
            tool: schema::<SearchArgs>(tool(
                "search",
                "Search the library: movies, shows, episodes, artists, albums and tracks whose \
                 title matches. A year at the end of the query narrows it to that year. Each \
                 result has an id for get_item and the playlist tools.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::LibraryRead,
            tool: schema::<GetItemArgs>(tool(
                "get_item",
                "One item by its id: its details, the user's progress and whether they watched \
                 it, its children (a show's seasons, a season's episodes, an album's tracks), \
                 and the user's playlists that hold it.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::LibraryRead,
            tool: schema::<PageArgs>(tool(
                "list_recent",
                "What was added to the library most recently, newest first.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::LibraryRead,
            tool: schema::<NoArgs>(tool(
                "continue_watching",
                "What the user started and has not finished, most recent first, and the next \
                 episode of each show they are watching: what \"what haven't I finished?\" \
                 asks. Each says when it was last viewed (lastViewedAt, Unix seconds).",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::LibraryRead,
            tool: schema::<PageArgs>(tool(
                "watch_history",
                "What the user watched or started, most recent first, a page at a time, with \
                 their progress, whether each is watched, and when it was last viewed \
                 (lastViewedAt, Unix seconds).",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::PlaylistsRead,
            tool: schema::<NoArgs>(tool(
                "list_playlists",
                "The playlists the user sees: their own, those shared with them and the \
                 server-wide ones, with whether they may change each and where it is pinned. \
                 First, while there is music, the built-in Liked (the tracks the user liked; \
                 changed by like and unlike) and All (every track; read-only).",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::PlaylistsRead,
            tool: schema::<GetPlaylistArgs>(tool(
                "get_playlist",
                "A playlist's entries, a page at a time, in order. Each entry has an entry \
                 number for remove_from_playlist and move_in_playlist, and its item.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<CreatePlaylistArgs>(tool(
                "create_playlist",
                "Make a playlist of the user's own, empty: its id, for add_to_playlist.",
            ))
            .annotate(writes()),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<AddArgs>(tool(
                "add_to_playlist",
                "Add items to a playlist, in the order given, at its end or before an entry. \
                 A movie, an episode or a track adds itself; a season adds its episodes, a \
                 show all of them in airing order, an album its tracks, an artist each album's.",
            ))
            .annotate(writes()),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<RemoveArgs>(tool(
                "remove_from_playlist",
                "Take entries out of a playlist, by their entry numbers from get_playlist.",
            ))
            .annotate(
                ToolAnnotations::new()
                    .read_only(false)
                    .destructive(true)
                    .open_world(false),
            ),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<MoveArgs>(tool(
                "move_in_playlist",
                "Move an entry of a playlist before another, or to its end.",
            ))
            .annotate(writes()),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<PlaylistArgs>(tool(
                "pin_playlist",
                "Pin a playlist to the end of the user's Pinned, in their apps' sidebar.",
            ))
            .annotate(writes().idempotent(true)),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<PlaylistArgs>(tool(
                "unpin_playlist",
                "Take a playlist out of the user's Pinned.",
            ))
            .annotate(writes().idempotent(true)),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<LikeArgs>(tool(
                "like",
                "Like tracks or albums: their tracks go to the top of the user's Liked \
                 playlist, an album's in order; one liked already stays where it is. Only \
                 tracks and albums are liked.",
            ))
            .annotate(writes().idempotent(true)),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<LikeArgs>(tool(
                "unlike",
                "Take tracks, or every track of albums, out of the user's Liked playlist.",
            ))
            // As remove_from_playlist: liked again, a track goes to the
            // top, not back where it was.
            .annotate(
                ToolAnnotations::new()
                    .read_only(false)
                    .destructive(true)
                    .idempotent(true)
                    .open_world(false),
            ),
        },
        Entry {
            scope: Scope::PlaylistsWrite,
            tool: schema::<SetWatchedArgs>(tool(
                "set_watched",
                "Mark a movie or an episode watched or unwatched for the user; a season or a \
                 show marks every episode of it. Either way its progress starts over.",
            ))
            .annotate(writes().idempotent(true)),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<NoArgs>(tool(
                "list_users",
                "Every user of the server: role, whether disabled, sign-in methods, devices \
                 and sessions counted, when last seen.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<UsernameArgs>(tool(
                "disable_user",
                "Disable a user: they cannot sign in, and every session of theirs ends now. \
                 An admin enables them again in the web UI.",
            ))
            .annotate(destroys()),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<UsernameArgs>(tool(
                "list_sessions",
                "A user's live sign-in sessions on every device and the web UI, most recently \
                 used first.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<EndSessionArgs>(tool(
                "end_session",
                "End one of a user's sign-in sessions, by its id from list_sessions: that \
                 device or browser must sign in again.",
            ))
            .annotate(destroys()),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<NoArgs>(tool(
                "list_playbacks",
                "What plays on the server now, and what ended in the last hour: who, what, on \
                 which device, and how it is converted.",
            ))
            .annotate(reads()),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<PlaybackArgs>(tool(
                "stop_playback",
                "Stop a playback now, by its id from list_playbacks. Its app is told it was \
                 stopped; its progress is kept.",
            ))
            .annotate(destroys()),
        },
        Entry {
            scope: Scope::Admin,
            tool: schema::<LogsArgs>(tool(
                "read_logs",
                "The server's last log lines, oldest first, optionally only those at least \
                 as severe as a level or containing some text.",
            ))
            .annotate(reads()),
        },
    ]
});

/// A tool that adds or changes, and takes nothing away.
fn writes() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(false)
        .open_world(false)
}

/// A tool whose change cannot be taken back by calling it again: the
/// client asks its user first.
fn destroys() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(true)
        .open_world(false)
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct NoArgs {}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SearchArgs {
    /// What to search for.
    query: String,
    /// The most results of each kind; 10 if left out, at most 100.
    limit: Option<u32>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct GetItemArgs {
    /// The item's id, as search and the other tools give it
    /// (e.g. "plex:episode:208620").
    id: String,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct PageArgs {
    /// Where the page starts; 0 if left out.
    offset: Option<u32>,
    /// How many; 20 if left out, at most 100.
    limit: Option<u32>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct PlaylistArgs {
    /// The playlist's id, from list_playlists or create_playlist.
    playlist: String,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct GetPlaylistArgs {
    /// The playlist's id.
    playlist: String,
    /// Where the page starts; 0 if left out.
    offset: Option<u32>,
    /// How many entries; 20 if left out, at most 100.
    limit: Option<u32>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct CreatePlaylistArgs {
    /// Its name, up to 100 characters.
    name: String,
    /// What it may hold: any of "movies", "episodes" and "tracks". All
    /// three if left out.
    kinds: Option<Vec<String>>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct AddArgs {
    /// The playlist's id.
    playlist: String,
    /// The items' ids, in the order to add them; at most 100.
    items: Vec<String>,
    /// The entry to add them before; at the end if left out.
    before: Option<u64>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RemoveArgs {
    /// The playlist's id.
    playlist: String,
    /// The entries' numbers, from get_playlist; at most 100.
    entries: Vec<u64>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct MoveArgs {
    /// The playlist's id.
    playlist: String,
    /// The entry to move.
    entry: u64,
    /// The entry to put it before; at the end if left out.
    before: Option<u64>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct LikeArgs {
    /// The tracks' or albums' ids; at most 100.
    items: Vec<String>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SetWatchedArgs {
    /// The item's id: a movie, an episode, a season or a show.
    id: String,
    /// Watched, or not.
    watched: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct UsernameArgs {
    /// The user's username.
    username: String,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct EndSessionArgs {
    /// The user's username.
    username: String,
    /// The session's id, from list_sessions.
    session: String,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct PlaybackArgs {
    /// The playback's id, from list_playbacks.
    playback: String,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct LogsArgs {
    /// Only lines whose text or source contains this, in any case.
    text: Option<String>,
    /// Only lines at least this severe: "error", "warn", "info", "debug"
    /// or "trace".
    level: Option<String>,
    /// At most this many of the last lines; 100 if left out, at most 500.
    limit: Option<u32>,
}

/// How many times an edit is made again on a playlist someone else
/// changed meanwhile, before the agent is told.
const EDIT_TRIES: usize = 3;
/// The most entries one call adds or removes.
const MAX_EDITS: usize = crate::playlists::MAX_EDITS;

impl PageArgs {
    fn page(&self) -> (u32, u32) {
        (
            self.offset.unwrap_or(0),
            self.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE),
        )
    }
}

/// Why a tool could not do what it was asked: told to the agent as the
/// tool's result, so it can try otherwise.
#[derive(Debug)]
struct ToolError(String);

impl ToolError {
    fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

impl From<ErrorObjectOwned> for ToolError {
    /// An admin API refusal: its own words.
    fn from(e: ErrorObjectOwned) -> Self {
        ToolError(e.message().to_owned())
    }
}

impl From<CatalogError> for ToolError {
    fn from(e: CatalogError) -> Self {
        match e {
            CatalogError::NotFound(id) | CatalogError::NotServed(id) => {
                ToolError(format!("{id} is not in the library"))
            }
            e => {
                tracing::warn!(error = %e, "an MCP tool could not read the catalog");
                ToolError("the library could not be read now; try again later".into())
            }
        }
    }
}

impl From<crate::watch::WatchError> for ToolError {
    fn from(e: crate::watch::WatchError) -> Self {
        match e {
            crate::watch::WatchError::Catalog(e) => e.into(),
            crate::watch::WatchError::NotTracked(id) => {
                ToolError(format!("{id} has no watched state"))
            }
            crate::watch::WatchError::Store(e) => {
                tracing::error!(error = %e, "an MCP tool");
                ToolError("internal error".into())
            }
        }
    }
}

impl From<crate::playlists::PlaylistsError> for ToolError {
    fn from(e: crate::playlists::PlaylistsError) -> Self {
        use crate::playlists::PlaylistsError as E;
        match e {
            E::Catalog(e) => e.into(),
            E::Store(e) => {
                tracing::error!(error = %e, "an MCP tool");
                ToolError("internal error".into())
            }
            e => ToolError(e.to_string()),
        }
    }
}

type ToolResult = Result<Value, ToolError>;

/// `arguments` as a tool's `T`.
fn args<T: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<T, ToolError> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|e| ToolError(format!("arguments: {e}")))
}

/// `text` as an item id.
fn media_id(text: &str) -> Result<MediaId, ToolError> {
    text.parse()
        .map_err(|_| ToolError(format!("{text:?} is not an item id")))
}

/// Seconds, from a duration.
fn secs(d: std::time::Duration) -> u64 {
    d.as_secs()
}

/// An item as the tools give it: its id, kind and names, the numbers that
/// place it, and the user's state; no artwork, no paths.
pub fn item_json(item: &Item) -> Value {
    let state = |p: &spoolframe_types::PlaybackState| {
        let mut state = json!({
            "watched": p.watched,
            "positionSecs": p.position.map(secs),
        });
        // Where a tool filled it: when it was last viewed, Unix seconds.
        if let Some(at) = p.last_viewed_at {
            state["lastViewedAt"] = json!(at);
        }
        state
    };
    match item {
        Item::Movie(m) => json!({
            "id": m.id.to_string(), "kind": "movie", "title": m.title, "year": m.year,
            "durationSecs": m.duration.map(secs), "state": state(&m.playback),
        }),
        Item::Show(s) => json!({
            "id": s.id.to_string(), "kind": "show", "title": s.title, "year": s.year,
            "seasons": s.season_count,
        }),
        Item::Season(s) => json!({
            "id": s.id.to_string(), "kind": "season", "title": s.title,
            "show": s.show_title, "showId": s.show_id.to_string(), "season": s.season,
        }),
        Item::Episode(e) => json!({
            "id": e.id.to_string(), "kind": "episode", "title": e.title,
            "show": e.show_title, "showId": e.show_id.to_string(),
            "season": e.season, "episode": e.episode, "year": e.year,
            "durationSecs": e.duration.map(secs), "state": state(&e.playback),
        }),
        Item::Artist(a) => json!({ "id": a.id.to_string(), "kind": "artist", "title": a.name }),
        Item::Album(a) => json!({
            "id": a.id.to_string(), "kind": "album", "title": a.title,
            "artist": a.artist_name, "year": a.year,
        }),
        Item::Track(t) => json!({
            "id": t.id.to_string(), "kind": "track", "title": t.title,
            "artist": t.artist_name, "album": t.album_title, "albumId": t.album_id.to_string(),
            "track": t.track, "durationSecs": t.duration.map(secs), "state": state(&t.playback),
        }),
    }
}

/// [`item_json`], with what an item's own page says besides: its summary,
/// rating, genres and the first of its cast.
fn item_detail(item: &Item) -> Value {
    let mut value = item_json(item);
    let (summary, rating, cast, genres) = match item {
        Item::Movie(m) => (&m.summary, &m.content_rating, &m.cast[..], &[][..]),
        Item::Show(s) => (&s.summary, &s.content_rating, &s.cast[..], &s.genres[..]),
        Item::Episode(e) => (&e.summary, &e.content_rating, &e.cast[..], &[][..]),
        Item::Artist(a) => (&a.summary, &None, &[][..], &[][..]),
        _ => (&None, &None, &[][..], &[][..]),
    };
    let more = json!({
        "summary": summary,
        "contentRating": rating,
        "genres": genres,
        "cast": cast.iter().take(10).map(|c| json!({ "name": c.name, "character": c.character })).collect::<Vec<_>>(),
    });
    if let (Value::Object(value), Value::Object(more)) = (&mut value, more) {
        value.extend(more);
    }
    value
}

fn items_json(items: &[Item]) -> Vec<Value> {
    items.iter().map(item_json).collect()
}

/// The MCP server one request is served by.
#[derive(Clone)]
pub struct McpServer {
    api: AdminApi,
}

impl McpServer {
    pub(crate) fn new(api: AdminApi) -> Self {
        Self { api }
    }

    fn library(&self) -> Result<&Library, ToolError> {
        self.api
            .mcp_library()
            .ok_or_else(|| ToolError::new("this server has no library to read"))
    }

    async fn run(
        &self,
        caller: &McpCaller,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> ToolResult {
        match name {
            "search" => self.search(caller, args(arguments)?).await,
            "get_item" => self.get_item(caller, args(arguments)?).await,
            "list_recent" => self.list_recent(caller, args(arguments)?).await,
            "continue_watching" => {
                let NoArgs {} = args(arguments)?;
                self.continue_watching(caller).await
            }
            "watch_history" => self.watch_history(caller, args(arguments)?).await,
            "list_playlists" => {
                let NoArgs {} = args(arguments)?;
                self.list_playlists(caller).await
            }
            "get_playlist" => self.get_playlist(caller, args(arguments)?).await,
            "create_playlist" => self.create_playlist(caller, args(arguments)?).await,
            "add_to_playlist" => self.add_to_playlist(caller, args(arguments)?).await,
            "remove_from_playlist" => self.remove_from_playlist(caller, args(arguments)?).await,
            "move_in_playlist" => self.move_in_playlist(caller, args(arguments)?).await,
            "pin_playlist" => {
                let a: PlaylistArgs = args(arguments)?;
                let library = self.library()?;
                library
                    .playlists
                    .pin(&caller.actor(), &a.playlist, u32::MAX)
                    .await?;
                Ok(json!({ "pinned": true }))
            }
            "unpin_playlist" => {
                let a: PlaylistArgs = args(arguments)?;
                let library = self.library()?;
                library
                    .playlists
                    .unpin(&caller.actor(), &a.playlist)
                    .await?;
                Ok(json!({ "pinned": false }))
            }
            "like" | "unlike" => {
                let a: LikeArgs = args(arguments)?;
                let items = a
                    .items
                    .iter()
                    .map(|id| media_id(id))
                    .collect::<Result<Vec<_>, _>>()?;
                let library = self.library()?;
                let liked = if name == "like" {
                    library.playlists.like(&caller.actor(), &items).await?
                } else {
                    library.playlists.unlike(&caller.actor(), &items).await?
                };
                Ok(playlist_json(&liked))
            }
            "set_watched" => {
                let a: SetWatchedArgs = args(arguments)?;
                let library = self.library()?;
                library
                    .watch
                    .mark(caller.principal(), &media_id(&a.id)?, a.watched)
                    .await?;
                Ok(json!({ "id": a.id, "watched": a.watched }))
            }
            "list_users" => {
                let NoArgs {} = args(arguments)?;
                Ok(json!({ "users": self.api.users().await? }))
            }
            "disable_user" => {
                let a: UsernameArgs = args(arguments)?;
                let ended = self.api.disable(&a.username).await?;
                Ok(json!({ "disabled": a.username, "sessionsEnded": ended }))
            }
            "list_sessions" => {
                let a: UsernameArgs = args(arguments)?;
                Ok(json!({ "sessions": self.api.sessions_of(&a.username, "").await? }))
            }
            "end_session" => {
                let a: EndSessionArgs = args(arguments)?;
                self.api.end_session_of(&a.username, &a.session).await?;
                Ok(json!({ "ended": a.session }))
            }
            "list_playbacks" => {
                let NoArgs {} = args(arguments)?;
                serde_json::to_value(self.api.playback_list()?)
                    .map_err(|e| ToolError(e.to_string()))
            }
            "stop_playback" => {
                let a: PlaybackArgs = args(arguments)?;
                self.api.stop_playback(&a.playback)?;
                Ok(json!({ "stopped": a.playback }))
            }
            "read_logs" => self.read_logs(args(arguments)?),
            other => Err(ToolError(format!("no tool {other}"))),
        }
    }

    async fn search(&self, caller: &McpCaller, a: SearchArgs) -> ToolResult {
        let library = self.library()?;
        let request = spoolframe_protocol::SearchRequest {
            query: a.query,
            limit: a.limit.unwrap_or(10).clamp(1, MAX_PAGE),
            scope: SearchScope::All,
            kind: None,
            library: None,
        };
        let mut found = crate::search::search(
            library.catalog.as_ref(),
            &request,
            crate::search::this_year(),
        )
        .await?;
        let mut items: Vec<&mut Item> = Vec::new();
        if let Some(spoolframe_protocol::TopResult::Item(item)) = &mut found.top {
            items.push(item);
        }
        for group in &mut found.groups {
            items.extend(group.items.iter_mut());
        }
        library.watch.fill(caller.principal(), &mut items).await?;
        let top = match &found.top {
            Some(spoolframe_protocol::TopResult::Item(item)) => item_json(item),
            Some(spoolframe_protocol::TopResult::Person(p)) => json!({ "person": p.name }),
            None => Value::Null,
        };
        let groups: Vec<Value> = found
            .groups
            .iter()
            .map(|g| {
                json!({
                    "kind": g.kind,
                    "items": items_json(&g.items),
                    "people": g.people.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
                    "more": g.more,
                })
            })
            .collect();
        Ok(json!({ "top": top, "groups": groups, "year": found.year }))
    }

    async fn get_item(&self, caller: &McpCaller, a: GetItemArgs) -> ToolResult {
        let library = self.library()?;
        let id = media_id(&a.id)?;
        let mut found = library.catalog.item(&id).await?;
        // As a client's item screen does: state kept under an earlier key
        // comes over.
        library
            .watch
            .relink(caller.principal(), &id, &found.external_ids)
            .await?;
        let mut items: Vec<&mut Item> = std::iter::once(&mut found.item)
            .chain(found.children.iter_mut())
            .collect();
        library.watch.fill(caller.principal(), &mut items).await?;
        let playlists = library
            .playlists
            .holding(&caller.actor(), &found.item, &found.children, true)
            .await?;
        let (liked, liked_children) = library
            .playlists
            .liked(&caller.actor(), &found.item, &found.children)
            .await?;
        Ok(json!({
            "item": item_detail(&found.item),
            "children": items_json(&found.children),
            "playlists": playlists.iter().map(|p| json!({ "id": p.id, "name": p.name })).collect::<Vec<_>>(),
            "liked": liked,
            "likedTracks": liked_children,
        }))
    }

    async fn list_recent(&self, caller: &McpCaller, a: PageArgs) -> ToolResult {
        let library = self.library()?;
        let (_, limit) = a.page();
        let mut items = library.catalog.recently_added(limit).await?;
        let mut all: Vec<&mut Item> = items.iter_mut().collect();
        library.watch.fill(caller.principal(), &mut all).await?;
        Ok(json!({ "items": items_json(&items) }))
    }

    async fn continue_watching(&self, caller: &McpCaller) -> ToolResult {
        let library = self.library()?;
        let mut home = library.watch.home(caller.principal()).await?;
        let mut rows: Vec<&mut Item> = home
            .continue_watching
            .iter_mut()
            .chain(home.next_up.iter_mut())
            .collect();
        library
            .watch
            .fill_last_viewed(caller.principal(), &mut rows)
            .await?;
        Ok(json!({
            "continueWatching": items_json(&home.continue_watching),
            "nextUp": items_json(&home.next_up),
        }))
    }

    async fn watch_history(&self, caller: &McpCaller, a: PageArgs) -> ToolResult {
        let library = self.library()?;
        let (offset, limit) = a.page();
        let (mut items, total) = library
            .watch
            .history(caller.principal(), offset, limit)
            .await?;
        let mut rows: Vec<&mut Item> = items.iter_mut().collect();
        library
            .watch
            .fill_last_viewed(caller.principal(), &mut rows)
            .await?;
        Ok(json!({ "items": items_json(&items), "offset": offset, "total": total }))
    }
}

impl McpServer {
    async fn list_playlists(&self, caller: &McpCaller) -> ToolResult {
        let library = self.library()?;
        let (playlists, _) = library.playlists.list(&caller.actor(), true).await?;
        Ok(json!({ "playlists": playlists.iter().map(playlist_json).collect::<Vec<_>>() }))
    }

    async fn get_playlist(&self, caller: &McpCaller, a: GetPlaylistArgs) -> ToolResult {
        let library = self.library()?;
        let offset = a.offset.unwrap_or(0);
        let limit = a.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
        let page = library
            .playlists
            .get(&caller.actor(), &a.playlist, offset, limit)
            .await?;
        Ok(json!({
            "playlist": playlist_json(&page.playlist),
            "entries": page.entries.iter().map(|e| json!({
                "entry": e.entry,
                "item": item_json(&e.item),
                "unavailable": e.unavailable,
                "liked": e.liked,
            })).collect::<Vec<_>>(),
            "offset": page.offset,
            "total": page.total,
        }))
    }

    async fn create_playlist(&self, caller: &McpCaller, a: CreatePlaylistArgs) -> ToolResult {
        let library = self.library()?;
        let kinds = match a.kinds {
            None => Kinds {
                movies: true,
                episodes: true,
                tracks: true,
            },
            Some(names) => {
                let mut kinds = Kinds::default();
                for name in names {
                    match name.as_str() {
                        "movies" => kinds.movies = true,
                        "episodes" => kinds.episodes = true,
                        "tracks" => kinds.tracks = true,
                        other => {
                            return Err(ToolError(format!(
                                "{other:?} is not a kind: movies, episodes or tracks"
                            )));
                        }
                    }
                }
                kinds
            }
        };
        let made = library
            .playlists
            .create(&caller.actor(), &a.name, kinds, Visibility::Personal)
            .await?;
        Ok(playlist_json(&made))
    }

    /// Make `edits` on playlist `id` at its revision now, again on a newer
    /// one if someone else's change overtook it: the playlist's revision
    /// after.
    async fn edit(&self, caller: &McpCaller, id: &str, edits: Vec<PlaylistEdit>) -> ToolResult {
        let library = self.library()?;
        if edits.is_empty() || edits.len() > MAX_EDITS {
            return Err(ToolError(format!("between 1 and {MAX_EDITS} at a time")));
        }
        let actor = caller.actor();
        for _ in 0..EDIT_TRIES {
            let revision = library
                .playlists
                .get(&actor, id, 0, 1)
                .await?
                .playlist
                .revision;
            match library
                .playlists
                .edit(&actor, id, revision, edits.clone())
                .await
            {
                Err(crate::playlists::PlaylistsError::Conflict { .. }) => continue,
                done => return Ok(json!({ "revision": done? })),
            }
        }
        Err(ToolError::new(
            "the playlist kept changing under this edit; look at it again and retry",
        ))
    }

    async fn add_to_playlist(&self, caller: &McpCaller, a: AddArgs) -> ToolResult {
        let edits = a
            .items
            .iter()
            .map(|id| {
                Ok(PlaylistEdit::Add {
                    id: media_id(id)?,
                    before: a.before,
                })
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        self.edit(caller, &a.playlist, edits).await
    }

    async fn remove_from_playlist(&self, caller: &McpCaller, a: RemoveArgs) -> ToolResult {
        let edits = a
            .entries
            .iter()
            .map(|&entry| PlaylistEdit::Remove { entry })
            .collect();
        self.edit(caller, &a.playlist, edits).await
    }

    async fn move_in_playlist(&self, caller: &McpCaller, a: MoveArgs) -> ToolResult {
        let edits = vec![PlaylistEdit::Move {
            entry: a.entry,
            before: a.before,
        }];
        self.edit(caller, &a.playlist, edits).await
    }

    fn read_logs(&self, a: LogsArgs) -> ToolResult {
        let level = a
            .level
            .map(|l| {
                serde_json::from_value(Value::String(l.to_lowercase())).map_err(|_| {
                    ToolError(format!(
                        "{l:?} is not a level: error, warn, info, debug or trace"
                    ))
                })
            })
            .transpose()?;
        let query = spoolframe_admin_api::LogQuery {
            after: None,
            level,
            text: a.text,
            limit: Some(a.limit.unwrap_or(100).clamp(1, 500)),
        };
        serde_json::to_value(self.api.log_page(&query)?).map_err(|e| ToolError(e.to_string()))
    }

    /// Record an admin tool's call in the audit list: who, through which
    /// client, what, to whom, and how it went. A failure to record is
    /// logged; the call's own log line stands.
    async fn audit(&self, caller: &McpCaller, tool: &str, target: Option<&str>, outcome: &str) {
        let user = caller.user.to_string();
        let event = crate::store::audit::NewAuditEvent {
            actor: &user,
            client: &caller.client_name,
            action: tool,
            target,
            outcome,
        };
        if let Err(e) = self.api.oauth().store().record_audit(event).await {
            tracing::error!(error = %e, tool, "cannot record an admin tool call in the audit list");
        }
    }
}

/// A playlist as the tools give it.
fn playlist_json(p: &PlaylistSummary) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "kinds": p.kinds,
        "owner": p.owner_kind,
        "canEdit": p.can_edit,
        "canManage": p.can_manage,
        "pinned": p.pinned,
        "builtin": p.builtin,
    })
}

/// What an admin tool acts on, from its arguments, for the audit list.
fn audit_target(arguments: Option<&JsonObject>) -> Option<String> {
    let arguments = arguments?;
    ["username", "playback"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

/// The caller the bearer check put on the request.
fn caller(context: &RequestContext<RoleServer>) -> Result<McpCaller, ErrorData> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<McpCaller>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("a call without its caller", None))
}

impl ServerHandler for McpServer {
    /// The server's name and version, and in its description the client
    /// protocol's version and features, one per line, as the Server page
    /// lists them: for an agent asked what this server speaks.
    fn get_info(&self) -> ServerConfig {
        let protocol = self
            .api
            .protocol()
            .lines("experimental features on")
            .join("\n");
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("spoolframe", env!("CARGO_PKG_VERSION"))
                    .with_description(protocol),
            )
            .with_instructions(
                "Spoolframe is a media server. These tools act as the signed-in user, with \
                 what they allowed: their library, what they watch, their playlists, and for \
                 an admin, the server. Ids come from search and the other tools.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let caller = caller(&context)?;
        let tools = TOOLS
            .iter()
            .filter(|e| caller.may(e.scope))
            .map(|e| e.tool.clone())
            .collect();
        // MCP 2026-07-28 (SEP-2549) requires both on a list result, and
        // Claude Code refuses a tools/list without them; an older client
        // ignores them. Private: the list is the token's scopes'.
        Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(TOOLS_TTL_MS)
            .with_cache_scope(CacheScope::Private))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        TOOLS
            .iter()
            .find(|e| e.tool.name == name)
            .map(|e| e.tool.clone())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = caller(&context)?;
        let name: Cow<'static, str> = request.name.clone();
        let Some(entry) = TOOLS.iter().find(|e| e.tool.name == name) else {
            return Err(ErrorData::invalid_params(format!("no tool {name}"), None));
        };
        if !caller.may(entry.scope) {
            tracing::info!(
                target: AUDIT,
                user = %caller.user,
                client = %caller.client_name,
                tool = %name,
                outcome = "refused",
                "MCP call refused: the scope {} was not granted",
                entry.scope.as_str()
            );
            return Err(ErrorData::invalid_params(
                format!(
                    "{name} needs the {} scope, which this connection was not granted",
                    entry.scope.as_str()
                ),
                None,
            ));
        }
        // Counted whether it works or not: a call was made.
        if let Err(e) = self
            .api
            .oauth()
            .store()
            .note_oauth_call(&caller.grant_id)
            .await
        {
            tracing::warn!(error = %e, "cannot count an MCP call");
        }
        let target = audit_target(request.arguments.as_ref());
        let answer = self.run(&caller, &name, request.arguments).await;
        let outcome = if answer.is_ok() { "ok" } else { "failed" };
        if entry.scope == Scope::Admin {
            let recorded = match &answer {
                Ok(_) => "ok".to_owned(),
                Err(ToolError(why)) => format!("failed: {why}"),
            };
            self.audit(&caller, &name, target.as_deref(), &recorded)
                .await;
        }
        tracing::info!(
            target: AUDIT,
            user = %caller.user,
            client = %caller.client_name,
            tool = %name,
            outcome,
            "MCP call"
        );
        Ok(match answer {
            Ok(value) => CallToolResult::structured(value),
            Err(ToolError(why)) => CallToolResult::error(vec![ContentBlock::text(why)]),
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(scopes: &[Scope], admin: bool) -> McpCaller {
        McpCaller {
            user: LocalUserId::from_uuid(uuid::Uuid::from_u128(1)),
            admin,
            client_name: "Claude".into(),
            grant_id: "g".into(),
            scopes: scopes.to_vec(),
        }
    }

    #[test]
    fn admin_is_covered_only_while_its_user_is_an_admin() {
        assert!(caller(&[Scope::Admin], true).may(Scope::Admin));
        assert!(!caller(&[Scope::Admin], false).may(Scope::Admin));
        assert!(!caller(&[Scope::LibraryRead], true).may(Scope::Admin));
        assert!(!caller(&[Scope::LibraryRead], false).may(Scope::PlaylistsWrite));
    }

    #[test]
    fn playlists_write_covers_reading_them_and_read_covers_no_write() {
        assert!(caller(&[Scope::PlaylistsWrite], false).may(Scope::PlaylistsRead));
        assert!(caller(&[Scope::PlaylistsRead], false).may(Scope::PlaylistsRead));
        assert!(!caller(&[Scope::PlaylistsRead], false).may(Scope::PlaylistsWrite));
        assert!(!caller(&[Scope::LibraryRead], false).may(Scope::PlaylistsRead));
    }

    #[test]
    fn every_tool_has_a_unique_name_and_an_object_schema() {
        let mut names: Vec<&str> = TOOLS.iter().map(|e| e.tool.name.as_ref()).collect();
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count);
        for entry in TOOLS.iter() {
            assert_eq!(
                entry.tool.input_schema.get("type"),
                Some(&json!("object")),
                "{}",
                entry.tool.name
            );
        }
    }

    #[test]
    fn what_cannot_be_undone_is_marked_destructive_and_reads_are_read_only() {
        let annotations = |name: &str| {
            TOOLS
                .iter()
                .find(|e| e.tool.name == name)
                .and_then(|e| e.tool.annotations.clone())
                .unwrap()
        };
        for name in [
            "disable_user",
            "end_session",
            "stop_playback",
            "remove_from_playlist",
            "unlike",
        ] {
            assert_eq!(annotations(name).destructive_hint, Some(true), "{name}");
        }
        for name in ["create_playlist", "add_to_playlist", "set_watched", "like"] {
            assert_eq!(annotations(name).destructive_hint, Some(false), "{name}");
        }
        for name in ["search", "list_users", "read_logs", "list_playlists"] {
            assert_eq!(annotations(name).read_only_hint, Some(true), "{name}");
        }
    }

    #[test]
    fn an_item_is_given_without_artwork_or_paths() {
        let item = crate::test_support::movie();
        let value = item_json(&item);
        assert_eq!(value["id"], "plex:movie:1");
        assert_eq!(value["kind"], "movie");
        assert_eq!(value["title"], "Alien");
        assert!(value.get("artwork").is_none());
        let detail = item_detail(&item);
        assert_eq!(detail["cast"][0]["name"], "Sigourney Weaver");
    }
}

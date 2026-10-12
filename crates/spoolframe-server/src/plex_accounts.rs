//! Sign in with Plex per user (P3-12): each user links their own Plex
//! account in My profile, with plex.tv's PIN flow, and their watch state
//! then syncs with that account.
//!
//! The account's token is kept as the user's secret
//! (`secrets/plex-account-<user id>`, with who plex.tv said it is), never
//! in a reply, a log line or a config layer. It reaches the whole Plex
//! account, so it is used only to read and write that user's watch state
//! and to list the account's servers. Unlinking deletes it.
//!
//! PX-04's single owner setting, `plex.watch_sync.user`, is the owner's
//! link made by the config: that user syncs with each Plex server's own
//! token, or none on a LAN PMS that lets clients in without one, exactly
//! as PX-04 did. A link made in My profile takes its place while it
//! exists.

use parking_lot::Mutex;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use spoolframe_admin_api::{PlexAccountServer, PlexLink};
use spoolframe_plex::account::{AccountError, Pin, PinState, PlexConnection, PlexResource, PlexTv};
use url::Url;

use crate::config::ServerConfig;
use crate::config::secrets::Secrets;
use crate::store::Store;
use crate::store::users::{LocalUserId, UserError, UserRecord};

/// A link made in My profile: the account's token and who it is.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLink {
    pub token: String,
    pub username: Option<String>,
    pub email: Option<String>,
    /// Unix seconds.
    pub linked_at: i64,
}

impl std::fmt::Debug for AccountLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountLink")
            .field("token", &"<redacted>")
            .field("username", &self.username)
            .field("email", &self.email)
            .field("linked_at", &self.linked_at)
            .finish()
    }
}

/// How a user's state reaches Plex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Their own Plex account, linked in My profile.
    Account(AccountLink),
    /// The config's owner (`plex.watch_sync.user`): each Plex server's own
    /// token.
    Owner,
}

impl Link {
    /// As the API shows it: never the token.
    pub fn shown(&self) -> PlexLink {
        match self {
            Link::Account(a) => PlexLink {
                username: a.username.clone(),
                email: a.email.clone(),
                by_config: false,
                linked_at: Some(a.linked_at),
            },
            Link::Owner => PlexLink {
                username: None,
                email: None,
                by_config: true,
                linked_at: None,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("not a PIN you began")]
    NotYourPin,
    /// [`MAX_LINK_BEGINS`] begun within the window.
    #[error("too many Plex sign-ins begun; wait a few minutes")]
    TooMany { retry_after: std::time::Duration },
    /// Unlinking what the config file links.
    #[error(
        "the server's config file links this account (plex.watch_sync.user); only that file \
         can unlink it"
    )]
    ByConfig,
    #[error(transparent)]
    PlexTv(#[from] AccountError),
    #[error("cannot keep the Plex link: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Users(#[from] UserError),
}

/// Where linking ends up after a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linking {
    Waiting,
    Linked(Link),
    Expired,
}

/// Where plex.tv and its sign-in page are: the real ones, or stand-ins.
#[derive(Debug, Clone)]
pub struct PlexSite {
    pub api: Url,
    pub app: Url,
}

/// Every user's Plex link.
#[derive(Debug)]
pub struct PlexAccounts {
    secrets: Secrets,
    store: Store,
    site: PlexSite,
    /// `plex.watch_sync.user`, as the server started (a restart setting).
    owner: Option<LocalUserId>,
    /// The PINs begun, whose each is, and when.
    pins: Mutex<HashMap<u64, (LocalUserId, std::time::Instant)>>,
    /// When each user began their last links, for [`MAX_LINK_BEGINS`].
    begun: Mutex<HashMap<LocalUserId, std::collections::VecDeque<std::time::Instant>>>,
    /// The user whose link was made or removed, for the sync to start or
    /// stop their runs.
    changed: tokio::sync::broadcast::Sender<LocalUserId>,
}

/// The Plex links a user may begin in [`LINK_BEGIN_WINDOW`] (PX-09
/// finding 15): each asks plex.tv for a PIN, which the server keeps. A
/// person links once; a few is a page reloaded, or a PIN let expire.
pub const MAX_LINK_BEGINS: usize = 5;

/// How long a begun link counts against [`MAX_LINK_BEGINS`]: plex.tv's
/// PINs last 15 minutes.
const LINK_BEGIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(15 * 60);

pub(crate) fn secret_name(user: impl std::fmt::Display) -> String {
    format!("plex-account-{}", user)
}

impl PlexAccounts {
    pub fn new(secrets: Secrets, store: Store, site: PlexSite, owner: Option<LocalUserId>) -> Self {
        Self {
            secrets,
            store,
            site,
            owner,
            pins: Mutex::default(),
            begun: Mutex::default(),
            changed: tokio::sync::broadcast::Sender::new(16),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The owner the config links, if any.
    pub fn owner(&self) -> Option<LocalUserId> {
        self.owner
    }

    /// Fires with each user whose link was made or removed.
    pub fn changes(&self) -> tokio::sync::broadcast::Receiver<LocalUserId> {
        self.changed.subscribe()
    }

    /// `user`'s link made in My profile, if any.
    pub fn account(&self, user: LocalUserId) -> std::io::Result<Option<AccountLink>> {
        let Some(text) = self.secrets.get(&secret_name(user))? else {
            return Ok(None);
        };
        match serde_json::from_str(&text) {
            Ok(link) => Ok(Some(link)),
            Err(e) => {
                // Never the text: it holds the token.
                tracing::warn!(
                    user = %user,
                    error = ?e.classify(),
                    "an unreadable Plex link; treated as none"
                );
                Ok(None)
            }
        }
    }

    /// How `user`'s state reaches Plex, if it does: their own account, or
    /// the config's owner link.
    pub fn link(&self, user: LocalUserId) -> std::io::Result<Option<Link>> {
        if let Some(account) = self.account(user)? {
            return Ok(Some(Link::Account(account)));
        }
        Ok((self.owner == Some(user)).then_some(Link::Owner))
    }

    /// Every user, with their link.
    pub async fn users(&self) -> Result<Vec<(UserRecord, Option<Link>)>, LinkError> {
        let users = self.store.list_users().await?;
        users
            .into_iter()
            .map(|u| {
                let link = self.link(u.id)?;
                Ok((u, link))
            })
            .collect()
    }

    fn plex_tv(&self) -> Result<PlexTv, LinkError> {
        let client_id = self.secrets.plex_client_id()?;
        Ok(PlexTv::new(
            self.site.api.clone(),
            self.site.app.clone(),
            client_id,
        )?)
    }

    /// Begin linking `user`'s Plex account: a PIN for them to approve,
    /// and Plex's sign-in page for it, which goes on to `forward` when done.
    /// At most [`MAX_LINK_BEGINS`] a user in [`LINK_BEGIN_WINDOW`], so
    /// neither plex.tv nor the PINs kept here are asked for without end
    /// (PX-09 finding 15); the user keeps only that many PINs.
    pub async fn begin(&self, user: LocalUserId, forward: Option<&str>) -> Result<Pin, LinkError> {
        let plex_tv = self.plex_tv()?;
        {
            // The store's clock, so a test can move past the window.
            let now = self.store.clock().now();
            let mut begun = self.begun.lock();
            begun.retain(|_, times| {
                times.retain(|at| now.duration_since(*at) < LINK_BEGIN_WINDOW);
                !times.is_empty()
            });
            let times = begun.entry(user).or_default();
            if times.len() >= MAX_LINK_BEGINS {
                let oldest = times.front().copied().unwrap_or(now);
                return Err(LinkError::TooMany {
                    retry_after: LINK_BEGIN_WINDOW.saturating_sub(now.duration_since(oldest)),
                });
            }
            times.push_back(now);
        }
        let pin = plex_tv.begin(forward).await?;
        let mut pins = self.pins.lock();
        let mine: Vec<(u64, std::time::Instant)> = pins
            .iter()
            .filter(|(_, (owner, _))| *owner == user)
            .map(|(id, (_, at))| (*id, *at))
            .collect();
        if mine.len() >= MAX_LINK_BEGINS
            && let Some((oldest, _)) = mine.iter().min_by_key(|(_, at)| *at)
        {
            pins.remove(oldest);
        }
        pins.insert(pin.id, (user, self.store.clock().now()));
        Ok(pin)
    }

    /// Whether `user`'s PIN `id` is approved; once it is, the link is kept.
    pub async fn check(&self, user: LocalUserId, id: u64) -> Result<Linking, LinkError> {
        let mine = self
            .pins
            .lock()
            .get(&id)
            .is_some_and(|(owner, _)| *owner == user);
        if !mine {
            return Err(LinkError::NotYourPin);
        }
        let plex_tv = self.plex_tv()?;
        let token = match plex_tv.check(id).await? {
            PinState::Waiting => return Ok(Linking::Waiting),
            PinState::Expired => {
                self.forget_pin(id);
                return Ok(Linking::Expired);
            }
            PinState::Linked(token) => token,
        };
        self.forget_pin(id);
        // Only for showing who; the link is made either way.
        let account = plex_tv.user(&token).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot ask plex.tv whose token it is");
            Default::default()
        });
        let link = AccountLink {
            token,
            username: account.username,
            email: account.email,
            linked_at: self.store.now(),
        };
        self.save(user, &link)?;
        tracing::info!(
            user = %user,
            account = link.username.as_deref().or(link.email.as_deref()),
            "Plex account linked"
        );
        Ok(Linking::Linked(Link::Account(link)))
    }

    fn forget_pin(&self, id: u64) {
        self.pins.lock().remove(&id);
    }

    /// Keep `link` as `user`'s.
    pub fn save(&self, user: LocalUserId, link: &AccountLink) -> Result<(), LinkError> {
        let text = serde_json::to_string(link).map_err(std::io::Error::other)?;
        self.secrets.set(&secret_name(user), &text)?;
        let _ = self.changed.send(user);
        Ok(())
    }

    /// Unlink `user`'s Plex account: the token is deleted. Whether there
    /// was one; the config's owner link cannot be.
    pub fn unlink(&self, user: LocalUserId, by: &str) -> Result<bool, LinkError> {
        let removed = self.secrets.remove(&secret_name(user))?;
        if !removed && self.owner == Some(user) {
            return Err(LinkError::ByConfig);
        }
        if removed {
            tracing::info!(
                user = %user,
                by,
                "Plex account unlinked; its token is deleted"
            );
            let _ = self.changed.send(user);
        }
        Ok(removed)
    }

    /// The Plex servers `token`'s account reaches.
    pub async fn servers(&self, token: &str) -> Result<Vec<PlexResource>, LinkError> {
        Ok(self.plex_tv()?.servers(token).await?)
    }
}

/// The user `plex.watch_sync.user` names, if it does and they exist.
pub async fn configured_owner(
    config: &ServerConfig,
    store: &Store,
) -> Result<Option<LocalUserId>, UserError> {
    let Some(name) = config.plex.as_ref().and_then(|p| p.owner()) else {
        return Ok(None);
    };
    match store.password_login(name).await? {
        Some(user) => Ok(Some(user.id)),
        None => {
            tracing::warn!(
                user = name,
                "plex.watch_sync.user does not exist; nobody is linked to Plex by the config"
            );
            Ok(None)
        }
    }
}

/// The address `resource` is best reached at, and whether it is on the
/// LAN: never the relay.
fn address(resource: &PlexResource) -> Option<(&str, bool)> {
    resource
        .connections
        .iter()
        .find(|c| !c.relay)
        .map(|c| (c.uri.as_str(), c.local))
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// `url` is `connection`: its address, or the address and port plex.tv
/// says its plex.direct name stands for (where discovery finds it).
fn reaches(connection: &PlexConnection, url: &str) -> bool {
    if same_url(&connection.uri, url) {
        return true;
    }
    let (Some(address), Some(port)) = (&connection.address, connection.port) else {
        return false;
    };
    Url::parse(url.trim()).is_ok_and(|u| {
        u.host_str()
            .is_some_and(|h| h.trim_start_matches('[').trim_end_matches(']') == address)
            && u.port_or_known_default() == Some(port)
    })
}

/// The account's servers as the add-a-library flow lists them, each marked
/// with the id it is added under among `configured` (id, URL), if it is.
pub fn account_servers(
    resources: &[PlexResource],
    configured: &[(String, String)],
) -> Vec<PlexAccountServer> {
    resources
        .iter()
        .map(|r| {
            let added = configured
                .iter()
                .find(|(_, url)| r.connections.iter().any(|c| same_url(&c.uri, url)))
                .map(|(id, _)| id.clone());
            let (url, local) = match address(r) {
                Some((url, local)) => (Some(url.to_owned()), local),
                None => (None, false),
            };
            PlexAccountServer {
                name: r.name.clone(),
                machine_id: r.machine_id.clone(),
                url,
                local,
                owned: r.owned,
                added,
            }
        })
        .collect()
}

/// The token the account uses on the server at `url`, if it reaches it.
pub fn token_for_url(resources: &[PlexResource], url: &str) -> Option<String> {
    resources
        .iter()
        .find(|r| r.connections.iter().any(|c| reaches(c, url)))
        .and_then(|r| r.access_token.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The begin window is timed by the store's clock: a user at the
    /// limit is let begin again once it has passed (review 2026-10-11
    /// L42).
    #[tokio::test]
    async fn a_user_at_the_begin_limit_may_begin_again_after_the_window() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-link-window").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let clock = store.clock().clone();
        let alice = store
            .add_user("alice", None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        let accounts = PlexAccounts::new(
            crate::config::secrets::Secrets::new(dir.as_ref()),
            store,
            PlexSite {
                // Nothing listens: a begin let through fails reaching it.
                api: "http://127.0.0.1:9".parse().unwrap(),
                app: "http://127.0.0.1:9".parse().unwrap(),
            },
            None,
        );
        accounts.begun.lock().insert(
            alice,
            std::iter::repeat_n(clock.now(), MAX_LINK_BEGINS).collect(),
        );
        assert!(matches!(
            accounts.begin(alice, None).await,
            Err(LinkError::TooMany { .. })
        ));
        clock.advance(LINK_BEGIN_WINDOW);
        assert!(!matches!(
            accounts.begin(alice, None).await,
            Err(LinkError::TooMany { .. })
        ));
    }

    fn resource(name: &str, connections: &[(&str, bool, bool)]) -> PlexResource {
        PlexResource {
            name: name.into(),
            machine_id: format!("{}-machine", name.to_lowercase()),
            owned: name == "Tower",
            access_token: Some(format!("tok-{}", name.to_lowercase())),
            connections: connections
                .iter()
                .map(|&(uri, local, relay)| PlexConnection {
                    uri: uri.into(),
                    address: None,
                    port: None,
                    local,
                    relay,
                })
                .collect(),
        }
    }

    #[test]
    fn an_account_s_servers_are_listed_by_their_best_address_and_marked_when_added() {
        let resources = [
            resource("Tower", &[("http://10.0.0.2:32400", true, false)]),
            resource(
                "Cabin",
                &[
                    ("https://1-2-3-4.cabin.plex.direct:32400", false, false),
                    ("https://relay.plex.direct:8443", false, true),
                ],
            ),
            resource("Far", &[("https://relay.plex.direct:8443", false, true)]),
        ];
        let listed = account_servers(
            &resources,
            &[("plex".into(), "http://10.0.0.2:32400/".into())],
        );
        assert_eq!(listed[0].added.as_deref(), Some("plex"));
        assert!(listed[0].local && listed[0].owned);
        assert_eq!(
            listed[1].url.as_deref(),
            Some("https://1-2-3-4.cabin.plex.direct:32400")
        );
        assert!(!listed[1].local);
        assert_eq!(listed[1].added, None);
        assert_eq!(listed[2].url, None);
        assert_eq!(
            token_for_url(&resources, "https://1-2-3-4.cabin.plex.direct:32400/").as_deref(),
            Some("tok-cabin")
        );
        assert_eq!(token_for_url(&resources, "http://nas:32400"), None);
    }

    #[test]
    fn a_server_found_on_the_lan_gets_the_token_of_the_plex_direct_name_for_its_address() {
        let mut tower = resource(
            "Tower",
            &[("https://10-0-0-2.abc.plex.direct:32400", true, false)],
        );
        tower.connections[0].address = Some("10.0.0.2".into());
        tower.connections[0].port = Some(32400);
        let resources = [tower];
        assert_eq!(
            token_for_url(&resources, "http://10.0.0.2:32400").as_deref(),
            Some("tok-tower")
        );
        assert_eq!(token_for_url(&resources, "http://10.0.0.2:32401"), None);
        assert_eq!(token_for_url(&resources, "http://10.0.0.3:32400"), None);
    }

    #[test]
    fn a_link_never_shows_its_token() {
        let link = AccountLink {
            token: "s3cr3t".into(),
            username: Some("alice".into()),
            email: None,
            linked_at: 1,
        };
        assert!(!format!("{link:?}").contains("s3cr3t"));
        let shown = serde_json::to_string(&Link::Account(link).shown()).unwrap();
        assert!(!shown.contains("s3cr3t"), "{shown}");
    }
}

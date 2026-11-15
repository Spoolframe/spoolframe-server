//! The calls of a regular feature that brings requests
//! ([`spoolframe_protocol::features`]): reached only through proof that
//! the feature is on for the connection.
//!
//! A feature's calls are on [`On<G>`] (in a session) or [`Invited<G>`]
//! (before one), one `impl` block a feature. Neither is made unless the
//! server named the feature: [`crate::Session::feature`] and
//! [`crate::Client::invited`] are [`SdkError::FeatureOff`] otherwise, and
//! nothing is sent. Hide what needs it, as for an older server.
//!
//! A reconnect may turn a feature off (the server was replaced by an
//! older one); a call then is [`SdkError::FeatureOff`] too, and still
//! sends nothing.

use std::fmt;
use std::marker::PhantomData;

use iroh::EndpointId;
use spoolframe_protocol::diagnostics::DiagnosticsReport;
use spoolframe_protocol::downloads::DownloadAddonOptions;
use spoolframe_protocol::invite::Invite;
use spoolframe_protocol::{Feature, NextMode, Reply, Request};
use spoolframe_types::{Item, MediaId};

use crate::{Client, SdkError, Session};

mod sealed {
    pub trait Sealed {}
}

/// A regular feature with calls of its own. Sealed: one for each such
/// feature this SDK has.
pub trait Gate: sealed::Sealed + 'static {
    const FEATURE: Feature;
}

/// A marker type for each feature with calls.
macro_rules! feature {
    ($(#[$doc:meta])* $marker:ident = $feature:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub enum $marker {}

        impl sealed::Sealed for $marker {}

        impl Gate for $marker {
            const FEATURE: Feature = $feature;
        }
    };
}

feature!(
    /// `spoolframe:october-2026`: what plays after an episode
    /// ([`On<October2026>`]), and an account made with an invite and an
    /// app passkey, with no password ([`Invited<October2026>`]).
    October2026 = Feature::October2026
);

feature!(
    /// `spoolframe:download-addons`: subtitles and audio tracks added to
    /// a download without fetching the film again
    /// ([`On<DownloadAddons>`], and [`crate::Downloads::add_tracks`]).
    DownloadAddons = Feature::DownloadAddons
);

feature!(
    /// `spoolframe:diagnostics`: an app's playback log sent to its server
    /// for the admins to read ([`On<Diagnostics>`]).
    Diagnostics = Feature::Diagnostics
);

/// Feature `G`'s calls in a session: [`crate::Session::feature`]. Cheap
/// to clone.
pub struct On<G: Gate> {
    pub(crate) session: Session,
    gate: PhantomData<G>,
}

impl<G: Gate> On<G> {
    /// The session it calls in.
    pub fn session(&self) -> &Session {
        &self.session
    }
}

impl<G: Gate> Clone for On<G> {
    fn clone(&self) -> Self {
        Self {
            session: self.session.clone(),
            gate: PhantomData,
        }
    }
}

impl<G: Gate> fmt::Debug for On<G> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("On")
            .field("feature", &G::FEATURE)
            .field("server", &self.session.server)
            .finish()
    }
}

/// Feature `G`'s calls before a session, for an invite's server:
/// [`crate::Client::invited`]. Cheap to clone.
pub struct Invited<G: Gate> {
    pub(crate) client: Client,
    pub(crate) link: String,
    /// The link, read once.
    pub(crate) invite: Invite,
    /// The server's key it names.
    pub(crate) id: EndpointId,
    gate: PhantomData<G>,
}

impl<G: Gate> Invited<G> {
    pub(crate) fn new(client: Client, link: String, invite: Invite, id: EndpointId) -> Self {
        Self {
            client,
            link,
            invite,
            id,
            gate: PhantomData,
        }
    }

    /// The invite link it was made for.
    pub fn link(&self) -> &str {
        &self.link
    }
}

impl<G: Gate> Clone for Invited<G> {
    fn clone(&self) -> Self {
        Self::new(
            self.client.clone(),
            self.link.clone(),
            self.invite.clone(),
            self.id,
        )
    }
}

impl<G: Gate> fmt::Debug for Invited<G> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invited")
            .field("feature", &G::FEATURE)
            .field("server", &self.id)
            .finish()
    }
}

impl Session {
    /// Feature `G`'s calls, if it is on for the connection this session
    /// is on ([`Session::has_feature`]); otherwise
    /// [`SdkError::FeatureOff`], and nothing is sent.
    pub fn feature<G: Gate>(&self) -> Result<On<G>, SdkError> {
        if !self.has_feature(G::FEATURE) {
            return Err(SdkError::FeatureOff {
                feature: G::FEATURE,
            });
        }
        Ok(On {
            session: self.clone(),
            gate: PhantomData,
        })
    }
}

impl On<October2026> {
    /// The episode to play after episode `item`: with
    /// [`NextMode::InOrder`] the one right after it, with
    /// [`NextMode::FirstUnwatched`] the first after it the user has not
    /// watched. Specials are in the order, first, only with `specials`.
    /// `None` at the show's end, and for an episode the user may not see.
    /// An item that is not an episode is [`SdkError::InvalidInput`].
    pub async fn next_episode(
        &self,
        item: &MediaId,
        mode: NextMode,
        specials: bool,
    ) -> Result<Option<Item>, SdkError> {
        self.next(Request::NextEpisode {
            item: item.clone(),
            mode,
            specials,
        })
        .await
    }

    /// Where show `show`'s Continue goes: the first episode in progress,
    /// else the first the user has not watched, specials left out. `None`
    /// when every one is watched. An item that is not a show is
    /// [`SdkError::InvalidInput`].
    pub async fn show_next(&self, show: &MediaId) -> Result<Option<Item>, SdkError> {
        self.next(Request::ShowNext { show: show.clone() }).await
    }

    async fn next(&self, request: Request) -> Result<Option<Item>, SdkError> {
        match self.session.call(request).await? {
            Reply::NextEpisode { item } => Ok(item),
            other => Err(crate::session::unexpected(other)),
        }
    }
}

/// How long [`On::read_track_subtitle`] waits between two asks while the
/// server reads pictures as text.
const NOT_READY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long [`On::read_track_subtitle`] waits in all for pictures read as
/// text: a film's take minutes, one at a time on the server.
const NOT_READY_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

impl On<DownloadAddons> {
    /// [`crate::Downloads::add_tracks`] of this session's client: for an
    /// app that holds the feature's calls alone (a UniFFI binding). The
    /// audio is queued for the download manager; the subtitles are fetched
    /// in the call.
    pub async fn add_tracks(
        &self,
        key: &str,
        audio: Vec<u32>,
        subtitles: Vec<u32>,
    ) -> Result<crate::LocalDownload, SdkError> {
        self.session
            .client
            .downloads()
            .add_tracks(key, self, audio, subtitles)
            .await
    }

    /// What may be added to a download of film or episode `id`: each
    /// audio track with its file's size, and each subtitle, with why one
    /// cannot be added. A paired server's item is
    /// [`SdkError::Unsupported`].
    pub async fn download_addon_options(
        &self,
        id: &MediaId,
    ) -> Result<DownloadAddonOptions, SdkError> {
        match self
            .session
            .call(Request::DownloadAddonOptions { id: id.clone() })
            .await?
        {
            Reply::DownloadAddonOptions(options) => Ok(options),
            other => Err(crate::session::unexpected(other)),
        }
    }

    /// Subtitle `subtitle` (its place in the item's tracks) of film or
    /// episode `id`, whole, as WebVTT text: on an Apple device, timed to
    /// the download's MP4. Waits, asking again every few seconds, while
    /// the server reads pictures as text; past half an hour it is
    /// [`SdkError::SubtitleNotReady`].
    pub async fn read_track_subtitle(
        &self,
        id: &MediaId,
        subtitle: u32,
    ) -> Result<String, SdkError> {
        let deadline = tokio::time::Instant::now() + NOT_READY_LIMIT;
        loop {
            match self.session.track_subtitle(id, subtitle).await {
                Ok(read) => {
                    return String::from_utf8(read.bytes).map_err(|_| {
                        SdkError::Protocol("a WebVTT subtitle that is not UTF-8".into())
                    });
                }
                Err(SdkError::SubtitleNotReady { .. })
                    if tokio::time::Instant::now() + NOT_READY_WAIT < deadline =>
                {
                    tokio::time::sleep(NOT_READY_WAIT).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl On<Diagnostics> {
    /// Send `report` to the server for its admins to read, and the id it
    /// is kept under. A part over its limit
    /// ([`spoolframe_protocol::diagnostics`]: the log at most 960 KiB) is
    /// [`SdkError::InvalidInput`], and nothing is sent: trim the log to
    /// its newest lines. The server takes one a minute from a session;
    /// another sooner is [`SdkError::RateLimited`]. Never sent again by
    /// itself.
    pub async fn send_diagnostics(&self, report: &DiagnosticsReport) -> Result<String, SdkError> {
        if let Some(why) = report.refusal() {
            return Err(SdkError::InvalidInput(why));
        }
        match self
            .session
            .call(Request::SendDiagnostics {
                report: report.clone(),
            })
            .await?
        {
            Reply::DiagnosticsSent { id } => Ok(id),
            other => Err(crate::session::unexpected(other)),
        }
    }
}

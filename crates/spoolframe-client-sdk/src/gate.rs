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
    /// `spoolframe:next-episode`: what plays after an episode
    /// ([`On<NextEpisodes>`]).
    NextEpisodes = Feature::NextEpisodes
);
feature!(
    /// `spoolframe:invite-passkeys`: an account made with an invite and
    /// an app passkey, with no password ([`Invited<InvitePasskeys>`]).
    InvitePasskeys = Feature::InvitePasskeys
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

impl On<NextEpisodes> {
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

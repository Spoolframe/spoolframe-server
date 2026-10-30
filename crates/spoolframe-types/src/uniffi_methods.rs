//! Methods Swift and Kotlin call on the types (feature `uniffi`), where
//! the Rust method borrows and UniFFI needs an owned value: the same
//! answer, under the Rust method's name.

use crate::{Artwork, ArtworkKind, Item, MediaId, PlaybackState, QuotaOf};

#[uniffi::export]
impl Item {
    /// Its id, whatever kind it is: what a list keys its rows by.
    #[uniffi::method(name = "id")]
    pub fn id_owned(&self) -> MediaId {
        self.id().clone()
    }

    /// Its own name: a film's, an episode's or a track's title; an
    /// artist's name.
    #[uniffi::method(name = "title")]
    pub fn title_owned(&self) -> String {
        self.title().to_owned()
    }

    /// Its artwork, whatever kind it is.
    #[uniffi::method(name = "artwork")]
    pub fn artwork_owned(&self) -> Vec<Artwork> {
        self.artwork().to_vec()
    }

    /// Its first poster.
    pub fn poster(&self) -> Option<Artwork> {
        self.artwork_of(ArtworkKind::Poster).cloned()
    }

    /// Its first background.
    pub fn background(&self) -> Option<Artwork> {
        self.artwork_of(ArtworkKind::Background).cloned()
    }

    /// Its first thumbnail.
    pub fn thumb(&self) -> Option<Artwork> {
        self.artwork_of(ArtworkKind::Thumb).cloned()
    }

    /// The user's progress on a film, an episode or a track; `None` for
    /// the kinds that do not play themselves.
    #[uniffi::method(name = "playback")]
    pub fn playback_owned(&self) -> Option<PlaybackState> {
        self.playback().cloned()
    }
}

/// The server id of the paired server a federated item is on; `None`
/// for the server's own ([`MediaId::origin`]).
#[uniffi::export]
pub fn media_origin(id: MediaId) -> Option<String> {
    id.origin().map(str::to_owned)
}

#[uniffi::export]
impl QuotaOf {
    /// It is the user's own daily limit, not the server's account at the
    /// site: "You've reached your limit" rather than "No one can".
    pub fn is_user(&self) -> bool {
        *self == QuotaOf::User
    }
}

//! Identifiers. On the wire each one is a namespaced string, for example
//! `plex:movie:12345`.
//!
//! Clients treat them as opaque: they compare them and send them back, and
//! never take them apart. The namespace is part of the ID so that `native:`
//! IDs (and, with federation, IDs from another server) can be added without
//! a format change. Only the server reads the parts, through the accessors.
//!
//! A paired server's item (P8-02) is in [`Namespace::Federated`]: its key
//! is the origin's server id, then the id the origin knows it by without
//! the kind, which the federated id carries already
//! (`fed:movie:mzxw6ytbojtg633c:plex:12`). The origin is the server id,
//! not its endpoint key, so a rotated key keeps every id.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a string is not a valid identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{0:?} is not a known ID namespace")]
    UnknownNamespace(String),
    #[error("{0:?} is not a known media kind")]
    UnknownKind(String),
    #[error("{0:?} has no key")]
    MissingKey(String),
    /// A federated id's origin is empty or has a `:` in it.
    #[error("{0:?} is not a server id to name an origin by")]
    InvalidOrigin(String),
}

/// Which catalog an ID belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Namespace {
    /// An item in the Plex library; the key is its Plex `ratingKey`.
    Plex,
    /// Something the server made itself rather than a catalog: a sheet of
    /// frame previews (P3-13), an [`ArtworkId`] like `preview:<set>/<n>`.
    Preview,
    /// An image the server keeps or draws itself (PX-53): a playlist's
    /// uploaded cover (`image:playlist/<hash>`), its mosaic, a built-in
    /// playlist's tile. Only ever an [`ArtworkId`].
    Image,
    /// A paired server's (P8-02), as this server caches it: the key is the
    /// origin's server id and the id the origin knows it by
    /// (`fed:movie:<origin>:plex:12`, `fed:<origin>:plex:2`).
    Federated,
}

impl Namespace {
    fn as_str(self) -> &'static str {
        match self {
            Namespace::Plex => "plex",
            Namespace::Preview => "preview",
            Namespace::Image => "image",
            Namespace::Federated => "fed",
        }
    }
}

/// `origin` checked as a federated id's: not empty, and no `:` in it.
fn origin(origin: &str) -> Result<&str, IdError> {
    if origin.is_empty() || origin.contains(':') {
        return Err(IdError::InvalidOrigin(origin.to_owned()));
    }
    Ok(origin)
}

/// A federated key's origin and the rest, if it is one.
fn split_origin(key: &str) -> Option<(&str, &str)> {
    key.split_once(':')
        .filter(|(origin, rest)| !origin.is_empty() && !rest.is_empty())
}

impl FromStr for Namespace {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        match s {
            "plex" => Ok(Namespace::Plex),
            "preview" => Ok(Namespace::Preview),
            "image" => Ok(Namespace::Image),
            "fed" => Ok(Namespace::Federated),
            other => Err(IdError::UnknownNamespace(other.to_owned())),
        }
    }
}

/// What kind of item a [`MediaId`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Movie,
    Show,
    Season,
    Episode,
    Artist,
    Album,
    Track,
}

impl MediaKind {
    /// Its name in an id (`movie` in `plex:movie:12`).
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Movie => "movie",
            MediaKind::Show => "show",
            MediaKind::Season => "season",
            MediaKind::Episode => "episode",
            MediaKind::Artist => "artist",
            MediaKind::Album => "album",
            MediaKind::Track => "track",
        }
    }

    /// Whether an item of this kind plays: a movie, an episode or a
    /// track, never a show, a season, an artist or an album.
    pub fn plays(self) -> bool {
        matches!(
            self,
            MediaKind::Movie | MediaKind::Episode | MediaKind::Track
        )
    }
}

impl FromStr for MediaKind {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        Ok(match s {
            "movie" => MediaKind::Movie,
            "show" => MediaKind::Show,
            "season" => MediaKind::Season,
            "episode" => MediaKind::Episode,
            "artist" => MediaKind::Artist,
            "album" => MediaKind::Album,
            "track" => MediaKind::Track,
            other => return Err(IdError::UnknownKind(other.to_owned())),
        })
    }
}

/// A key must be non-empty; it may contain `:` (only the leading parts are
/// split off).
fn key(whole: &str, key: Option<&str>) -> Result<String, IdError> {
    match key {
        Some(k) if !k.is_empty() => Ok(k.to_owned()),
        _ => Err(IdError::MissingKey(whole.to_owned())),
    }
}

/// A movie, show, season, episode, artist, album or track:
/// `<namespace>:<kind>:<key>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MediaId {
    namespace: Namespace,
    kind: MediaKind,
    key: String,
}

impl MediaId {
    /// Build an ID. The server does this; clients only receive them.
    pub fn new(
        namespace: Namespace,
        kind: MediaKind,
        key: impl Into<String>,
    ) -> Result<Self, IdError> {
        let key = key.into();
        if key.is_empty() {
            return Err(IdError::MissingKey(format!(
                "{}:{}:",
                namespace.as_str(),
                kind.as_str()
            )));
        }
        Ok(Self {
            namespace,
            kind,
            key,
        })
    }

    /// For the server: which catalog resolves this ID.
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// For the server: what kind of item this is.
    pub fn kind(&self) -> MediaKind {
        self.kind
    }

    /// For the server: the catalog's own key for the item.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// `remote`, an item of the server whose server id is `origin`, as
    /// this server names it (P8-02): `fed:<kind>:<origin>:<namespace>:<key>`.
    pub fn federated(origin_id: &str, remote: &MediaId) -> Result<Self, IdError> {
        let key = format!(
            "{}:{}:{}",
            origin(origin_id)?,
            remote.namespace.as_str(),
            remote.key
        );
        Self::new(Namespace::Federated, remote.kind, key)
    }

    /// For a federated id: the origin's server id, and the id the origin
    /// knows the item by. `None` for any other id, or one that does not
    /// read as one.
    pub fn remote(&self) -> Option<(&str, MediaId)> {
        if self.namespace != Namespace::Federated {
            return None;
        }
        let (origin, rest) = split_origin(&self.key)?;
        let (namespace, key) = rest.split_once(':')?;
        Some((
            origin,
            Self::new(namespace.parse().ok()?, self.kind, key).ok()?,
        ))
    }

    /// The server id of the paired server a federated id's item is on;
    /// `None` for this server's own, as [`MediaId::remote`] reads it.
    pub fn origin(&self) -> Option<&str> {
        self.remote().map(|(origin, _)| origin)
    }
}

impl fmt::Display for MediaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.namespace.as_str(),
            self.kind.as_str(),
            self.key
        )
    }
}

impl FromStr for MediaId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        let mut parts = s.splitn(3, ':');
        let namespace = parts.next().unwrap_or_default().parse()?;
        let kind = parts.next().unwrap_or_default().parse()?;
        let key = key(s, parts.next())?;
        Ok(Self {
            namespace,
            kind,
            key,
        })
    }
}

/// Defines an ID made of a namespace and a key: `<namespace>:<key>`.
macro_rules! namespaced_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name {
            namespace: Namespace,
            key: String,
        }

        impl $name {
            /// Build an ID. The server does this; clients only receive them.
            pub fn new(namespace: Namespace, key: impl Into<String>) -> Result<Self, IdError> {
                let key = key.into();
                if key.is_empty() {
                    return Err(IdError::MissingKey(format!("{}:", namespace.as_str())));
                }
                Ok(Self { namespace, key })
            }

            /// For the server: which catalog resolves this ID.
            pub fn namespace(&self) -> Namespace {
                self.namespace
            }

            /// For the server: the catalog's own key.
            pub fn key(&self) -> &str {
                &self.key
            }

            /// `remote`, of the server whose server id is `origin`, as
            /// this server names it (P8-02): `fed:<origin>:<remote>`.
            pub fn federated(origin_id: &str, remote: &$name) -> Result<Self, IdError> {
                Self::new(
                    Namespace::Federated,
                    format!("{}:{remote}", origin(origin_id)?),
                )
            }

            /// For a federated id: the origin's server id, and the id the
            /// origin knows it by. `None` for any other id, or one that
            /// does not read as one.
            pub fn remote(&self) -> Option<(&str, $name)> {
                if self.namespace != Namespace::Federated {
                    return None;
                }
                let (origin, rest) = split_origin(&self.key)?;
                Some((origin, rest.parse().ok()?))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", self.namespace.as_str(), self.key)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                let mut parts = s.splitn(2, ':');
                let namespace = parts.next().unwrap_or_default().parse()?;
                let key = key(s, parts.next())?;
                Ok(Self { namespace, key })
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(s: String) -> Result<Self, IdError> {
                s.parse()
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.to_string()
            }
        }
    };
}

namespaced_id!(
    /// A library (a Plex library section today): `<namespace>:<key>`.
    LibraryId
);

namespaced_id!(
    /// A piece of artwork, fetched through the client protocol, never from
    /// Plex directly: `<namespace>:<key>`.
    ArtworkId
);

namespaced_id!(
    /// A person a search found (P3-09), whose items can be listed:
    /// `<namespace>:<key>`.
    PersonId
);

impl TryFrom<String> for MediaId {
    type Error = IdError;

    fn try_from(s: String) -> Result<Self, IdError> {
        s.parse()
    }
}

impl From<MediaId> for String {
    fn from(id: MediaId) -> String {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_media_id_prints_as_namespace_kind_key() {
        let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "12345").unwrap();
        assert_eq!(id.to_string(), "plex:movie:12345");
    }

    #[test]
    fn a_media_id_survives_a_round_trip_through_its_string() {
        let id: MediaId = "plex:episode:98321".parse().unwrap();
        assert_eq!(id.to_string().parse::<MediaId>().unwrap(), id);
    }

    #[test]
    fn a_media_id_on_the_wire_is_a_plain_string() {
        let id = MediaId::new(Namespace::Plex, MediaKind::Track, "7").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""plex:track:7""#);
    }

    #[test]
    fn a_key_may_contain_colons() {
        let id: MediaId = "plex:movie:a:b".parse().unwrap();
        assert_eq!(id.key(), "a:b");
    }

    #[test]
    fn an_unknown_namespace_is_rejected() {
        assert_eq!(
            "nope:movie:1".parse::<MediaId>(),
            Err(IdError::UnknownNamespace("nope".into()))
        );
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        assert_eq!(
            "plex:photo:1".parse::<MediaId>(),
            Err(IdError::UnknownKind("photo".into()))
        );
    }

    #[test]
    fn an_empty_key_is_rejected() {
        assert_eq!(
            "plex:movie:".parse::<MediaId>(),
            Err(IdError::MissingKey("plex:movie:".into()))
        );
        assert!(MediaId::new(Namespace::Plex, MediaKind::Movie, "").is_err());
    }

    #[test]
    fn a_missing_key_is_rejected() {
        assert!("plex:movie".parse::<MediaId>().is_err());
    }

    #[test]
    fn deserializing_a_bad_media_id_fails() {
        assert!(serde_json::from_str::<MediaId>(r#""nope:movie:1""#).is_err());
    }

    #[test]
    fn a_library_id_round_trips_through_its_string() {
        let id = LibraryId::new(Namespace::Plex, "3").unwrap();
        assert_eq!(id.to_string(), "plex:3");
        assert_eq!("plex:3".parse::<LibraryId>().unwrap(), id);
    }

    #[test]
    fn an_artwork_id_keeps_a_path_like_key() {
        let id: ArtworkId = "plex:/library/metadata/123/thumb/1699".parse().unwrap();
        assert_eq!(id.key(), "/library/metadata/123/thumb/1699");
    }

    #[test]
    fn a_preview_sheet_s_id_is_in_a_namespace_of_its_own() {
        let id: ArtworkId = "preview:0f3a/2".parse().unwrap();
        assert_eq!(id.namespace(), Namespace::Preview);
        assert_eq!(id.key(), "0f3a/2");
        assert_eq!(id.to_string(), "preview:0f3a/2");
    }

    #[test]
    fn a_stored_image_s_id_is_in_a_namespace_of_its_own() {
        let id: ArtworkId = "image:playlist/0f3a".parse().unwrap();
        assert_eq!(id.namespace(), Namespace::Image);
        assert_eq!(id.key(), "playlist/0f3a");
        assert_eq!(id.to_string(), "image:playlist/0f3a");
    }

    const ORIGIN: &str = "mzxw6ytbojtg633c";

    /// P8-02: a paired server's item names its origin and the origin's
    /// id, reads back with the parser every id reads with, and says what
    /// it is of.
    #[test]
    fn a_federated_id_round_trips_and_names_its_origin() {
        let remote: MediaId = "plex:episode:cabin:12".parse().unwrap();
        let id = MediaId::federated(ORIGIN, &remote).unwrap();
        assert_eq!(id.to_string(), "fed:episode:mzxw6ytbojtg633c:plex:cabin:12");
        assert_eq!(id.namespace(), Namespace::Federated);
        assert_eq!(id.kind(), MediaKind::Episode);
        let read: MediaId = id.to_string().parse().unwrap();
        assert_eq!(read, id);
        assert_eq!(read.remote(), Some((ORIGIN, remote)));
        assert_eq!(
            serde_json::from_str::<MediaId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );

        let library = LibraryId::federated(ORIGIN, &"plex:2".parse().unwrap()).unwrap();
        assert_eq!(library.to_string(), "fed:mzxw6ytbojtg633c:plex:2");
        assert_eq!(
            library.to_string().parse::<LibraryId>().unwrap().remote(),
            Some((ORIGIN, "plex:2".parse().unwrap()))
        );
        let art: ArtworkId = "plex:/library/metadata/1/thumb/2".parse().unwrap();
        let ours = ArtworkId::federated(ORIGIN, &art).unwrap();
        assert_eq!(
            ours.to_string(),
            "fed:mzxw6ytbojtg633c:plex:/library/metadata/1/thumb/2"
        );
        assert_eq!(ours.remote(), Some((ORIGIN, art)));
    }

    /// The namespaces there were read and print as they did, and are not
    /// federated.
    #[test]
    fn the_namespaces_there_were_are_unchanged() {
        for text in [
            "plex:movie:12345",
            "plex:episode:cabin:12",
            "plex:movie:a:b",
        ] {
            let id: MediaId = text.parse().unwrap();
            assert_eq!(id.to_string(), text);
            assert_eq!(id.remote(), None);
        }
        for text in ["plex:3", "preview:0f3a/2", "image:playlist/0f3a"] {
            let id: ArtworkId = text.parse().unwrap();
            assert_eq!(id.to_string(), text);
            assert_eq!(id.remote(), None);
        }
    }

    #[test]
    fn a_federated_id_needs_an_origin_and_an_id_the_origin_reads() {
        let remote: MediaId = "plex:movie:1".parse().unwrap();
        assert_eq!(
            MediaId::federated("", &remote),
            Err(IdError::InvalidOrigin(String::new()))
        );
        assert!(MediaId::federated("a:b", &remote).is_err());
        // Read, but not as anything an origin has.
        for text in [
            "fed:movie:mzxw",
            "fed:movie:mzxw:nope:1",
            "fed:movie::plex:1",
        ] {
            let id: MediaId = text.parse().unwrap();
            assert_eq!(id.remote(), None, "{text}");
        }
    }

    #[test]
    fn an_artwork_id_without_a_key_is_rejected() {
        assert!("plex:".parse::<ArtworkId>().is_err());
        assert!("plex".parse::<ArtworkId>().is_err());
    }
}

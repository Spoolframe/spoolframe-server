//! Regular features: how a server knows a client handles something.
//!
//! A client names the features it handles in [`Hello::features`]; the
//! server's answer, [`ServerInfo::features`], names those of them it has.
//! A feature is on for the connection only when both named it. Then, and
//! only then, the server answers the feature's requests (a request of a
//! feature that is off is `Unsupported`, as one it does not know) and
//! sends its events. A name either side does not know is ignored, never
//! an error, and both fields are left out when empty, so an older peer
//! sends and reads what it did before.
//!
//! Every opt-in request and event comes with a feature. A new reply field
//! needs none: it is `serde(default)`, and an `Option` where its absence
//! means something else than its default. Nothing is decided from a
//! version number. Unlike the experimental features
//! ([`crate::experimental`]), these are ordinary protocol, with its
//! stability, and a server that has one always names it when asked. A
//! name stays once added.
//!
//! [`Hello::features`]: crate::Hello::features
//! [`ServerInfo::features`]: crate::ServerInfo::features

use std::fmt;

use enumset::{EnumSet, EnumSetType};

/// A regular feature. On the wire it is its name ([`Feature::name`]).
#[derive(Debug, Hash, PartialOrd, Ord, EnumSetType)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[non_exhaustive]
pub enum Feature {
    /// `ProfileRemoved` and `PinChanged` on the events stream.
    ProfileEvents,
    /// `WatchChanged` on the events stream.
    WatchEvents,
    /// `NextEpisode` and `ShowNext`.
    NextEpisodes,
    /// `BeginInvitePasskey` and `RedeemInviteWithPasskey`: an account
    /// made with an invite and an app passkey.
    InvitePasskeys,
}

/// A set of features: [`Features::all`] is every one this build knows,
/// and `client & server` those on for a connection. Bit positions are
/// this build's own; only names cross the wire ([`names`]).
pub type Features = EnumSet<Feature>;

impl Feature {
    /// Its name on the wire: the one place the names are written.
    pub const fn name(self) -> &'static str {
        match self {
            Feature::ProfileEvents => "spoolframe:profile-events",
            Feature::WatchEvents => "spoolframe:watch-events",
            Feature::NextEpisodes => "spoolframe:next-episode",
            Feature::InvitePasskeys => "spoolframe:invite-passkeys",
        }
    }

    /// The feature named `name`, if this build knows it.
    pub fn of(name: &str) -> Option<Feature> {
        Features::all().iter().find(|f| f.name() == name)
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// [`Features`] on the wire, for `#[serde(with = "names")]`: the list
/// of their names. A name this build does not know is skipped as it is
/// read. Never enumset's own serde, which would send its bits.
pub mod names {
    use std::fmt;

    use serde::de::{SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use serde::{Deserializer, Serializer};

    use super::{Feature, Features};

    pub fn serialize<S: Serializer>(features: &Features, s: S) -> Result<S::Ok, S::Error> {
        let mut names = s.serialize_seq(Some(features.len()))?;
        for feature in features.iter() {
            names.serialize_element(feature.name())?;
        }
        names.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Features, D::Error> {
        struct Names;

        impl<'de> Visitor<'de> for Names {
            type Value = Features;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of feature names")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Features, A::Error> {
                let mut features = Features::empty();
                // Owned: CBOR text may come in chunks.
                while let Some(name) = seq.next_element::<String>()? {
                    if let Some(feature) = Feature::of(&name) {
                        features |= feature;
                    }
                }
                Ok(features)
            }
        }

        d.deserialize_seq(Names)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    /// A field as `Hello` and `ServerInfo` carry it.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Named(#[serde(with = "names")] Features);

    #[test]
    fn every_feature_is_named_once_and_read_back() {
        let mut names: Vec<&str> = Features::all().iter().map(Feature::name).collect();
        for feature in Features::all() {
            assert_eq!(Feature::of(feature.name()), Some(feature));
            assert!(feature.name().starts_with("spoolframe:"), "{feature}");
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Features::all().len());
        assert_eq!(Feature::of("spoolframe:teleport"), None);
    }

    /// What is on for a connection: those both sides named.
    #[test]
    fn on_is_what_both_named() {
        let client = Feature::ProfileEvents | Feature::InvitePasskeys;
        let server = Feature::InvitePasskeys | Feature::WatchEvents;
        assert_eq!(client & server, Feature::InvitePasskeys);
        assert!((client & Features::empty()).is_empty());
    }

    /// Only names cross the wire, in JSON and in CBOR.
    #[test]
    fn a_set_goes_as_the_list_of_its_names() {
        let set = Named(Feature::ProfileEvents | Feature::InvitePasskeys);
        assert_eq!(
            serde_json::to_value(&set).unwrap(),
            serde_json::json!(["spoolframe:profile-events", "spoolframe:invite-passkeys"])
        );
        let cbor = crate::codec::encode(&set).unwrap();
        let value: ciborium::Value = ciborium::from_reader(cbor.as_slice()).unwrap();
        assert_eq!(
            value,
            ciborium::Value::Array(vec![
                "spoolframe:profile-events".into(),
                "spoolframe:invite-passkeys".into(),
            ])
        );
        assert_eq!(crate::codec::decode::<Named>(&cbor).unwrap(), set);
    }

    /// A name this build does not know is skipped; the rest are read.
    #[test]
    fn an_unknown_name_is_skipped() {
        let names = vec!["spoolframe:teleport", "spoolframe:watch-events"];
        let read: Named = serde_json::from_value(serde_json::json!(names)).unwrap();
        assert_eq!(read, Named(Feature::WatchEvents.into()));
        let cbor = crate::codec::encode(&names).unwrap();
        assert_eq!(
            crate::codec::decode::<Named>(&cbor).unwrap(),
            Named(Feature::WatchEvents.into())
        );
    }
}

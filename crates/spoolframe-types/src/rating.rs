//! Whose a rating is: Rotten Tomatoes' Tomatometer, IMDb's, TMDB's
//! ([`RatingSource`], feature `spoolframe:rating-sources`), read from the
//! catalog's rating image (Plex's `ratingImage` and `audienceRatingImage`,
//! such as `rottentomatoes://image.rating.ripe`).

use serde::{Deserialize, Serialize};

/// Whose a rating is, and how it reads: its provider, Rotten Tomatoes'
/// verdict where it has one, and the catalog's own name for its image.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RatingSource {
    pub provider: RatingProvider,
    /// Rotten Tomatoes' verdict: fresh or rotten for critics, upright or
    /// spilled for the audience. `None` for a provider with none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub state: Option<RatingState>,
    /// The catalog's name for its image, as it gave it
    /// (`rottentomatoes://image.rating.ripe`): for an app to tell a
    /// provider or verdict this build does not know.
    pub image: String,
}

/// The names of `MergeFacts`' rating source fields on the wire: what a
/// server leaves out of a reply to a connection without
/// `spoolframe:rating-sources`.
pub const RATING_SOURCE_FIELDS: [&str; 2] = ["rating_source", "audience_rating_source"];

/// Who rated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum RatingProvider {
    RottenTomatoes,
    Imdb,
    Tmdb,
    /// One this build does not know: its `image` says which.
    #[serde(other)]
    Unknown,
}

/// Rotten Tomatoes' verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum RatingState {
    /// The critics' Tomatometer is 60% or more (Plex's `ripe`).
    Fresh,
    /// Certified Fresh (Plex's `certified`).
    CertifiedFresh,
    /// The critics' Tomatometer is under 60%.
    Rotten,
    /// The audience score is 60% or more (the full popcorn bucket).
    Upright,
    /// The audience score is under 60% (the tipped bucket).
    Spilled,
    /// One this build does not know: the source's `image` says which.
    #[serde(other)]
    Unknown,
}

/// How a provider's rating reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RatingScale {
    /// A percentage: Rotten Tomatoes'.
    Percent,
    /// Out of 10, to one place: IMDb's, TMDB's, and any not known.
    OutOf10,
}

impl RatingSource {
    /// The source an image name says (Plex's `ratingImage` or
    /// `audienceRatingImage`): `None` for an empty one. A provider or
    /// verdict this build does not know is `Unknown`, and `image` keeps
    /// the name as it was.
    pub fn from_image(image: &str) -> Option<Self> {
        let image = image.trim();
        if image.is_empty() {
            return None;
        }
        let (scheme, rest) = image.split_once("://").unwrap_or((image, ""));
        let provider = match scheme {
            "rottentomatoes" => RatingProvider::RottenTomatoes,
            "imdb" => RatingProvider::Imdb,
            "themoviedb" | "tmdb" => RatingProvider::Tmdb,
            _ => RatingProvider::Unknown,
        };
        let state = (provider == RatingProvider::RottenTomatoes).then(|| {
            match rest.strip_prefix("image.rating.").unwrap_or_default() {
                "ripe" => RatingState::Fresh,
                "certified" => RatingState::CertifiedFresh,
                "rotten" => RatingState::Rotten,
                "upright" => RatingState::Upright,
                "spilled" => RatingState::Spilled,
                _ => RatingState::Unknown,
            }
        });
        Some(Self {
            provider,
            state,
            image: image.to_owned(),
        })
    }

    /// How its ratings read.
    pub fn scale(&self) -> RatingScale {
        match self.provider {
            RatingProvider::RottenTomatoes => RatingScale::Percent,
            RatingProvider::Imdb | RatingProvider::Tmdb | RatingProvider::Unknown => {
                RatingScale::OutOf10
            }
        }
    }

    /// A rating of `tenths` (0 to 100, as `MergeFacts` has them) as this
    /// provider shows it: `"92%"` for Rotten Tomatoes' 9.2, `"7.9"` for
    /// IMDb's.
    pub fn display(&self, tenths: u16) -> String {
        match self.scale() {
            RatingScale::Percent => format!("{tenths}%"),
            RatingScale::OutOf10 => format!("{}.{}", tenths / 10, tenths % 10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(image: &str) -> (RatingProvider, Option<RatingState>) {
        let source = RatingSource::from_image(image).unwrap();
        assert_eq!(source.image, image);
        (source.provider, source.state)
    }

    #[test]
    fn each_known_plex_image_is_read() {
        use RatingProvider::*;
        use RatingState::*;
        let cases = [
            (
                "rottentomatoes://image.rating.ripe",
                (RottenTomatoes, Some(Fresh)),
            ),
            (
                "rottentomatoes://image.rating.certified",
                (RottenTomatoes, Some(CertifiedFresh)),
            ),
            (
                "rottentomatoes://image.rating.rotten",
                (RottenTomatoes, Some(Rotten)),
            ),
            (
                "rottentomatoes://image.rating.upright",
                (RottenTomatoes, Some(Upright)),
            ),
            (
                "rottentomatoes://image.rating.spilled",
                (RottenTomatoes, Some(Spilled)),
            ),
            ("imdb://image.rating", (Imdb, None)),
            ("themoviedb://image.rating", (Tmdb, None)),
        ];
        for (image, expected) in cases {
            assert_eq!(read(image), expected, "{image}");
        }
    }

    #[test]
    fn an_unknown_image_keeps_its_name() {
        assert_eq!(
            read("metacritic://image.rating"),
            (RatingProvider::Unknown, None)
        );
        assert_eq!(
            read("rottentomatoes://image.rating.verified"),
            (RatingProvider::RottenTomatoes, Some(RatingState::Unknown))
        );
        assert_eq!(RatingSource::from_image(""), None);
    }

    #[test]
    fn rotten_tomatoes_shows_a_percentage_and_the_rest_out_of_ten() {
        let rt = RatingSource::from_image("rottentomatoes://image.rating.ripe").unwrap();
        let imdb = RatingSource::from_image("imdb://image.rating").unwrap();
        assert_eq!(
            (rt.display(92), imdb.display(79), imdb.display(100)),
            ("92%".to_owned(), "7.9".to_owned(), "10.0".to_owned())
        );
    }

    /// The names a server strips are the fields' own.
    #[test]
    fn the_stripped_names_are_merge_facts_fields() {
        let source = RatingSource::from_image("imdb://image.rating");
        let facts = crate::MergeFacts {
            rating_source: source.clone(),
            audience_rating_source: source,
            ..Default::default()
        };
        let json = serde_json::to_value(&facts).unwrap();
        for name in RATING_SOURCE_FIELDS {
            assert!(json.get(name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_wire_names_are_fixed() {
        let source = RatingSource::from_image("rottentomatoes://image.rating.certified").unwrap();
        assert_eq!(
            serde_json::to_value(&source).unwrap(),
            serde_json::json!({
                "provider": "rotten_tomatoes",
                "state": "certified_fresh",
                "image": "rottentomatoes://image.rating.certified"
            })
        );
        let newer: RatingSource = serde_json::from_value(serde_json::json!({
            "provider": "letterboxd",
            "state": "glowing",
            "image": "letterboxd://image.rating"
        }))
        .unwrap();
        assert_eq!(
            (newer.provider, newer.state),
            (RatingProvider::Unknown, Some(RatingState::Unknown))
        );
    }
}

//! What a search covers and how its results are grouped (P3-09), and the
//! people it finds.

use serde::{Deserialize, Serialize};

use crate::id::PersonId;

/// What a search covers: everything, or one kind of thing. Music is one
/// scope, with artists, albums and tracks as groups inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    All,
    Movies,
    Shows,
    Episodes,
    Music,
}

impl SearchScope {
    /// The groups a search in this scope returns, in the order they are
    /// shown: episodes last.
    pub fn kinds(self) -> &'static [SearchKind] {
        use SearchKind::*;
        match self {
            SearchScope::All => &[Shows, Movies, People, Artists, Albums, Tracks, Episodes],
            SearchScope::Movies => &[Movies],
            SearchScope::Shows => &[Shows],
            SearchScope::Episodes => &[Episodes],
            SearchScope::Music => &[Artists, Albums, Tracks],
        }
    }
}

/// One group of search results. People, artists, albums and tracks have no
/// scope of their own; a search for one kind ("See all") names it.
///
/// Ordered as the groups are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Shows,
    Movies,
    People,
    Artists,
    Albums,
    Tracks,
    Episodes,
}

/// What a person did in the items they are found for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonRole {
    Actor,
    Director,
}

/// An actor or director a search found. Their items are listed by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub id: PersonId,
    pub name: String,
    pub role: PersonRole,
    /// How many items in the library they are in, as the catalog counts
    /// them; listing them may find a few more or fewer.
    pub count: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Namespace;

    #[test]
    fn scopes_and_kinds_are_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&SearchScope::Episodes).unwrap(),
            r#""episodes""#
        );
        assert_eq!(
            serde_json::to_string(&SearchKind::People).unwrap(),
            r#""people""#
        );
    }

    #[test]
    fn music_holds_artists_albums_and_tracks() {
        assert_eq!(
            SearchScope::Music.kinds(),
            [SearchKind::Artists, SearchKind::Albums, SearchKind::Tracks]
        );
    }

    #[test]
    fn everything_ends_with_episodes() {
        assert_eq!(SearchScope::All.kinds().last(), Some(&SearchKind::Episodes));
    }

    #[test]
    fn a_person_round_trips() {
        let person = Person {
            id: PersonId::new(Namespace::Plex, "actor-134469").unwrap(),
            name: "Keanu Reeves".into(),
            role: PersonRole::Actor,
            count: Some(9),
        };
        let json = serde_json::to_value(&person).unwrap();
        assert_eq!(json["id"], "plex:actor-134469");
        assert_eq!(json["role"], "actor");
        assert_eq!(serde_json::from_value::<Person>(json).unwrap(), person);
    }
}

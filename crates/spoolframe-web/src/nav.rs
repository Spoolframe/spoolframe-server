//! The admin's sidebar, and the phone's drawer that repeats it: its
//! top-level sections in order, apart from the page. Each section's own
//! sub-pages (Users and devices' Invites, Server's Activity and the rest)
//! are drawn under it by the app. Host-compiled, so `cargo test` covers
//! it.

/// A top-level section of the admin's sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Overview,
    UsersAndDevices,
    /// The servers paired with this one (P8-01): a section of its own,
    /// not a page of Users and devices, as it is about other servers.
    Federation,
    Server,
    Plex,
    MyProfile,
    MyPlaylists,
}

impl Section {
    /// What the sidebar calls it.
    pub fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::UsersAndDevices => "Users and devices",
            Section::Federation => "Federation",
            Section::Server => "Server",
            Section::Plex => "Plex",
            Section::MyProfile => "My profile",
            Section::MyPlaylists => "My playlists",
        }
    }

    /// Whether a divider sets it apart from the sections above: the
    /// admin's own pages, after the server's.
    pub fn divided(self) -> bool {
        self == Section::MyProfile
    }
}

/// The sidebar's sections, in order. Federation is listed only while the
/// server has it on (`federation_on`, experimental).
pub fn sections(federation_on: bool) -> Vec<Section> {
    let mut sections = vec![Section::Overview, Section::UsersAndDevices];
    if federation_on {
        sections.push(Section::Federation);
    }
    sections.extend([
        Section::Server,
        Section::Plex,
        Section::MyProfile,
        Section::MyPlaylists,
    ]);
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With federation on, it is a top-level section of its own, after
    /// Users and devices.
    #[test]
    fn federation_is_a_section_of_its_own_while_it_is_on() {
        assert_eq!(
            sections(true),
            [
                Section::Overview,
                Section::UsersAndDevices,
                Section::Federation,
                Section::Server,
                Section::Plex,
                Section::MyProfile,
                Section::MyPlaylists,
            ]
        );
        assert_eq!(Section::Federation.label(), "Federation");
    }

    /// Off, the sidebar has no Federation.
    #[test]
    fn federation_is_not_listed_while_it_is_off() {
        assert!(!sections(false).contains(&Section::Federation));
        assert_eq!(sections(false).len(), sections(true).len() - 1);
    }

    /// Only the admin's own pages are set apart.
    #[test]
    fn my_profile_alone_is_divided_from_the_server_s_sections() {
        let divided: Vec<_> = sections(true).into_iter().filter(|s| s.divided()).collect();
        assert_eq!(divided, [Section::MyProfile]);
    }
}

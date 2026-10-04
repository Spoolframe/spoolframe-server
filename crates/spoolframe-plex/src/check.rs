//! `plex check-paths`: map every media file in the PMS and report, per
//! library, which ones this host can read (P1-04). A bad mapping shows up
//! when the admin sets it up, not when somebody presses play.

use std::path::{Path, PathBuf};

use spoolframe_catalog::CatalogError;
use spoolframe_types::LibraryKind;

use crate::wire::{Directory, SectionKey};
use crate::{Call, PlexCatalog, convert, paths};

/// How many failing paths a report keeps per kind of failure.
const EXAMPLES: usize = 3;

/// How many items one listing request asks for.
const PAGE: u32 = 200;

/// Failures of one kind: how many, and a few of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Failures {
    pub count: u64,
    /// `"<plex path> -> <local path>: <reason>"`, at most a few.
    pub examples: Vec<String>,
}

impl Failures {
    fn add(&mut self, example: impl FnOnce() -> String) {
        self.count += 1;
        if self.examples.len() < EXAMPLES {
            self.examples.push(example());
        }
    }
}

/// The result for one library.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryPaths {
    pub library: String,
    /// The library's folders, as Plex sees them.
    pub roots: Vec<PathBuf>,
    /// The Plex paths of the first few files that cannot be read, for
    /// suggesting a mapping ([`crate::paths::suggest`]).
    pub failed: Vec<PathBuf>,
    /// Files that are readable after mapping.
    pub ok: u64,
    /// Unreadable files that no mapping matched: probably a mapping is
    /// missing.
    pub no_mapping: Failures,
    /// Unreadable files that a mapping did match: probably the mapping is
    /// wrong, or the mount is missing.
    pub unreadable: Failures,
}

impl LibraryPaths {
    pub fn is_ok(&self) -> bool {
        self.no_mapping.count == 0 && self.unreadable.count == 0
    }
}

/// Which items to check in each library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every item: the `check-paths` command.
    All,
    /// The first few items: the check at startup.
    Sample(u32),
}

/// The Plex `type` number of a library's playable items: movies, episodes
/// or tracks. Listing a section with it returns those items directly, with
/// their media, instead of walking shows and albums.
fn leaf_type(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Movies => "1",
        LibraryKind::Shows => "4",
        LibraryKind::Music => "10",
    }
}

impl PlexCatalog {
    /// Check the media paths of every served library, or only of the one
    /// titled `library`.
    pub async fn check_paths(
        &self,
        library: Option<&str>,
        scope: Scope,
    ) -> Result<Vec<LibraryPaths>, CatalogError> {
        let call = Call::get("/library/sections");
        let sections = self.fetch(&call).await?.directories;
        let mut reports = Vec::new();
        for dir in &sections {
            if library.is_some_and(|wanted| wanted != dir.title)
                || !self.serves(Some(&SectionKey(dir.key.clone())))
            {
                continue;
            }
            let Some(lib) = convert::library(dir).map_err(|e| self.converted(&call, e))? else {
                continue;
            };
            reports.push(self.check_section(dir, lib.kind, scope).await?);
        }
        if let Some(wanted) = library
            && reports.is_empty()
        {
            return Err(CatalogError::NotFound(format!("Plex library {wanted:?}")));
        }
        Ok(reports)
    }

    async fn check_section(
        &self,
        dir: &Directory,
        kind: LibraryKind,
        scope: Scope,
    ) -> Result<LibraryPaths, CatalogError> {
        let mut report = LibraryPaths {
            library: dir.title.clone(),
            roots: dir
                .locations
                .iter()
                .map(|l| PathBuf::from(&l.path))
                .collect(),
            ..LibraryPaths::default()
        };
        let limit = match scope {
            Scope::All => u32::MAX,
            Scope::Sample(n) => n,
        };
        let mut offset = 0u32;
        while offset < limit {
            let size = PAGE.min(limit - offset);
            let mut call = Call::get(format!("/library/sections/{}/all", dir.key))
                .about(format!("Plex library {:?}", dir.title));
            call.query = vec![("type", leaf_type(kind))];
            call.headers = vec![
                ("X-Plex-Container-Start", offset.to_string()),
                ("X-Plex-Container-Size", size.to_string()),
            ];
            let page = self.fetch(&call).await?;
            for m in &page.metadata {
                for file in convert::media_files(m) {
                    self.check_file(&mut report, Path::new(file));
                }
            }
            let got = u32::try_from(page.metadata.len()).unwrap_or(u32::MAX);
            offset = offset.saturating_add(got);
            let total = page.total_size.unwrap_or(offset);
            if got < size || offset >= total {
                break;
            }
        }
        Ok(report)
    }

    fn check_file(&self, report: &mut LibraryPaths, file: &Path) {
        let mapped = self.paths.map(Some(&report.library), file);
        let Some(reason) = paths::unreadable(&mapped.local) else {
            report.ok += 1;
            return;
        };
        if report.failed.len() < EXAMPLES {
            report.failed.push(file.to_owned());
        }
        let example = || format!("{} -> {}: {reason}", file.display(), mapped.local.display());
        match mapped.mapping {
            None => report.no_mapping.add(example),
            Some(_) => report.unreadable.add(example),
        }
    }
}

//! `plex check-paths`: map every media file in the PMS and report, per
//! library, which ones this host can read (P1-04). A bad mapping shows up
//! when the admin sets it up, not when somebody presses play.

use std::path::{Path, PathBuf};

use spoolframe_catalog::{CatalogError, FileCheck, LibraryCheck, MissingFile, Row};
use spoolframe_types::{LibraryId, LibraryKind};

use crate::wire::{Directory, Metadata, SectionKey};
use crate::{Call, PlexCatalog, convert};

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
    /// The library's section key (P3-16).
    pub section: String,
    /// Every unreadable file, with its item, in Plex's order: what an
    /// admin's "Missing files" filter lists (P3-16). As many as
    /// `no_mapping.count + unreadable.count`, but for an item Plex sends
    /// in a shape that does not convert, which is counted and logged only.
    pub missing: Vec<MissingFile>,
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

    /// What a check of every served library reads besides the files
    /// (P3-18): the path mappings, and each served section's key, title
    /// and folders as Plex has them now, by key. The same text while those
    /// are the same: the order libraries are listed in, and their
    /// Spoolframe names, are not in it.
    pub async fn paths_basis(&self) -> Result<String, CatalogError> {
        use std::fmt::Write as _;
        let mut basis = String::new();
        for m in self.paths.mappings() {
            let _ = writeln!(basis, "map {:?} {:?} {:?}", m.library, m.from, m.to);
        }
        let mut sections: Vec<_> = self
            .sections()
            .await?
            .into_iter()
            .filter(|s| s.kind.is_some() && self.serves(Some(&SectionKey(s.key.clone()))))
            .collect();
        sections.sort_by(|a, b| a.key.cmp(&b.key));
        for s in sections {
            let _ = writeln!(basis, "section {:?} {:?} {:?}", s.key, s.title, s.roots);
        }
        Ok(basis)
    }

    async fn check_section(
        &self,
        dir: &Directory,
        kind: LibraryKind,
        scope: Scope,
    ) -> Result<LibraryPaths, CatalogError> {
        let mut report = LibraryPaths {
            library: dir.title.clone(),
            section: dir.key.clone(),
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
            // With the agents' ids, for the rows of the files that fail.
            call.query = vec![("type", leaf_type(kind)), ("includeGuids", "1")];
            call.headers = vec![
                ("X-Plex-Container-Start", offset.to_string()),
                ("X-Plex-Container-Size", size.to_string()),
            ];
            let page = self.fetch(&call).await?;
            for m in &page.metadata {
                self.check_item(&mut report, m);
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

    /// Check each file of item `m`; one that fails is counted, and kept
    /// with the item's row.
    fn check_item(&self, report: &mut LibraryPaths, m: &Metadata) {
        let checks: Vec<FileCheck> = convert::media_files(m)
            .into_iter()
            .map(|file| self.paths.check(Some(&report.library), Path::new(file)))
            .collect();
        let mut failing = Vec::new();
        for checked in &checks {
            let Some(reason) = &checked.problem else {
                report.ok += 1;
                continue;
            };
            if report.failed.len() < EXAMPLES {
                report.failed.push(checked.reported.clone());
            }
            let example = || {
                format!(
                    "{} -> {}: {reason}",
                    checked.reported.display(),
                    checked.local.display()
                )
            };
            match checked.mapping {
                None => report.no_mapping.add(example),
                Some(_) => report.unreadable.add(example),
            }
            failing.push(checked.clone());
        }
        if failing.is_empty() {
            return;
        }
        let item = match convert::item(m) {
            Ok(item) => item,
            Err(e) => {
                tracing::warn!(
                    library = report.library,
                    rating_key = %m.rating_key,
                    error = e.message,
                    "a missing file's item does not convert; counted, not listed"
                );
                return;
            }
        };
        let row = Row {
            item,
            guid: m.guid.clone(),
            agents: m.guids.iter().map(|g| g.id.clone()).collect(),
            added_at: m.added_at,
            updated_at: m.updated_at,
            children: m.child_count,
            leaves: m.leaf_count,
            files: checks,
        };
        for file in failing {
            report.missing.push(MissingFile {
                row: row.clone(),
                file,
            });
        }
    }

    /// Every media file of library `id`, served, checked (P3-16): the
    /// check `check-paths` makes of it.
    pub async fn check_library(&self, id: &LibraryId) -> Result<LibraryCheck, CatalogError> {
        let key = crate::plex_key(id.namespace(), id.key(), id)?;
        if !self.serves(Some(&SectionKey(key.to_owned()))) {
            return Err(CatalogError::NotServed(id.to_string()));
        }
        let call = Call::get("/library/sections");
        let sections = self.fetch(&call).await?.directories;
        let dir = sections
            .iter()
            .find(|d| d.key == key)
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        let lib = convert::library(dir)
            .map_err(|e| self.converted(&call, e))?
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        let report = self.check_section(dir, lib.kind, Scope::All).await?;
        Ok(LibraryCheck {
            ok: report.ok,
            missing: report.missing,
        })
    }
}

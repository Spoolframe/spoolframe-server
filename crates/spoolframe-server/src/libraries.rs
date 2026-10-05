//! Choosing the libraries clients see, and the Plex servers they come
//! from, while the server runs (P3-11): for Server › Libraries, the add
//! library flow and the setup wizard.
//!
//! Every change is one to `libraries` or `plex.*` in the runtime layer,
//! made through [`RuntimeSettings`], so it is checked as a config file is,
//! refused where the locked file sets it, and the catalog follows at once.
//! A token is kept as a secret, never in a layer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use spoolframe_admin_api::{
    LibrariesOverview, LibraryEntry, LibraryPathCheck, LibraryStatus, PathMappingEntry,
    PlexServerEntry, PlexStatus, SyncDirection, UnsupportedSection,
};
use spoolframe_catalog::CatalogError;
use spoolframe_plex::{LibraryPaths, PlexCatalog, Scope};
use spoolframe_types::{LibraryId, Namespace};

use crate::admin::PATH_SAMPLE;
use crate::catalog::{Composite, ids};
use crate::config::layers::{Layers, SettingError, plex_token_secret};
use crate::config::tree::{Table, Value};
use crate::config::{
    self, ConfigError, FIRST_PLEX_SERVER, LibraryConfig, PathMappingConfig, PlexLibraryConfig,
    PlexServerConfig, ServerConfig, library_kind_name,
};
use crate::runtime::{ChangeError, RuntimeSettings};

/// Why a library or Plex server change was not made.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("there is no library {0}")]
    NoSuchLibrary(String),
    #[error("there is no Plex server {0}")]
    NoSuchServer(String),
    /// A change that cannot be made; the message says why.
    #[error("{0}")]
    Invalid(String),
    /// A Plex server that had to answer did not.
    #[error("{0}")]
    Unavailable(String),
    #[error("this server is not set up: no Plex server is configured")]
    NotSetUp,
    #[error(transparent)]
    Change(#[from] ChangeError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// The id clients know a library by.
pub fn library_id(source: &PlexLibraryConfig) -> String {
    match LibraryId::new(Namespace::Plex, source.section.as_str()) {
        Ok(local) => ids::library(&source.server, local).to_string(),
        Err(_) => format!("plex:{}", source.section),
    }
}

fn string(s: &str) -> Value {
    Value::String(s.to_owned())
}

fn object(members: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        members
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<Table>(),
    )
}

fn library_value(l: &LibraryConfig) -> Value {
    let mut source = vec![
        ("server", string(&l.plex.server)),
        ("section", string(&l.plex.section)),
    ];
    // Left out until an admin chooses, so the default keeps applying.
    if let Some(sync) = &l.plex.sync {
        source.push(("sync", string(sync)));
    }
    object([
        ("name", string(&l.name)),
        ("kind", string(&l.kind)),
        ("plex", object(source)),
    ])
}

fn libraries_value(list: &[LibraryConfig]) -> Value {
    Value::Array(list.iter().map(library_value).collect())
}

fn mapping_value(m: &PathMappingConfig) -> Value {
    let mut members = vec![
        ("from", string(&m.from.display().to_string())),
        ("to", string(&m.to.display().to_string())),
    ];
    if let Some(library) = &m.library {
        members.push(("library", string(library)));
    }
    object(members)
}

fn mappings_value(list: &[PathMappingConfig]) -> Value {
    Value::Array(list.iter().map(mapping_value).collect())
}

fn server_value(s: &PlexServerConfig) -> Value {
    let mut members = vec![
        ("id", string(&s.id)),
        ("url", string(&s.url)),
        ("path_mapping", mappings_value(&s.path_mapping)),
    ];
    if let Some(name) = &s.name {
        members.push(("name", string(name)));
    }
    if let Some(file) = &s.token_file {
        members.push(("token_file", string(&file.display().to_string())));
    }
    object(members)
}

fn servers_value(list: &[PlexServerConfig]) -> Value {
    Value::Array(list.iter().map(server_value).collect())
}

/// Path mappings as the API passes them, as the config has them.
pub fn mappings(entries: Vec<PathMappingEntry>) -> Vec<PathMappingConfig> {
    entries
        .into_iter()
        .map(|m| PathMappingConfig {
            library: m.library,
            from: m.from.into(),
            to: m.to.into(),
        })
        .collect()
}

fn entries(mappings: &[PathMappingConfig]) -> Vec<PathMappingEntry> {
    mappings
        .iter()
        .map(|m| PathMappingEntry {
            library: m.library.clone(),
            from: m.from.display().to_string(),
            to: m.to.display().to_string(),
        })
        .collect()
}

/// An id for a server Plex calls `name`: lower-case, dashes for the rest,
/// starting with a letter, and not one of `taken`.
fn server_id(name: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let mut slug = slug.trim_end_matches('-').to_owned();
    if !slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        slug = format!("pms-{slug}").trim_end_matches('-').to_owned();
    }
    slug.truncate(24);
    let slug = slug.trim_end_matches('-').to_owned();
    let free = |id: &str| id != FIRST_PLEX_SERVER && !taken.iter().any(|t| t == id);
    if free(&slug) {
        return slug;
    }
    (2..)
        .map(|n| format!("{slug}-{n}"))
        .find(|id| free(id))
        .expect("some number is free")
}

/// The URL as the catalog keeps it, for telling whether two are the same
/// server.
fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// A Plex server now, as `plexStatus` shows it: reachable or not, and
/// every section, those Spoolframe cannot serve apart. Down is an answer,
/// not an error.
pub async fn plex_status(plex: &PlexCatalog) -> PlexStatus {
    let down = |e: CatalogError| PlexStatus {
        reachable: false,
        error: Some(e.to_string()),
        server_name: None,
        version: None,
        url: Some(plex.base_url().to_owned()),
        libraries: Vec::new(),
        unsupported: Vec::new(),
    };
    let (server_name, version) = match plex.identity().await {
        Ok(identity) => identity,
        Err(e) => return down(e),
    };
    let sections = match plex.sections().await {
        Ok(sections) => sections,
        Err(e) => return down(e),
    };
    let mut libraries = Vec::new();
    let mut unsupported = Vec::new();
    for section in sections {
        let Some(kind) = section.kind else {
            unsupported.push(UnsupportedSection {
                title: section.title,
                plex_type: section.plex_type,
            });
            continue;
        };
        let items = match plex.section_size(&section.key).await {
            Ok(items) => items,
            Err(e) => return down(e),
        };
        libraries.push(LibraryStatus {
            section: section.key,
            title: section.title,
            kind,
            items,
            library: None,
        });
    }
    PlexStatus {
        reachable: true,
        error: None,
        server_name,
        version,
        url: Some(plex.base_url().to_owned()),
        libraries,
        unsupported,
    }
}

/// A sample check of one library's files, with a mapping suggested from
/// `bases` where they fail.
pub fn path_check(report: LibraryPaths, bases: &[PathBuf]) -> LibraryPathCheck {
    let suggestion = report
        .failed
        .first()
        .and_then(|file| spoolframe_plex::paths::suggest(file, bases))
        .map(|m| PathMappingEntry {
            library: Some(report.library.clone()),
            from: m.from.display().to_string(),
            to: m.to.display().to_string(),
        });
    LibraryPathCheck {
        checked: report.ok + report.no_mapping.count + report.unreadable.count,
        ok: report.ok,
        failures: report
            .no_mapping
            .examples
            .into_iter()
            .chain(report.unreadable.examples)
            .collect(),
        roots: report
            .roots
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        library: report.library,
        suggestion,
    }
}

/// The libraries now: as chosen, or (not chosen yet) every library of the
/// first server, as it is served.
fn served_now(config: &ServerConfig, first: &PlexStatus) -> Vec<LibraryConfig> {
    match &config.libraries {
        Some(chosen) => chosen.clone(),
        None => first
            .libraries
            .iter()
            .map(|l| LibraryConfig {
                name: l.title.clone(),
                kind: library_kind_name(l.kind).to_owned(),
                plex: PlexLibraryConfig {
                    server: FIRST_PLEX_SERVER.to_owned(),
                    section: l.section.clone(),
                    sync: None,
                },
            })
            .collect(),
    }
}

/// Mark each section of `status` with the library it is, if any.
fn mark(status: &mut PlexStatus, server: &str, served: &[LibraryConfig]) {
    for section in &mut status.libraries {
        section.library = served
            .iter()
            .find(|l| l.plex.server == server && l.plex.section == section.section)
            .map(|l| library_id(&l.plex));
    }
}

impl RuntimeSettings {
    fn config_now(&self) -> Result<(Layers, ServerConfig), LibraryError> {
        let layers = self.layers();
        let config = layers.config()?;
        Ok((layers, config))
    }

    fn composite(&self) -> Result<std::sync::Arc<Composite>, LibraryError> {
        self.catalog().composite().ok_or(LibraryError::NotSetUp)
    }

    /// The libraries as chosen, or, before any is, every library the first
    /// server serves now: what the first change starts from.
    async fn chosen(&self, config: &ServerConfig) -> Result<Vec<LibraryConfig>, LibraryError> {
        if let Some(chosen) = &config.libraries {
            return Ok(chosen.clone());
        }
        let composite = self.composite()?;
        let first = composite.first();
        let sections = first.catalog.sections().await.map_err(|e| {
            LibraryError::Unavailable(format!(
                "cannot list {}'s libraries to choose from: {e}",
                first.name
            ))
        })?;
        Ok(sections
            .into_iter()
            .filter_map(|s| {
                Some(LibraryConfig {
                    name: s.title,
                    kind: library_kind_name(s.kind?).to_owned(),
                    plex: PlexLibraryConfig {
                        server: FIRST_PLEX_SERVER.to_owned(),
                        section: s.key,
                        sync: None,
                    },
                })
            })
            .collect())
    }

    fn save_libraries(&self, list: &[LibraryConfig], by: &str) -> Result<(), LibraryError> {
        self.change_keys(
            "libraries",
            by,
            vec![("libraries", Some(libraries_value(list)))],
        )?;
        Ok(())
    }

    fn position(list: &[LibraryConfig], id: &str) -> Result<usize, LibraryError> {
        list.iter()
            .position(|l| library_id(&l.plex) == id)
            .ok_or_else(|| LibraryError::NoSuchLibrary(id.to_owned()))
    }

    /// Add section `section` of Plex server `server` as the last library,
    /// called `name` or Plex's name; its id.
    pub async fn library_add(
        &self,
        server: &str,
        section: &str,
        name: Option<&str>,
        by: &str,
    ) -> Result<String, LibraryError> {
        let (_, config) = self.config_now()?;
        let composite = self.composite()?;
        let source = composite
            .server(server)
            .ok_or_else(|| LibraryError::NoSuchServer(server.to_owned()))?;
        let sections = source.catalog.sections().await.map_err(|e| {
            LibraryError::Unavailable(format!("{} does not answer: {e}", source.name))
        })?;
        let found = sections
            .into_iter()
            .find(|s| s.key == section)
            .ok_or_else(|| {
                LibraryError::Invalid(format!("{} has no library section {section}", source.name))
            })?;
        let Some(kind) = found.kind else {
            return Err(LibraryError::Invalid(format!(
                "{} holds {}, which Spoolframe does not serve",
                found.title,
                found.plex_type.as_deref().unwrap_or("something else")
            )));
        };
        let mut list = self.chosen(&config).await?;
        if list
            .iter()
            .any(|l| l.plex.server == server && l.plex.section == section)
        {
            return Err(LibraryError::Invalid(format!(
                "{} on {} is a library already",
                found.title, source.name
            )));
        }
        // Spoolframe plays the files itself (P1-04): a section none of
        // whose sample files is here, under the saved mappings, would play
        // nothing, so it is not added. The add flow says the same before
        // asking; this is for every other caller.
        let sample = source
            .catalog
            .unrestricted()
            .serving(Some(HashSet::from([section.to_owned()])))
            .check_paths(None, Scope::Sample(PATH_SAMPLE))
            .await
            .map_err(|e| {
                LibraryError::Unavailable(format!(
                    "cannot check {}'s files on {}: {e}",
                    found.title, source.name
                ))
            })?;
        if let Some(why) = sample
            .into_iter()
            .find_map(|report| path_check(report, &[]).refusal())
        {
            return Err(LibraryError::Invalid(why));
        }
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(&found.title)
            .to_owned();
        let added = LibraryConfig {
            name,
            kind: library_kind_name(kind).to_owned(),
            plex: PlexLibraryConfig {
                server: server.to_owned(),
                section: section.to_owned(),
                sync: None,
            },
        };
        let id = library_id(&added.plex);
        tracing::info!(by, library = id, name = added.name, "library added");
        list.push(added);
        self.save_libraries(&list, by)?;
        Ok(id)
    }

    /// Call library `id` `name`.
    pub async fn library_rename(&self, id: &str, name: &str, by: &str) -> Result<(), LibraryError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(LibraryError::Invalid("a library needs a name".into()));
        }
        let (_, config) = self.config_now()?;
        let mut list = self.chosen(&config).await?;
        let at = Self::position(&list, id)?;
        list[at].name = name.to_owned();
        self.save_libraries(&list, by)
    }

    /// Put library `id` at `position`, or last if that is past the end.
    pub async fn library_move(
        &self,
        id: &str,
        position: usize,
        by: &str,
    ) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        let mut list = self.chosen(&config).await?;
        let at = Self::position(&list, id)?;
        let library = list.remove(at);
        list.insert(position.min(list.len()), library);
        self.save_libraries(&list, by)
    }

    /// Sync library `id`'s watch state with linked Plex accounts this way
    /// (P3-12).
    pub async fn library_set_sync(
        &self,
        id: &str,
        sync: SyncDirection,
        by: &str,
    ) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        let mut list = self.chosen(&config).await?;
        let at = Self::position(&list, id)?;
        list[at].plex.sync = Some(sync.config_name().to_owned());
        self.save_libraries(&list, by)?;
        tracing::info!(
            by,
            library = id,
            sync = sync.config_name(),
            "library watch sync changed"
        );
        Ok(())
    }

    /// Stop serving library `id`; its watch state is kept.
    pub async fn library_remove(&self, id: &str, by: &str) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        let mut list = self.chosen(&config).await?;
        let at = Self::position(&list, id)?;
        let removed = list.remove(at);
        self.save_libraries(&list, by)?;
        tracing::info!(
            by,
            library = id,
            name = removed.name,
            "library removed; its watch state is kept"
        );
        Ok(())
    }

    /// Add the Plex server at `url`, with `token` or else the first
    /// server's; its id.
    pub async fn plex_server_add(
        &self,
        url: &str,
        token: Option<&str>,
        by: &str,
    ) -> Result<String, LibraryError> {
        let parsed = config::plex_url(url.trim(), "url")
            .map_err(|e| LibraryError::Invalid(e.to_string()))?;
        let url = parsed.as_str().trim_end_matches('/').to_owned();
        let token = match token.map(str::trim).filter(|t| !t.is_empty()) {
            Some(token) => Some(token.to_owned()),
            None => self.plex_token(),
        };
        let probe = PlexCatalog::new(parsed.clone(), token.clone())
            .map_err(|e| LibraryError::Unavailable(e.to_string()))?;
        let (name, _) = probe
            .identity()
            .await
            .map_err(|e| LibraryError::Unavailable(e.to_string()))?;
        let name = name.unwrap_or_else(|| parsed.host_str().unwrap_or("Plex").to_owned());
        let (layers, config) = self.config_now()?;
        let Some(plex) = &config.plex else {
            // The first: as the wizard's Plex step makes it.
            if let Some(token) = &token {
                self.set_server_token(FIRST_PLEX_SERVER, Some(token), by)?;
            }
            self.change_keys(
                "plex",
                by,
                vec![
                    ("plex.url", Some(string(&url))),
                    ("plex.name", Some(string(&name))),
                ],
            )?;
            tracing::info!(by, url, name, "the first Plex server added");
            return Ok(FIRST_PLEX_SERVER.to_owned());
        };
        let servers = plex.all_servers();
        if let Some(there) = servers.iter().find(|s| same_url(&s.url, &url)) {
            return Err(LibraryError::Invalid(format!(
                "{url} is added already, as {}",
                there.display_name()
            )));
        }
        let taken: Vec<String> = servers.iter().map(|s| s.id.clone()).collect();
        let id = server_id(&name, &taken);
        // Refused before anything is written when a locked file lists the
        // servers.
        if layers.is_locked("plex.servers") {
            return Err(ChangeError::Setting(SettingError::Locked {
                key: "plex.servers".into(),
                path: layers.file_path().map(Into::into).unwrap_or_default(),
            })
            .into());
        }
        if let Some(token) = &token {
            self.secrets()
                .set(&plex_token_secret(&id), token)
                .map_err(ChangeError::Write)?;
        }
        let mut more = plex.servers.clone();
        more.push(PlexServerConfig {
            id: id.clone(),
            name: Some(name.clone()),
            url: url.clone(),
            token_file: None,
            path_mapping: Vec::new(),
        });
        self.change_keys(
            "plex.servers",
            by,
            vec![("plex.servers", Some(servers_value(&more)))],
        )?;
        tracing::info!(by, server = id, url, name, "Plex server added");
        Ok(id)
    }

    /// The servers after the first, and where `id` is among them; `None`
    /// for the first.
    fn locate(
        config: &ServerConfig,
        id: &str,
    ) -> Result<(Vec<PlexServerConfig>, Option<usize>), LibraryError> {
        let plex = config.plex.as_ref().ok_or(LibraryError::NotSetUp)?;
        if id == FIRST_PLEX_SERVER {
            return Ok((plex.servers.clone(), None));
        }
        let at = plex
            .servers
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| LibraryError::NoSuchServer(id.to_owned()))?;
        Ok((plex.servers.clone(), Some(at)))
    }

    /// Change where Plex server `id` is reached, or what it is called.
    pub fn plex_server_update(
        &self,
        id: &str,
        url: Option<&str>,
        name: Option<&str>,
        by: &str,
    ) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        let (mut servers, at) = Self::locate(&config, id)?;
        let url = url.map(str::trim).filter(|u| !u.is_empty());
        let name = name.map(str::trim).filter(|n| !n.is_empty());
        let mut keys = Vec::new();
        match at {
            None => {
                if let Some(url) = url {
                    keys.push(("plex.url", Some(string(url))));
                }
                if let Some(name) = name {
                    keys.push(("plex.name", Some(string(name))));
                }
            }
            Some(at) => {
                if let Some(url) = url {
                    servers[at].url = url.to_owned();
                }
                if let Some(name) = name {
                    servers[at].name = Some(name.to_owned());
                }
                keys.push(("plex.servers", Some(servers_value(&servers))));
            }
        }
        if keys.is_empty() {
            return Ok(());
        }
        self.change_keys("plex", by, keys)?;
        Ok(())
    }

    /// Keep `token` as Plex server `id`'s.
    pub fn plex_server_set_token(
        &self,
        id: &str,
        token: &str,
        by: &str,
    ) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        Self::locate(&config, id)?;
        let token = token.trim();
        if token.is_empty() {
            return Err(ChangeError::Secret {
                key: "token".into(),
            }
            .into());
        }
        self.set_server_token(id, Some(token), by)?;
        Ok(())
    }

    /// Set Plex server `id`'s path mappings.
    pub fn plex_server_set_paths(
        &self,
        id: &str,
        mappings: Vec<PathMappingConfig>,
        by: &str,
    ) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        let (mut servers, at) = Self::locate(&config, id)?;
        let key = match at {
            None => ("plex.path_mapping", Some(mappings_value(&mappings))),
            Some(at) => {
                servers[at].path_mapping = mappings;
                ("plex.servers", Some(servers_value(&servers)))
            }
        };
        self.change_keys("path mappings", by, vec![key])?;
        Ok(())
    }

    /// Remove Plex server `id`, its libraries and its token.
    pub fn plex_server_remove(&self, id: &str, by: &str) -> Result<(), LibraryError> {
        let (_, config) = self.config_now()?;
        let (mut servers, at) = Self::locate(&config, id)?;
        let Some(at) = at else {
            return Err(LibraryError::Invalid(
                "the first Plex server cannot be removed; change its address or token instead"
                    .into(),
            ));
        };
        let removed = servers.remove(at);
        // Its libraries first: each change is checked, and a library of a
        // server that is gone is refused.
        let mut keys = Vec::new();
        if let Some(chosen) = &config.libraries {
            let kept: Vec<LibraryConfig> = chosen
                .iter()
                .filter(|l| l.plex.server != id)
                .cloned()
                .collect();
            keys.push(("libraries", Some(libraries_value(&kept))));
        }
        keys.push(("plex.servers", Some(servers_value(&servers))));
        self.change_keys("plex.servers", by, keys)?;
        if let Err(e) = self.secrets().remove(&plex_token_secret(id)) {
            tracing::warn!(server = id, error = %e, "cannot forget the removed server's token");
        }
        tracing::info!(
            by,
            server = id,
            name = removed.display_name(),
            "Plex server removed with its libraries; their watch state is kept"
        );
        Ok(())
    }

    /// The libraries and Plex servers, each server asked how it is doing
    /// (all at once); `bases` are where path mappings are looked for.
    pub async fn libraries_overview(
        &self,
        bases: &[PathBuf],
    ) -> Result<LibrariesOverview, LibraryError> {
        let (layers, config) = self.config_now()?;
        let locked = layers.is_locked("libraries");
        let (Some(plex), Some(composite)) = (&config.plex, self.catalog().composite()) else {
            return Ok(LibrariesOverview {
                libraries: Vec::new(),
                servers: Vec::new(),
                chosen: config.libraries.is_some(),
                locked,
            });
        };
        let mut tasks = tokio::task::JoinSet::new();
        for (i, source) in composite.servers().iter().enumerate() {
            let catalog = source.catalog.clone();
            tasks.spawn(async move {
                let status = plex_status(&catalog).await;
                let paths = match status.reachable {
                    true => catalog
                        .check_paths(None, Scope::Sample(PATH_SAMPLE))
                        .await
                        .unwrap_or_default(),
                    false => Vec::new(),
                };
                (i, status, paths)
            });
        }
        let mut reports: Vec<Option<(PlexStatus, Vec<LibraryPaths>)>> =
            composite.servers().iter().map(|_| None).collect();
        while let Some(done) = tasks.join_next().await {
            match done {
                Ok((i, status, paths)) => reports[i] = Some((status, paths)),
                Err(e) => std::panic::resume_unwind(e.into_panic()),
            }
        }
        let reports: Vec<(PlexStatus, Vec<LibraryPaths>)> = reports
            .into_iter()
            .map(|r| r.expect("every server answered"))
            .collect();
        let served = served_now(&config, &reports[0].0);
        let configured: HashMap<String, PlexServerConfig> = plex
            .all_servers()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        // What each is called: as configured, else what it calls itself,
        // else its address's host.
        let names: Vec<String> = composite
            .servers()
            .iter()
            .zip(&reports)
            .map(|(source, (status, _))| {
                configured
                    .get(&source.id)
                    .and_then(|s| s.name.clone())
                    .or_else(|| status.server_name.clone())
                    .unwrap_or_else(|| source.name.clone())
            })
            .collect();

        let libraries = served
            .iter()
            .map(|l| {
                let at = composite
                    .servers()
                    .iter()
                    .position(|s| s.id == l.plex.server);
                let server_name = at
                    .map(|i| names[i].clone())
                    .unwrap_or_else(|| l.plex.server.clone());
                let (status, paths) = match at {
                    Some(i) => (&reports[i].0, reports[i].1.as_slice()),
                    None => {
                        return LibraryEntry {
                            id: library_id(&l.plex),
                            name: l.name.clone(),
                            kind: l.kind().unwrap_or(spoolframe_types::LibraryKind::Movies),
                            server: l.plex.server.clone(),
                            server_name,
                            section: l.plex.section.clone(),
                            plex_title: None,
                            items: None,
                            paths: None,
                            error: Some("its Plex server is not configured".into()),
                            sync: plex.sync(&l.plex),
                        };
                    }
                };
                let section = status
                    .libraries
                    .iter()
                    .find(|s| s.section == l.plex.section);
                let error = match (&status.error, section) {
                    (Some(e), _) => Some(e.clone()),
                    (None, None) => Some(format!(
                        "{server_name} has no library section {} any more",
                        l.plex.section
                    )),
                    (None, Some(_)) => None,
                };
                let title = section.map(|s| s.title.clone());
                LibraryEntry {
                    id: library_id(&l.plex),
                    name: l.name.clone(),
                    kind: section
                        .map(|s| s.kind)
                        .or_else(|| l.kind().ok())
                        .unwrap_or(spoolframe_types::LibraryKind::Movies),
                    server: l.plex.server.clone(),
                    server_name,
                    section: l.plex.section.clone(),
                    items: section.map(|s| s.items),
                    paths: title.as_ref().and_then(|t| {
                        paths
                            .iter()
                            .find(|p| &p.library == t)
                            .cloned()
                            .map(|p| path_check(p, bases))
                    }),
                    plex_title: title,
                    error,
                    sync: plex.sync(&l.plex),
                }
            })
            .collect();

        let servers = composite
            .servers()
            .iter()
            .zip(reports)
            .zip(names)
            .map(|((source, (mut status, _)), name)| {
                mark(&mut status, &source.id, &served);
                let first = source.id == FIRST_PLEX_SERVER;
                let server = configured.get(&source.id);
                let token_set = self.secrets().is_set(&plex_token_secret(&source.id))
                    || server.is_some_and(|s| s.token_file.is_some());
                PlexServerEntry {
                    id: source.id.clone(),
                    name,
                    status,
                    token_set,
                    token_locked: layers.server_token_changeable(&source.id).is_err(),
                    locked: layers.is_locked(if first { "plex.url" } else { "plex.servers" }),
                    path_mapping: server.map(|s| entries(&s.path_mapping)).unwrap_or_default(),
                }
            })
            .collect();
        Ok(LibrariesOverview {
            libraries,
            servers,
            chosen: config.libraries.is_some(),
            locked,
        })
    }

    /// The Plex servers' sections as `plexStatus` shows them, marked with
    /// the libraries they are.
    pub fn mark_served(&self, server: &str, status: &mut PlexStatus) {
        let Ok((_, config)) = self.config_now() else {
            return;
        };
        let served = served_now(&config, status);
        mark(status, server, &served);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_s_id_comes_from_its_name_and_is_never_taken() {
        assert_eq!(server_id("Cabin", &[]), "cabin");
        assert_eq!(server_id("Andrey's NAS (2)", &[]), "andrey-s-nas-2");
        assert_eq!(server_id("4K Tower", &[]), "pms-4k-tower");
        assert_eq!(server_id("Plex", &[]), "plex-2");
        assert_eq!(
            server_id("Cabin", &["cabin".into(), "cabin-2".into()]),
            "cabin-3"
        );
        assert_eq!(server_id("…", &[]), "pms");
    }

    #[test]
    fn a_library_s_id_is_the_one_clients_know() {
        let source = |server: &str| PlexLibraryConfig {
            server: server.into(),
            section: "2".into(),
            sync: None,
        };
        assert_eq!(library_id(&source("plex")), "plex:2");
        assert_eq!(library_id(&source("cabin")), "plex:cabin:2");
    }

    #[test]
    fn libraries_written_back_parse_as_they_were() {
        let list = vec![LibraryConfig {
            name: "Cabin \"movies\"".into(),
            kind: "movies".into(),
            plex: PlexLibraryConfig {
                server: "plex".into(),
                section: "2".into(),
                sync: Some("from_plex".into()),
            },
        }];
        let text = format!(
            "schema = \"spoolframe.server.v1\"; plex {{ url = \"http://a:32400\"; }} libraries = {};",
            libraries_value(&list).to_ucl()
        );
        assert_eq!(ServerConfig::parse(&text).unwrap().libraries, Some(list));
    }
}

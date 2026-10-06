//! What the setup wizard and the Settings pages decide (P3-10), apart from
//! the pages: host-compiled, so `cargo test` covers it.

use spoolframe_admin_api::{LibraryPathCheck, PathMappingEntry};

/// One step of the wizard. Later phases add and replace steps here (P9
/// adds the native catalog's); a step's id is what the server keeps as
/// done, so an id is never reused for another step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub id: &'static str,
    pub title: &'static str,
}

/// Every step, in order. The first is P3-01's setup page; the last sums
/// up and ends the wizard.
pub const STEPS: &[Step] = &[
    Step {
        id: "admin",
        title: "First admin",
    },
    Step {
        id: "server",
        title: "Server",
    },
    Step {
        id: "plex",
        title: "Plex",
    },
    Step {
        id: "paths",
        title: "Paths",
    },
    Step {
        id: "playback",
        title: "Playback",
    },
    Step {
        id: "access",
        title: "Access",
    },
    Step {
        id: "done",
        title: "Done",
    },
];

/// The step called `id`, and its number from 1.
pub fn step(id: &str) -> Option<(usize, Step)> {
    STEPS
        .iter()
        .position(|s| s.id == id)
        .map(|i| (i + 1, STEPS[i]))
}

/// The step after `id`'s.
pub fn next(id: &str) -> Option<Step> {
    let (n, _) = step(id)?;
    STEPS.get(n).copied()
}

/// The step before `id`'s.
pub fn previous(id: &str) -> Option<Step> {
    let (n, _) = step(id)?;
    n.checked_sub(2).map(|i| STEPS[i])
}

/// Where a wizard left half-way picks up: the first step not confirmed.
/// `done` may name steps this build does not have; they are not counted.
pub fn resume(done: &[String]) -> Step {
    STEPS
        .iter()
        .find(|s| !done.iter().any(|d| d == s.id))
        .copied()
        .unwrap_or(STEPS[STEPS.len() - 1])
}

/// "Steps 1–3 are saved": what is behind a resumed step.
pub fn saved_before(at: Step) -> String {
    match step(at.id).map(|(n, _)| n) {
        Some(1) | None => "Nothing is saved yet.".into(),
        Some(2) => "Step 1 is saved.".into(),
        Some(n) => format!("Steps 1–{} are saved.", n - 1),
    }
}

/// `transcode.hardware`, as the Playback step offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hardware {
    Auto,
    Software,
    /// Families in order, as typed: `nvenc, vaapi`.
    Ordered(String),
}

impl Hardware {
    pub fn from_setting(value: &str) -> Self {
        match value.trim() {
            "auto" | "" => Hardware::Auto,
            "none" => Hardware::Software,
            list => Hardware::Ordered(list.to_owned()),
        }
    }

    pub fn setting(&self) -> String {
        match self {
            Hardware::Auto => "auto".into(),
            Hardware::Software => "none".into(),
            Hardware::Ordered(list) => list.trim().to_owned(),
        }
    }
}

/// One family of video encoders, as the Playback step lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    pub name: &'static str,
    /// The codecs it encodes here: "H.264", "HEVC".
    pub codecs: Vec<&'static str>,
}

/// The families the server found working, from ffmpeg's encoder names
/// (`hevc_vaapi`, `libx264`), the hardware ones first. Software is always
/// there.
pub fn families(video_encoders: &[String]) -> Vec<Family> {
    let mut out: Vec<Family> = [
        ("VA-API", "vaapi"),
        ("NVENC", "nvenc"),
        ("Quick Sync", "qsv"),
    ]
    .into_iter()
    .map(|(name, suffix)| Family {
        name,
        codecs: video_encoders
            .iter()
            .filter_map(|e| e.strip_suffix(&format!("_{suffix}")))
            .filter_map(codec_name)
            .collect(),
    })
    .filter(|f| !f.codecs.is_empty())
    .collect();
    out.push(Family {
        name: "Software",
        codecs: video_encoders
            .iter()
            .filter(|e| !e.contains('_') || e.starts_with("lib"))
            .filter_map(|e| codec_name(e.trim_start_matches("lib")))
            .collect(),
    });
    // Two encoders of one codec (libx264 and openh264) name it once.
    for family in &mut out {
        let mut seen = Vec::new();
        family.codecs.retain(|c| {
            let new = !seen.contains(c);
            seen.push(*c);
            new
        });
    }
    out
}

fn codec_name(ffmpeg: &str) -> Option<&'static str> {
    Some(match ffmpeg {
        "h264" | "x264" | "openh264" => "H.264",
        "hevc" | "x265" => "HEVC",
        "av1" | "svtav1" | "aom-av1" | "aom" => "AV1",
        "vp9" | "vpx-vp9" => "VP9",
        _ => return None,
    })
}

/// A tesseract language's name in English, for the few the boards show;
/// its code otherwise.
pub fn language_name(code: &str) -> String {
    match code {
        "eng" => "English",
        "deu" => "German",
        "fra" => "French",
        "spa" => "Spanish",
        "ita" => "Italian",
        "por" => "Portuguese",
        "nld" => "Dutch",
        "rus" => "Russian",
        "ukr" => "Ukrainian",
        "pol" => "Polish",
        "swe" => "Swedish",
        "jpn" => "Japanese",
        "kor" => "Korean",
        "chi_sim" => "Chinese (simplified)",
        "chi_tra" => "Chinese (traditional)",
        other => other,
    }
    .to_owned()
}

/// MiB as the Playback step shows them, in GiB: "20", "1.5".
pub fn gib(mib: i64) -> String {
    if mib % 1024 == 0 {
        (mib / 1024).to_string()
    } else {
        format!("{:.1}", mib as f64 / 1024.0)
    }
}

/// GiB as typed, in MiB; `None` for what is not a positive number.
pub fn mib(gib: &str) -> Option<i64> {
    let n: f64 = gib.trim().parse().ok()?;
    (n > 0.0 && n.is_finite()).then(|| (n * 1024.0).round() as i64)
}

/// A path with no trailing slash, `/` itself kept.
fn trimmed(path: &str) -> &str {
    match path.trim().trim_end_matches('/') {
        "" => "/",
        p => p,
    }
}

/// Where `plex`, in library `library`, is on this server under
/// `mappings`, as the server maps it: a rule for the library before a
/// global one, the longest prefix of whole components first. `None` when
/// no rule matches.
pub fn mapped(mappings: &[PathMappingEntry], library: &str, plex: &str) -> Option<String> {
    let plex = trimmed(plex);
    let matching = |scoped: bool| {
        mappings
            .iter()
            .filter(|m| m.library.as_deref().is_some() == scoped)
            .filter(|m| !scoped || m.library.as_deref() == Some(library))
            .filter_map(|m| {
                let from = trimmed(&m.from);
                let rest = if plex == from {
                    Some("")
                } else if from == "/" {
                    plex.strip_prefix('/')
                } else {
                    plex.strip_prefix(from).and_then(|r| r.strip_prefix('/'))
                }?;
                Some((from.matches('/').count(), m, rest))
            })
            .max_by_key(|(depth, _, _)| *depth)
            .map(|(_, m, rest)| {
                let to = trimmed(&m.to);
                match (to, rest) {
                    (to, "") => to.to_owned(),
                    ("/", rest) => format!("/{rest}"),
                    (to, rest) => format!("{to}/{rest}"),
                }
            })
    };
    matching(true).or_else(|| matching(false))
}

/// One folder of a library on the Paths step: where Plex has it, and
/// where it is here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRow {
    pub library: String,
    pub plex: String,
    pub local: String,
    /// `local` is the server's suggestion, not a saved mapping.
    pub suggested: bool,
}

/// The rows for `checks` under the saved `mappings`: each folder where it
/// maps to, or where the server suggests when its sample fails.
pub fn rows(checks: &[LibraryPathCheck], mappings: &[PathMappingEntry]) -> Vec<PathRow> {
    let mut out = Vec::new();
    for check in checks {
        let failing = check.ok < check.checked;
        for root in &check.roots {
            let saved = mapped(mappings, &check.library, root);
            let suggestion = check
                .suggestion
                .as_ref()
                .filter(|_| failing)
                .and_then(|s| mapped(std::slice::from_ref(s), &check.library, root));
            let (local, suggested) = match (saved, suggestion) {
                (_, Some(suggested)) => (suggested, true),
                (Some(saved), None) => (saved, false),
                (None, None) => (trimmed(root).to_owned(), false),
            };
            out.push(PathRow {
                library: check.library.clone(),
                plex: trimmed(root).to_owned(),
                local,
                suggested,
            });
        }
    }
    out
}

/// The mappings `rows` and the extra ones make: one per folder that is
/// somewhere else here, for its library, then the extra ones as typed.
pub fn mappings(rows: &[PathRow], extra: &[PathMappingEntry]) -> Vec<PathMappingEntry> {
    rows.iter()
        .filter(|r| !r.local.trim().is_empty() && trimmed(&r.local) != trimmed(&r.plex))
        .map(|r| PathMappingEntry {
            library: Some(r.library.clone()),
            from: r.plex.clone(),
            to: trimmed(&r.local).to_owned(),
        })
        .chain(
            extra
                .iter()
                .filter(|m| !m.from.trim().is_empty() && !m.to.trim().is_empty())
                .cloned(),
        )
        .collect()
}

/// Of the saved `mappings`, those no folder of `checks` is under: the
/// rows do not show them, so the Paths step keeps them as extra rows, or
/// Continue would save the rows' mappings without them.
pub fn unattached(
    checks: &[LibraryPathCheck],
    mappings: &[PathMappingEntry],
) -> Vec<PathMappingEntry> {
    mappings
        .iter()
        .filter(|m| {
            !checks.iter().any(|check| {
                check
                    .roots
                    .iter()
                    .any(|root| mapped(std::slice::from_ref(*m), &check.library, root).is_some())
            })
        })
        .cloned()
        .collect()
}

/// A library's sample, as its row says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sample {
    /// No files to look at.
    Empty,
    Ok {
        of: u64,
    },
    Missing {
        missing: u64,
        of: u64,
    },
}

pub fn sample(check: &LibraryPathCheck) -> Sample {
    match (check.checked, check.checked - check.ok.min(check.checked)) {
        (0, _) => Sample::Empty,
        (of, 0) => Sample::Ok { of },
        (of, missing) => Sample::Missing { missing, of },
    }
}

/// The setup summary's warnings (P3-18): "Music: 2 of 5 missing" for each
/// library whose sample has files this server cannot read.
pub fn missing_files(checks: &[LibraryPathCheck]) -> Vec<String> {
    checks
        .iter()
        .filter_map(|c| match sample(c) {
            Sample::Missing { missing, of } => {
                Some(format!("{}: {missing} of {of} missing", c.library))
            }
            _ => None,
        })
        .collect()
}

/// The longest display name the server takes (`setDisplayName`).
const MAX_DISPLAY_NAME: usize = 64;

/// The first admin's display name as typed in step 1 (P3-18), checked as
/// the server checks it before the account is made, so a name it would
/// refuse cannot leave an account made without one. `None` when blank:
/// the username is shown.
pub fn display_name(typed: &str) -> Result<Option<String>, String> {
    let name = typed.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_DISPLAY_NAME || name.chars().any(char::is_control) {
        return Err(format!(
            "A display name is at most {MAX_DISPLAY_NAME} characters, on one line."
        ));
    }
    Ok(Some(name.to_owned()))
}

/// The Done card's Playback line (WebWizardDone): how encoders are
/// chosen, naming the one Automatic picks ("Automatic (VA-API)"), and how
/// many encodes run at once. `video_encoders`: what the server found,
/// `None` until it answers.
pub fn playback_summary(
    hardware: &Hardware,
    video_encoders: Option<&[String]>,
    max_jobs: u64,
) -> String {
    let how = match hardware {
        // Automatic takes the first family that works: the hardware ones
        // come first, software last.
        Hardware::Auto => match video_encoders.map(families) {
            Some(found) => match found.first() {
                Some(f) if f.name != "Software" => format!("Automatic ({})", f.name),
                _ => "Automatic (software)".to_owned(),
            },
            None => "Automatic".to_owned(),
        },
        Hardware::Software => "Software only".to_owned(),
        Hardware::Ordered(list) => format!("In this order: {list}"),
    };
    let jobs = if max_jobs == 1 {
        "1 encode".to_owned()
    } else {
        format!("{max_jobs} encodes")
    };
    format!("{how} · {jobs} at once")
}

/// The Done card's one warning beside Paths: the library with missing
/// files, or how many have some. `None` when every file was found.
pub fn missing_summary(missing: &[String]) -> Option<String> {
    match missing {
        [] => None,
        [one] => Some(one.clone()),
        many => Some(format!("{} libraries have missing files", many.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_names_the_hardware_it_picks() {
        let found = ["h264_vaapi".to_owned(), "libx264".to_owned()];
        assert_eq!(
            playback_summary(&Hardware::Auto, Some(&found), 2),
            "Automatic (VA-API) · 2 encodes at once"
        );
    }

    #[test]
    fn automatic_without_hardware_says_software() {
        let found = ["libx264".to_owned()];
        assert_eq!(
            playback_summary(&Hardware::Auto, Some(&found), 1),
            "Automatic (software) · 1 encode at once"
        );
    }

    #[test]
    fn automatic_before_the_server_answers_names_nothing() {
        assert_eq!(
            playback_summary(&Hardware::Auto, None, 2),
            "Automatic · 2 encodes at once"
        );
    }

    #[test]
    fn one_library_missing_files_is_named() {
        assert_eq!(
            missing_summary(&["Music: 2 of 5 missing".to_owned()]).as_deref(),
            Some("Music: 2 of 5 missing")
        );
    }

    #[test]
    fn several_libraries_missing_files_are_one_warning() {
        assert_eq!(
            missing_summary(&[
                "Music: 2 of 5 missing".to_owned(),
                "Kids: 1 of 9 missing".to_owned()
            ])
            .as_deref(),
            Some("2 libraries have missing files")
        );
    }

    #[test]
    fn nothing_missing_warns_of_nothing() {
        assert_eq!(missing_summary(&[]), None);
    }

    fn rule(library: Option<&str>, from: &str, to: &str) -> PathMappingEntry {
        PathMappingEntry {
            library: library.map(str::to_owned),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_wizard_resumes_at_the_first_step_not_confirmed() {
        let done = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(resume(&done(&[])).id, "admin");
        assert_eq!(resume(&done(&["admin", "server", "plex"])).id, "paths");
        // Out of order, and a step from another build, count as they are.
        assert_eq!(resume(&done(&["server", "admin", "import"])).id, "plex");
        let all: Vec<String> = STEPS.iter().map(|s| s.id.to_owned()).collect();
        assert_eq!(resume(&all).id, "done");
        assert_eq!(
            saved_before(resume(&done(&["admin", "server", "plex"]))),
            "Steps 1–3 are saved."
        );
    }

    #[test]
    fn steps_are_numbered_and_walked_in_order() {
        assert_eq!(step("plex").map(|(n, s)| (n, s.title)), Some((3, "Plex")));
        assert_eq!(next("plex").unwrap().id, "paths");
        assert_eq!(previous("plex").unwrap().id, "server");
        assert_eq!(previous("admin"), None);
        assert_eq!(next("done"), None);
        assert_eq!(STEPS.len(), 7);
    }

    #[test]
    fn hardware_goes_both_ways() {
        for (setting, choice) in [
            ("auto", Hardware::Auto),
            ("none", Hardware::Software),
            ("nvenc,vaapi", Hardware::Ordered("nvenc,vaapi".into())),
        ] {
            assert_eq!(Hardware::from_setting(setting), choice);
            assert_eq!(choice.setting(), setting);
        }
    }

    #[test]
    fn encoders_are_grouped_by_family_with_software_last() {
        let found = families(&[
            "hevc_vaapi".into(),
            "h264_vaapi".into(),
            "libx264".into(),
            "libx265".into(),
        ]);
        assert_eq!(
            found,
            [
                Family {
                    name: "VA-API",
                    codecs: vec!["HEVC", "H.264"]
                },
                Family {
                    name: "Software",
                    codecs: vec!["H.264", "HEVC"]
                },
            ]
        );
    }

    #[test]
    fn scratch_space_is_shown_and_typed_in_gib() {
        assert_eq!(gib(20480), "20");
        assert_eq!(gib(1536), "1.5");
        assert_eq!(mib("20"), Some(20480));
        assert_eq!(mib(" 1.5 "), Some(1536));
        assert_eq!(mib("0"), None);
        assert_eq!(mib("lots"), None);
    }

    #[test]
    fn a_folder_maps_as_the_server_maps_it() {
        let rules = [
            rule(None, "/data", "/mnt/nas"),
            rule(Some("Music"), "/data/music", "/srv/music/"),
        ];
        assert_eq!(
            mapped(&rules, "Movies", "/data/movies").as_deref(),
            Some("/mnt/nas/movies")
        );
        assert_eq!(
            mapped(&rules, "Music", "/data/music").as_deref(),
            Some("/srv/music")
        );
        // Whole components only.
        assert_eq!(mapped(&rules, "Movies", "/database"), None);
        assert_eq!(
            mapped(&[rule(None, "/", "/x")], "Movies", "/a").as_deref(),
            Some("/x/a")
        );
    }

    fn check(library: &str, root: &str, ok: u64, checked: u64) -> LibraryPathCheck {
        LibraryPathCheck {
            library: library.into(),
            roots: vec![root.into()],
            checked,
            ok,
            failures: Vec::new(),
            suggestion: None,
        }
    }

    #[test]
    fn a_failing_folder_takes_the_suggestion_and_a_passing_one_keeps_its_mapping() {
        let failing = LibraryPathCheck {
            suggestion: Some(rule(Some("Movies"), "/data", "/mnt/nas")),
            ..check("Movies", "/data/movies", 0, 5)
        };
        let passing = check("Kids", "/mnt/kids", 5, 5);
        let saved = [rule(Some("Kids"), "/mnt/kids", "/srv/kids")];
        let rows = rows(&[failing, passing], &saved);
        assert_eq!(
            rows,
            [
                PathRow {
                    library: "Movies".into(),
                    plex: "/data/movies".into(),
                    local: "/mnt/nas/movies".into(),
                    suggested: true,
                },
                PathRow {
                    library: "Kids".into(),
                    plex: "/mnt/kids".into(),
                    local: "/srv/kids".into(),
                    suggested: false,
                },
            ]
        );
        assert_eq!(
            mappings(&rows, &[rule(None, "/x", "")]),
            [
                rule(Some("Movies"), "/data/movies", "/mnt/nas/movies"),
                rule(Some("Kids"), "/mnt/kids", "/srv/kids"),
            ]
        );
    }

    /// Saved mappings that no library folder shows are kept on Continue
    /// (P3-18): a rule for a library not served yet, or for a prefix no
    /// folder is under.
    #[test]
    fn saved_mappings_no_folder_shows_are_kept_as_extra_ones() {
        let checks = [
            check("Movies", "/data/movies", 5, 5),
            check("Kids", "/mnt/kids", 5, 5),
        ];
        let saved = [
            rule(Some("Kids"), "/mnt/kids", "/srv/kids"),
            rule(None, "/data", "/mnt/nas"),
            rule(None, "/music", "/srv/music"),
            rule(Some("Documentaries"), "/data/docs", "/srv/docs"),
            rule(Some("Movies"), "/elsewhere", "/srv/elsewhere"),
        ];
        let extra = unattached(&checks, &saved);
        assert_eq!(
            extra,
            [
                rule(None, "/music", "/srv/music"),
                rule(Some("Documentaries"), "/data/docs", "/srv/docs"),
                rule(Some("Movies"), "/elsewhere", "/srv/elsewhere"),
            ]
        );
        // Continue saves the rows' mappings and those.
        assert_eq!(
            mappings(&rows(&checks, &saved), &extra),
            [
                rule(Some("Movies"), "/data/movies", "/mnt/nas/movies"),
                rule(Some("Kids"), "/mnt/kids", "/srv/kids"),
                rule(None, "/music", "/srv/music"),
                rule(Some("Documentaries"), "/data/docs", "/srv/docs"),
                rule(Some("Movies"), "/elsewhere", "/srv/elsewhere"),
            ]
        );
        // With no folders known (the check failed), every one is kept.
        assert_eq!(unattached(&[], &saved), saved);
    }

    #[test]
    fn a_folder_at_the_same_path_here_needs_no_mapping() {
        let rows = rows(&[check("Movies", "/data/movies/", 5, 5)], &[]);
        assert_eq!(rows[0].local, "/data/movies");
        assert_eq!(mappings(&rows, &[]), []);
    }

    #[test]
    fn a_sample_says_how_many_of_it_are_missing() {
        assert_eq!(sample(&check("M", "/m", 5, 5)), Sample::Ok { of: 5 });
        assert_eq!(
            sample(&check("M", "/m", 3, 5)),
            Sample::Missing { missing: 2, of: 5 }
        );
        assert_eq!(sample(&check("M", "/m", 0, 0)), Sample::Empty);
    }

    #[test]
    fn the_summary_names_each_library_with_files_the_server_cannot_read() {
        let checks = [
            check("Movies", "/m", 5, 5),
            check("Music", "/music", 3, 5),
            check("Empty", "/e", 0, 0),
            check("Kids", "/k", 0, 5),
        ];
        assert_eq!(
            missing_files(&checks),
            ["Music: 2 of 5 missing", "Kids: 5 of 5 missing"]
        );
        assert!(missing_files(&checks[..1]).is_empty());
    }

    #[test]
    fn the_first_admin_s_display_name_is_checked_as_the_server_does() {
        assert_eq!(display_name("  Andrey "), Ok(Some("Andrey".into())));
        assert_eq!(display_name("   "), Ok(None));
        assert_eq!(display_name(&"é".repeat(64)), Ok(Some("é".repeat(64))));
        assert!(display_name(&"a".repeat(65)).is_err());
        assert!(display_name("two\nlines").is_err());
    }
}

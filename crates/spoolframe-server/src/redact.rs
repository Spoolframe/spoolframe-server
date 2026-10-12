//! A command line as playback debug shows it (P3-15): paths, yes; tokens,
//! never. ffmpeg reads a link in the job's directory, whose name is the
//! playback handle (a capability: whoever has it, and the session, reads
//! the playback), and a future input may be a URL carrying a Plex token.

/// What stands in for a secret.
pub const HIDDEN: &str = "<hidden>";

/// `command`, quoted as a shell would take it, with every one of `secrets`
/// and every token-like value (`X-Plex-Token=…`, `token: …`, `Bearer …`)
/// replaced by [`HIDDEN`].
pub fn command_line(command: &[String], secrets: &[&str]) -> String {
    command
        .iter()
        .map(|arg| quote(&hide(arg, secrets)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` with `secrets` and token-like values hidden.
pub fn hide(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, HIDDEN);
    }
    let text = hide_after(&text, "token=", false);
    let text = hide_after(&text, "token:", true);
    hide_after(&text, "bearer ", true)
}

/// The value after each `key` (in any case) hidden, up to the next `&`,
/// quote, `;` or whitespace; with `spaced`, spaces and tabs right after
/// the key are kept and the value after them is hidden.
fn hide_after(text: &str, key: &str, spaced: bool) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower[at..].find(key) {
        let mut start = at + found + key.len();
        if spaced {
            // Tabs too: `X-Plex-Token:\t<token>` left the token in clear
            // (review 2026-10-11 L50). Not newlines: a value never follows
            // its key on the next line.
            start += text[start..].len() - text[start..].trim_start_matches([' ', '\t']).len();
        }
        let end = text[start..]
            .find(|c: char| c == '&' || c == '\'' || c == '"' || c == ';' || c.is_whitespace())
            .map_or(text.len(), |n| start + n);
        out.push_str(&text[at..start]);
        if end > start {
            out.push_str(HIDDEN);
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// `arg` as a POSIX shell takes it back: bare when it is plain, else in
/// single quotes.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=,+@%".contains(c));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn the_playback_handle_in_a_path_is_hidden_and_the_path_kept() {
        let handle = "9f8e7d6c5b4a39281706f5e4d3c2b1a0";
        let shown = command_line(
            &args(&[
                "ffmpeg",
                "-i",
                &format!("/var/cache/spoolframe-jobs/{handle}/input"),
            ]),
            &[handle],
        );
        assert_eq!(
            shown,
            "ffmpeg -i '/var/cache/spoolframe-jobs/<hidden>/input'"
        );
        assert!(!shown.contains(handle));
    }

    #[test]
    fn a_token_after_a_tab_is_hidden() {
        let shown = hide_after("X-Plex-Token:\tsecret123", "token:", true);
        assert!(!shown.contains("secret123"), "{shown}");
        assert_eq!(shown, "X-Plex-Token:\t<hidden>");
    }

    #[test]
    fn a_plex_token_in_a_url_or_a_header_is_hidden() {
        let shown = command_line(
            &args(&[
                "ffmpeg",
                "-headers",
                "X-Plex-Token: s3cr3t-header\r\n",
                "-i",
                "http://plex.lan:32400/library/parts/1/file.mkv?X-Plex-Token=s3cr3tQuery&download=1",
                "-H",
                "Authorization: Bearer abc.def",
            ]),
            &[],
        );
        for secret in ["s3cr3t-header", "s3cr3tQuery", "abc.def"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(
            shown.contains("X-Plex-Token=<hidden>&download=1"),
            "{shown}"
        );
        assert!(shown.contains("/library/parts/1/file.mkv"), "{shown}");
    }

    #[test]
    fn arguments_are_quoted_as_a_shell_reads_them() {
        assert_eq!(
            command_line(
                &args(&[
                    "ffmpeg",
                    "-i",
                    "/media/Woe's Hollow.mkv",
                    "-vf",
                    "scale=-2:1080"
                ]),
                &[],
            ),
            r"ffmpeg -i '/media/Woe'\''s Hollow.mkv' -vf scale=-2:1080"
        );
    }
}

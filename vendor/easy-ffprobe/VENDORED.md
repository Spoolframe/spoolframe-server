# Vendored: easy-ffprobe 0.5.7

Source: <https://crates.io/crates/easy-ffprobe/0.5.7>
(<https://github.com/frederik-uni/ffprobe-rs>), MIT, by Christoph Herzog
and Frederik Uni. `LICENSE` is the upstream license, unchanged.

Spoolframe uses only the typed model of ffprobe's JSON (`FfProbe` and the
stream types). It runs ffprobe itself, with a timeout, and parses the
output with `serde_json::from_slice::<FfProbe>`.

## Changes from upstream

- `Stream::start_pts` is `Option<i64>` (was a required `i64`). ffprobe 9
  leaves it out for the streams of standalone subtitle files (`.srt`,
  `.vtt`, VobSub `.idx`), so upstream fails to parse those at all.
  `Stream::start_time` treats a missing value as 0.
- The format's `start_time` and `duration` are read with
  `Duration::try_from_secs_f64`. Upstream's `from_secs_f64` panics on a
  value past `u64::MAX` seconds or on `inf`, so such JSON panicked in the
  parse instead of failing it (PX-10, found by the `ffprobe_json` fuzz
  target).
- `codec_name` of video, audio and subtitle streams is `Option<String>`
  (was a required `String`). ffprobe's JSON leaves it out for a codec it
  does not know (`print_str_opt`), so upstream failed the whole file on
  one such stream.
- `Stream::extradata` (`Option<String>`, absent by default) is added:
  ffprobe prints a stream's codec record with `-show_data`, as a hex
  dump. Spoolframe reads it to tell whether an H.264 or HEVC record holds
  the stream's parameter sets.
- The `async` feature and `ffprobe_async*` are removed. They ignored
  `Config::ffprobe_bin` (always ran `ffprobe` from `PATH`) and had no
  timeout; Spoolframe runs the process itself.
- Not copied: `tests/` (they download sample media from the Internet and
  probe it), `.github/`, and
  the upstream `Cargo.lock`.

Drop this copy for the crates.io release once upstream accepts these.

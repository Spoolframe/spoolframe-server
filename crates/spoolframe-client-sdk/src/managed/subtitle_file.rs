//! A playback's subtitle that comes as a file: an external one, or one
//! the server reads as text ([`ManagedPlayback::subtitle_file`]).

use super::ManagedPlayback;
use crate::SdkError;
use crate::tracks::subtitle_comes_as_file;
use spoolframe_types::SubtitleFormat;
use std::time::Duration;

/// A subtitle file, whole: text in `format`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFile {
    pub format: SubtitleFormat,
    pub bytes: Vec<u8>,
}

/// Aborts the task it holds when dropped: a caller that stops waiting
/// stops the asking.
struct Asking(tokio::task::JoinHandle<Result<SubtitleFile, SdkError>>);

impl Drop for Asking {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ManagedPlayback {
    /// How long to wait before asking again for a subtitle the server is
    /// still reading as text.
    pub const SUBTITLE_RETRY: Duration = Duration::from_secs(5);

    /// The playback's subtitle file, once the server has it. One still
    /// being read as text is asked for again every
    /// [`Self::SUBTITLE_RETRY`] until it is ready; drop the future to
    /// stop. A plan whose subtitle is no file is
    /// [`SdkError::InvalidInput`]; an empty file, or one that is not
    /// UTF-8 text, is [`SdkError::Protocol`], as every subtitle file the
    /// server sends is text.
    pub async fn subtitle_file(&self) -> Result<SubtitleFile, SdkError> {
        let this = self.clone();
        let mut asking = Asking(
            self.0
                .runtime
                .spawn(async move { this.ask_subtitle().await }),
        );
        (&mut asking.0)
            .await
            .map_err(|e| SdkError::Protocol(format!("subtitle task ended: {e}")))?
    }

    async fn ask_subtitle(&self) -> Result<SubtitleFile, SdkError> {
        if !subtitle_comes_as_file(&self.live()?.plan) {
            return Err(SdkError::InvalidInput(
                "this playback's subtitle is not a file".into(),
            ));
        }
        let backend = &self.0.backend;
        let subtitle = loop {
            let answer = self
                .resuming(None, move |p| async move {
                    backend.read_subtitle(&p.handle).await
                })
                .await;
            match answer {
                Err(SdkError::SubtitleNotReady { done_percent }) => {
                    tracing::debug!(done_percent, "the subtitle is still being read");
                    tokio::time::sleep(Self::SUBTITLE_RETRY).await;
                }
                answer => break answer?,
            }
        };
        check(&subtitle.bytes).map_err(|why| SdkError::Protocol(why.into()))?;
        Ok(SubtitleFile {
            format: subtitle.format,
            bytes: subtitle.bytes,
        })
    }
}

/// Why `bytes` cannot be a subtitle file: every one the server sends is
/// text.
fn check(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err("the subtitle is empty");
    }
    if std::str::from_utf8(bytes).is_err() {
        return Err("the subtitle is not UTF-8 text");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn a_subtitle_of_whitespace_is_empty() {
        assert_eq!(check(b" \n\t"), Err("the subtitle is empty"));
        assert_eq!(check(b""), Err("the subtitle is empty"));
    }

    #[test]
    fn a_subtitle_that_is_not_utf8_is_refused() {
        assert_eq!(
            check(&[0xff, 0xfe, b'1']),
            Err("the subtitle is not UTF-8 text")
        );
    }

    #[test]
    fn a_text_subtitle_passes() {
        assert_eq!(
            check("1\n00:00:01,000 --> 00:00:02,000\nHé\n".as_bytes()),
            Ok(())
        );
    }
}

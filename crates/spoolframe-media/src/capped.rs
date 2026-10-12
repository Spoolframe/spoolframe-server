//! A child's output, kept only up to a size: the rest is read and
//! dropped, so a chatty child neither fills memory nor stalls on a full
//! pipe (review 2026-10-11 L66). For runs whose output only the start of
//! matters of (an encoder list, an error message); OCR's own runs refuse
//! too much stdout instead (`ocr::run`).

use std::process::{Output, Stdio};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// The most of each of stdout and stderr kept.
pub(crate) const KEPT: usize = 1 << 20;

/// `command` run to its end with stdout and stderr piped unless the caller
/// set them otherwise; at most `keep` bytes of each kept.
pub(crate) async fn output(command: &mut Command, keep: usize) -> std::io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stdout, stderr, status) = tokio::join!(
        read_some(stdout, keep),
        read_some(stderr, keep),
        child.wait()
    );
    Ok(Output {
        status: status?,
        stdout,
        stderr,
    })
}

/// Up to `keep` bytes of `pipe`, the rest read to its end and dropped.
async fn read_some<R: AsyncRead + Unpin>(pipe: Option<R>, keep: usize) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let _ = (&mut pipe).take(keep as u64).read_to_end(&mut kept).await;
    let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child writing far more than is kept runs to its end, and only
    /// the start of what it wrote is kept.
    #[tokio::test]
    async fn only_the_start_of_a_long_output_is_kept() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 3000000 /dev/zero >&2; echo done"]);
        let output = output(&mut command, 1000).await.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stderr.len(), 1000);
        assert_eq!(output.stdout, b"done\n");
    }
}

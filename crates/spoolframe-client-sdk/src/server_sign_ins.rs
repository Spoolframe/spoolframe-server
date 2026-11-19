//! Signing in to several servers at once, each server's result as it
//! comes: [`Client::sign_in_paired_progress`] for a paired guest, and
//! [`crate::PhoneSignIn::server_sign_ins`] for a TV a phone signs in. An
//! app shows "Signed in to 2 of 3" while the rest are still dialled.

use tokio::sync::watch;

use crate::client::Client;
use crate::error::SdkError;
use crate::portable::PortableUserId;
use crate::session::Session;

/// One server's sign-in, done.
#[derive(Debug, Clone)]
pub struct ServerSignIn {
    /// The server, a hex endpoint id.
    pub server: String,
    /// Its session, or why there is none.
    pub result: Result<Session, SdkError>,
}

/// What is known so far: how many servers in all, once known, and those
/// done, in the order they finished.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tally {
    pub(crate) total: Option<usize>,
    pub(crate) done: Vec<ServerSignIn>,
}

/// Each server's sign-in as it finishes: [`ServerSignIns::next`] until
/// `None`. Dropping it stops nothing: the sign-ins go on, and are kept.
#[derive(Debug)]
pub struct ServerSignIns {
    tally: watch::Receiver<Tally>,
    seen: usize,
}

impl ServerSignIns {
    pub(crate) fn new(tally: watch::Receiver<Tally>) -> Self {
        Self { tally, seen: 0 }
    }

    /// How many servers are signed in to in all: `None` until it is
    /// known (for a phone sign-in, until the phone's offers come).
    pub fn total(&self) -> Option<usize> {
        self.tally.borrow().total
    }

    /// The next server done, as it finishes; `None` once every one has
    /// been given, or the sign-in ended with no more to come.
    pub async fn next(&mut self) -> Option<ServerSignIn> {
        loop {
            {
                let tally = self.tally.borrow_and_update();
                if let Some(done) = tally.done.get(self.seen) {
                    self.seen += 1;
                    return Some(done.clone());
                }
                if tally.total.is_some_and(|total| self.seen >= total) {
                    return None;
                }
            }
            if self.tally.changed().await.is_err() {
                // Ended: what it sent before it ended is still given.
                let tally = self.tally.borrow();
                let done = tally.done.get(self.seen).cloned();
                self.seen += usize::from(done.is_some());
                return done;
            }
        }
    }
}

impl Client {
    /// [`Client::sign_in_paired`], each server's result as it finishes:
    /// all are signed in to at once, on a task of their own, which goes
    /// on whether or not the [`ServerSignIns`] is read.
    /// [`ServerSignIns::total`] is `servers.len()`.
    pub fn sign_in_paired_progress(
        &self,
        identity: PortableUserId,
        servers: Vec<String>,
    ) -> ServerSignIns {
        let (tally, watching) = watch::channel(Tally {
            total: Some(servers.len()),
            done: Vec::new(),
        });
        let client = self.clone();
        tokio::spawn(async move {
            let mut signing = tokio::task::JoinSet::new();
            for server in servers {
                let client = client.clone();
                signing.spawn(async move {
                    let result = client.sign_in_portable(&server, identity).await;
                    ServerSignIn { server, result }
                });
            }
            while let Some(done) = signing.join_next().await {
                match done {
                    Ok(done) => tally.send_modify(|t| t.done.push(done)),
                    Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                    Err(_) => {}
                }
            }
        });
        ServerSignIns::new(watching)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(server: &str) -> ServerSignIn {
        ServerSignIn {
            server: server.into(),
            result: Err(SdkError::Unreachable(server.into())),
        }
    }

    fn servers(got: &[ServerSignIn]) -> Vec<&str> {
        got.iter().map(|s| s.server.as_str()).collect()
    }

    /// Each as it comes, then `None` once the total is reached.
    #[tokio::test]
    async fn each_server_comes_as_it_finishes_then_none() {
        let (tally, watching) = watch::channel(Tally::default());
        let mut sign_ins = ServerSignIns::new(watching);
        assert_eq!(sign_ins.total(), None);
        tally.send_modify(|t| t.total = Some(2));
        assert_eq!(sign_ins.total(), Some(2));
        tally.send_modify(|t| t.done.push(failed("a")));
        let first = sign_ins.next().await.unwrap();
        let waiting = tokio::spawn(async move {
            let second = sign_ins.next().await;
            (second, sign_ins.next().await)
        });
        tally.send_modify(|t| t.done.push(failed("b")));
        let (second, end) = waiting.await.unwrap();
        assert_eq!(servers(&[first, second.unwrap()]), ["a", "b"]);
        assert!(end.is_none());
    }

    /// One that ends early still gives what it did, then `None`.
    #[tokio::test]
    async fn an_end_before_the_total_gives_what_was_done() {
        let (tally, watching) = watch::channel(Tally::default());
        let mut sign_ins = ServerSignIns::new(watching);
        tally.send_modify(|t| {
            t.total = Some(3);
            t.done.push(failed("a"));
        });
        drop(tally);
        assert_eq!(sign_ins.next().await.unwrap().server, "a");
        assert!(sign_ins.next().await.is_none());
    }
}

//! Signing in to several servers at once, each server's result as it
//! comes: [`Client::sign_in_paired_progress`] for a paired guest, and
//! [`crate::PhoneSignIn::server_sign_ins`] for a TV a phone signs in. An
//! app shows "Signed in to 2 of 3" while the rest are still dialled.

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
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

/// What is known so far: how many sign-ins in all, once known, and those
/// done, one entry per sign-in in the order each first finished, holding
/// its latest result; and whether that total is final. A phone
/// sign-in's grows while the phone may offer more, and a sign-in it
/// retries replaces its entry. So `done` never holds more than `total`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tally {
    pub(crate) total: Option<usize>,
    done: Vec<Done>,
    pub(crate) settled: bool,
}

/// One sign-in's latest result, and how many results it has had.
#[derive(Debug, Clone)]
struct Done {
    key: String,
    version: u64,
    result: ServerSignIn,
}

impl Tally {
    /// Keep `result` as the latest of sign-in `key`: in place of its
    /// earlier one, or as a new one.
    pub(crate) fn record(&mut self, key: String, result: ServerSignIn) {
        match self.done.iter_mut().find(|d| d.key == key) {
            Some(done) => {
                done.version += 1;
                done.result = result;
            }
            None => self.done.push(Done {
                key,
                version: 1,
                result,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn done_len(&self) -> usize {
        self.done.len()
    }
}

/// Each server's sign-in as it finishes: [`ServerSignIns::next`] until
/// `None`. Dropping it stops nothing: the sign-ins go on, and are kept.
/// A clone reads on from where this one has got to, on its own.
///
/// A sign-in done again (a phone retrying a server the TV could not sign
/// in to) comes again, with its new result: an update that replaces the
/// one given before for that server and account. Count each server by
/// its latest; [`ServerSignIns::total`] counts each once.
#[derive(Debug, Clone)]
pub struct ServerSignIns {
    tally: watch::Receiver<Tally>,
    /// The version of each entry given so far, by its place.
    seen: Vec<u64>,
}

impl ServerSignIns {
    pub(crate) fn new(tally: watch::Receiver<Tally>) -> Self {
        Self {
            tally,
            seen: Vec::new(),
        }
    }

    /// How many servers are signed in to in all: `None` until it is
    /// known (for a phone sign-in, until the phone's offers come; it
    /// grows if the phone offers more after).
    pub fn total(&self) -> Option<usize> {
        self.tally.borrow().total
    }

    /// The next server done, as it finishes, or done again with a new
    /// result; `None` once every one has been given at its latest and no
    /// more can come (for a phone sign-in, once the phone is done), or
    /// the sign-in ended.
    pub async fn next(&mut self) -> Option<ServerSignIn> {
        loop {
            if let Some(answer) = self.read() {
                return answer;
            }
            if self.tally.changed().await.is_err() {
                // Ended: what it sent before it ended is still given.
                return self.read().flatten();
            }
        }
    }

    /// The answer the tally gives now: `Some(Some(_))` the next server
    /// done, `Some(None)` every one given, `None` nothing yet.
    fn read(&mut self) -> Option<Option<ServerSignIn>> {
        let tally = self.tally.borrow_and_update();
        for (at, done) in tally.done.iter().enumerate() {
            if self.seen.get(at).copied().unwrap_or(0) != done.version {
                self.seen.resize(self.seen.len().max(at + 1), 0);
                self.seen[at] = done.version;
                return Some(Some(done.result.clone()));
            }
        }
        (tally.settled && self.seen.len() >= tally.total.unwrap_or(0)).then_some(None)
    }
}

/// Run every one of `signing` at once, each a sign-in's key and its
/// future, telling `tally` of each as it finishes (in place of that
/// key's earlier result): each one's result, in `signing`'s order.
pub(crate) async fn sign_in_each<F>(
    tally: &watch::Sender<Tally>,
    signing: Vec<(String, F)>,
) -> Vec<Result<Session, SdkError>>
where
    F: std::future::Future<Output = ServerSignIn>,
{
    let mut results: Vec<Option<Result<Session, SdkError>>> = vec![None; signing.len()];
    let mut running: FuturesUnordered<_> = signing
        .into_iter()
        .enumerate()
        .map(|(at, (key, signing))| async move { (at, key, signing.await) })
        .collect();
    while let Some((at, key, done)) = running.next().await {
        results[at] = Some(done.result.clone());
        tally.send_modify(|t| t.record(key, done));
    }
    results
        .into_iter()
        .map(|r| r.expect("each sign-in finished once"))
        .collect()
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
            settled: true,
        });
        let client = self.clone();
        tokio::spawn(async move {
            let signing = servers
                .into_iter()
                .map(|server| {
                    let client = &client;
                    (server.clone(), async move {
                        let result = client.sign_in_portable(&server, identity).await;
                        ServerSignIn { server, result }
                    })
                })
                .collect();
            sign_in_each(&tally, signing).await;
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
        tally.send_modify(|t| t.record("a".into(), failed("a")));
        let first = sign_ins.next().await.unwrap();
        let waiting = tokio::spawn(async move {
            let second = sign_ins.next().await;
            (second, sign_ins.next().await)
        });
        tally.send_modify(|t| {
            t.record("b".into(), failed("b"));
            t.settled = true;
        });
        let (second, end) = waiting.await.unwrap();
        assert_eq!(servers(&[first, second.unwrap()]), ["a", "b"]);
        assert!(end.is_none());
    }

    /// While more may come (a phone that may offer again), the total
    /// reached is not the end: it waits, and the total grows.
    #[tokio::test]
    async fn a_total_that_may_grow_is_not_the_end_until_settled() {
        let (tally, watching) = watch::channel(Tally::default());
        let mut sign_ins = ServerSignIns::new(watching);
        tally.send_modify(|t| {
            t.total = Some(1);
            t.record("a".into(), failed("a"));
        });
        assert_eq!(sign_ins.next().await.unwrap().server, "a");
        let waiting = tokio::spawn(async move {
            let next = sign_ins.next().await;
            (next, sign_ins.next().await, sign_ins.total())
        });
        tokio::task::yield_now().await;
        tally.send_modify(|t| {
            t.total = Some(2);
            t.record("b".into(), failed("b"));
        });
        tally.send_modify(|t| t.settled = true);
        let (next, end, total) = waiting.await.unwrap();
        assert_eq!(next.unwrap().server, "b");
        assert!(end.is_none());
        assert_eq!(total, Some(2));
    }

    /// A sign-in done again replaces its entry: it comes again as an
    /// update, and the entries never outnumber the total.
    #[tokio::test]
    async fn a_sign_in_done_again_is_an_update_not_another() {
        let (tally, watching) = watch::channel(Tally::default());
        let mut sign_ins = ServerSignIns::new(watching);
        tally.send_modify(|t| {
            t.total = Some(1);
            t.record("a".into(), failed("a"));
        });
        assert!(sign_ins.next().await.unwrap().result.is_err());
        tally.send_modify(|t| {
            t.record(
                "a".into(),
                ServerSignIn {
                    server: "a".into(),
                    result: Err(SdkError::TimedOut("again".into())),
                },
            );
            t.settled = true;
        });
        assert_eq!(tally.borrow().done_len(), 1);
        let again = sign_ins.next().await.unwrap();
        assert!(matches!(again.result, Err(SdkError::TimedOut(_))));
        assert!(sign_ins.next().await.is_none());
    }

    /// One that ends early still gives what it did, then `None`.
    #[tokio::test]
    async fn an_end_before_the_total_gives_what_was_done() {
        let (tally, watching) = watch::channel(Tally::default());
        let mut sign_ins = ServerSignIns::new(watching);
        tally.send_modify(|t| {
            t.total = Some(3);
            t.record("a".into(), failed("a"));
        });
        drop(tally);
        assert_eq!(sign_ins.next().await.unwrap().server, "a");
        assert!(sign_ins.next().await.is_none());
    }
}

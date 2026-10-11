//! Which servers this device reaches through relays: a choice per server,
//! the client's [`crate::ClientOptions::relays`] where there is none.
//!
//! The endpoint itself always has relays as a way to dial (an empty relay
//! map: no home relay, nothing contacted at start, never probed), so the
//! choice is made per server here instead: a server whose relays are off is
//! dialled with no relay URL ([`crate::Client`]'s dialler leaves them out),
//! and an address lookup's record of it loses its relay URLs
//! ([`RelayFiltered`]). Iroh then has no relay to reach it through.
//!
//! The same wrapper notes what each lookup answered about each server, for
//! [`crate::Client::reachability`].

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{Stream, StreamExt};
use iroh::EndpointId;
use iroh::address_lookup::{AddressLookup, EndpointData, Error, Item};
use parking_lot::RwLock;

use crate::reachability::{self, LookupOutcome, ReachLog};

/// The relay choice of every server that has one, and the client's.
#[derive(Debug)]
pub(crate) struct RelayPolicy {
    /// [`crate::ClientOptions::relays`]: for a server with no choice.
    default: bool,
    chosen: RwLock<HashMap<EndpointId, bool>>,
}

impl RelayPolicy {
    pub(crate) fn new(default: bool) -> Self {
        Self {
            default,
            chosen: RwLock::default(),
        }
    }

    /// Whether `id` is reached through relays.
    pub(crate) fn allows(&self, id: &EndpointId) -> bool {
        self.chosen.read().get(id).copied().unwrap_or(self.default)
    }

    /// `id`'s choice from now on; `None` follows the client's.
    pub(crate) fn choose(&self, id: EndpointId, relays: Option<bool>) {
        let mut chosen = self.chosen.write();
        match relays {
            Some(relays) => chosen.insert(id, relays),
            None => chosen.remove(&id),
        };
    }
}

/// An address lookup whose records of a server with relays off carry no
/// relay URL: a pkarr record names the server's home relay, which iroh
/// would otherwise dial whatever the dialler gave it. What it answers is
/// noted in `log` under `name`, as the record said it.
#[derive(Debug)]
pub(crate) struct RelayFiltered<T> {
    inner: T,
    policy: Arc<RelayPolicy>,
    name: String,
    log: Arc<ReachLog>,
}

impl<T> RelayFiltered<T> {
    pub(crate) fn new(
        inner: T,
        policy: Arc<RelayPolicy>,
        name: impl Into<String>,
        log: Arc<ReachLog>,
    ) -> Self {
        Self {
            inner,
            policy,
            name: name.into(),
            log,
        }
    }
}

type Items = Pin<Box<dyn Stream<Item = Result<Item, Error>> + Send + 'static>>;

impl<T: AddressLookup> AddressLookup for RelayFiltered<T> {
    fn publish(&self, data: &EndpointData) {
        self.inner.publish(data);
    }

    fn resolve(&self, id: EndpointId) -> Option<Items> {
        let items = noted(self.inner.resolve(id)?, id, &self.name, &self.log);
        if self.policy.allows(&id) {
            return Some(items);
        }
        Some(Box::pin(items.map(|item| item.map(without_relays))))
    }
}

/// `items`, each noted in `log` as it comes, and an end with none as no
/// record. A stream iroh drops early (it reached the server another way)
/// notes only what came.
fn noted(items: Items, id: EndpointId, name: &str, log: &Arc<ReachLog>) -> Items {
    let any = Arc::new(AtomicBool::new(false));
    let (each, at_end) = (log.clone(), log.clone());
    let (name, end_name) = (name.to_owned(), name.to_owned());
    let seen = any.clone();
    let items = items.inspect(move |item| {
        seen.store(true, Ordering::Relaxed);
        let outcome = match item {
            Ok(item) => reachability::found(item),
            Err(e) => LookupOutcome::Error {
                message: format!("{e:#}"),
            },
        };
        each.looked_up(id, &name, outcome);
    });
    let end = futures_util::stream::once(async move {
        if !any.load(Ordering::Relaxed) {
            at_end.looked_up(id, &end_name, LookupOutcome::NoRecord);
        }
    })
    .filter_map(|()| async { None });
    Box::pin(items.chain(end))
}

/// `item` with its relay URLs left out.
fn without_relays(item: Item) -> Item {
    let mut info = item.endpoint_info().clone();
    info.data.clear_relay_urls();
    Item::new(info, item.provenance(), item.last_updated())
}

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use iroh::address_lookup::EndpointInfo;
    use iroh::{RelayUrl, SecretKey};

    use super::*;

    fn id(seed: u8) -> EndpointId {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    /// A lookup that knows every server at one address and one relay.
    #[derive(Debug)]
    struct Known;

    impl AddressLookup for Known {
        fn resolve(&self, id: EndpointId) -> Option<Items> {
            let relay: RelayUrl = "https://relay.example.org".parse().unwrap();
            let info = EndpointInfo::new(id)
                .with_relay_url(relay)
                .with_ip_addrs(vec!["203.0.113.7:7420".parse().unwrap()]);
            Some(Box::pin(stream::iter([Ok(Item::new(info, "test", None))])))
        }
    }

    async fn relays_found(lookup: &RelayFiltered<Known>, id: EndpointId) -> usize {
        let item = lookup.resolve(id).unwrap().next().await.unwrap().unwrap();
        assert_eq!(item.endpoint_info().ip_addrs().count(), 1);
        item.endpoint_info().relay_urls().count()
    }

    #[test]
    fn a_choice_wins_over_the_client_s_and_none_follows_it() {
        let off = RelayPolicy::new(false);
        off.choose(id(1), Some(true));
        assert!(off.allows(&id(1)));
        assert!(!off.allows(&id(2)));
        let on = RelayPolicy::new(true);
        on.choose(id(1), Some(false));
        assert!(!on.allows(&id(1)));
        on.choose(id(1), None);
        assert!(on.allows(&id(1)));
    }

    /// A lookup that knows no server.
    #[derive(Debug)]
    struct Empty;

    impl AddressLookup for Empty {
        fn resolve(&self, _: EndpointId) -> Option<Items> {
            Some(Box::pin(stream::empty()))
        }
    }

    /// A lookup that fails.
    #[derive(Debug)]
    struct Failing;

    impl AddressLookup for Failing {
        fn resolve(&self, _: EndpointId) -> Option<Items> {
            let error = Error::from_err_any("failing", std::io::Error::other("no route to dns"));
            Some(Box::pin(stream::iter([Err(error)])))
        }
    }

    async fn drain<T: AddressLookup>(lookup: &T, id: EndpointId) {
        lookup.resolve(id).unwrap().for_each(|_| async {}).await;
    }

    #[tokio::test]
    async fn what_each_lookup_answered_is_noted_per_server() {
        let policy = Arc::new(RelayPolicy::new(false));
        let log = Arc::new(ReachLog::default());
        drain(
            &RelayFiltered::new(Known, policy.clone(), "known", log.clone()),
            id(1),
        )
        .await;
        drain(
            &RelayFiltered::new(Empty, policy.clone(), "empty", log.clone()),
            id(1),
        )
        .await;
        drain(
            &RelayFiltered::new(Failing, policy, "failing", log.clone()),
            id(1),
        )
        .await;
        let results = log.lookups_of(&id(1));
        let of = |name: &str| {
            results
                .iter()
                .find(|r| r.lookup == name)
                .unwrap()
                .outcome
                .clone()
        };
        // As the record said it, relays included though this server's are
        // off: the record is what is being reported.
        assert_eq!(
            of("known"),
            LookupOutcome::Found {
                relays: vec!["https://relay.example.org/".into()],
                addrs: vec!["203.0.113.7:7420".into()],
            }
        );
        assert_eq!(of("empty"), LookupOutcome::NoRecord);
        let LookupOutcome::Error { message } = of("failing") else {
            panic!("{results:?}");
        };
        assert!(message.contains("no route to dns"), "{message}");
        assert!(log.lookups_of(&id(2)).is_empty());
    }

    #[tokio::test]
    async fn a_lookup_drops_the_relays_of_a_server_whose_relays_are_off() {
        let policy = Arc::new(RelayPolicy::new(false));
        policy.choose(id(1), Some(true));
        let lookup = RelayFiltered::new(Known, policy, "known", Arc::default());
        assert_eq!(relays_found(&lookup, id(1)).await, 1);
        assert_eq!(relays_found(&lookup, id(2)).await, 0);
    }
}

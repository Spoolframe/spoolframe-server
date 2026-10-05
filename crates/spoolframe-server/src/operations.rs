//! Bounded, process-local mutation outcomes. Pending work survives the
//! requesting stream; expired IDs and IDs from earlier processes are refused.

use spoolframe_protocol::rpc::{OPERATION_WINDOW_MS, Operation, RpcReply};
use spoolframe_protocol::{Error, Response};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::watch;

const MAX_OPERATIONS: usize = 4096;
const MAX_BYTES: usize = 64 << 20;
const RESERVATION: usize = spoolframe_protocol::codec::MAX_FRAME as usize;

#[derive(Debug)]
struct Entry {
    fingerprint: [u8; 32],
    expires: u64,
    bytes: usize,
    outcome: watch::Receiver<Option<RpcReply>>,
}

#[derive(Debug)]
pub(crate) struct Operations {
    entries: Mutex<HashMap<(String, String), Entry>>,
    max_operations: usize,
    max_bytes: usize,
    per_endpoint: usize,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            max_operations: MAX_OPERATIONS,
            max_bytes: MAX_BYTES,
            per_endpoint: 512,
        }
    }
}

pub(crate) struct Invocation {
    pub outcome: watch::Receiver<Option<RpcReply>>,
    pub execute: Option<watch::Sender<Option<RpcReply>>>,
}

impl Operations {
    /// Anonymous sign-in outcomes cannot consume authenticated capacity.
    pub fn anonymous() -> Self {
        Self {
            max_operations: 512,
            max_bytes: 8 << 20,
            per_endpoint: 64,
            ..Self::default()
        }
    }

    /// Recover an existing invocation without admitting unauthenticated work.
    /// Exact replays (including logout) keep their outcome after session revocation.
    pub fn lookup(
        &self,
        endpoint: &str,
        instance: &str,
        operation: &Operation,
        fingerprint: [u8; 32],
        now: u64,
    ) -> Result<Option<Invocation>, Error> {
        Self::validate(instance, operation, now)?;
        let mut entries = self.entries.lock().expect("never poisoned");
        entries.retain(|_, entry| entry.expires > now);
        let Some(entry) = entries.get(&(endpoint.to_owned(), operation.id.clone())) else {
            return Ok(None);
        };
        if entry.fingerprint != fingerprint {
            return Err(Error::OperationConflict);
        }
        Ok(Some(Invocation {
            outcome: entry.outcome.clone(),
            execute: None,
        }))
    }

    fn validate(instance: &str, operation: &Operation, now: u64) -> Result<(), Error> {
        if operation.instance != instance {
            return Err(Error::ServerRestarted);
        }
        if operation.expires_at_ms <= now {
            return Err(Error::OperationExpired);
        }
        if operation.expires_at_ms > now.saturating_add(OPERATION_WINDOW_MS)
            || operation.id.is_empty()
            || operation.id.len() > 128
        {
            return Err(Error::InvalidInput {
                message: "invalid operation ID or expiry".into(),
            });
        }
        Ok(())
    }

    pub fn begin(
        &self,
        endpoint: &str,
        instance: &str,
        operation: &Operation,
        fingerprint: [u8; 32],
        now: u64,
    ) -> Result<Invocation, Error> {
        Self::validate(instance, operation, now)?;
        let key = (endpoint.to_owned(), operation.id.clone());
        let mut entries = self.entries.lock().expect("never poisoned");
        entries.retain(|_, entry| entry.expires > now);
        if let Some(entry) = entries.get(&key) {
            if entry.fingerprint != fingerprint {
                return Err(Error::OperationConflict);
            }
            return Ok(Invocation {
                outcome: entry.outcome.clone(),
                execute: None,
            });
        }
        if entries.len() >= self.max_operations
            || entries.values().map(|entry| entry.bytes).sum::<usize>()
                > self.max_bytes - RESERVATION
            || entries.keys().filter(|(peer, _)| peer == endpoint).count() >= self.per_endpoint
        {
            return Err(Error::RateLimited {
                retry_after_ms: 1000,
            });
        }
        let (send, outcome) = watch::channel(None);
        entries.insert(
            key,
            Entry {
                fingerprint,
                expires: operation.expires_at_ms,
                bytes: RESERVATION,
                outcome: outcome.clone(),
            },
        );
        Ok(Invocation {
            outcome,
            execute: Some(send),
        })
    }

    pub fn finish(
        &self,
        endpoint: &str,
        id: &str,
        send: watch::Sender<Option<RpcReply>>,
        mut reply: RpcReply,
    ) {
        let size = spoolframe_protocol::codec::encode_limited(&reply, RESERVATION)
            .map(|bytes| bytes.len());
        let bytes = match size {
            Ok(size) if size <= RESERVATION => size,
            _ => {
                reply = Response::Err(Error::ReplyTooLarge {
                    bytes: size.unwrap_or(RESERVATION + 1) as u64,
                })
                .into();
                RESERVATION
            }
        };
        let mut entries = self.entries.lock().expect("never poisoned");
        if let Some(entry) = entries.get_mut(&(endpoint.to_owned(), id.to_owned())) {
            entry.bytes = bytes;
        }
        send.send_replace(Some(reply));
    }
}

pub(crate) fn now_ms() -> u64 {
    // Wall-clock corrections must not make an expired and evicted ID valid
    // again. Anchor once, then advance only with the monotonic process clock.
    static CLOCK: std::sync::OnceLock<(u64, std::time::Instant)> = std::sync::OnceLock::new();
    let (base, started) = CLOCK.get_or_init(|| {
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_millis()
            .try_into()
            .expect("Unix milliseconds fit in u64");
        (base, std::time::Instant::now())
    });
    base.saturating_add(started.elapsed().as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::Reply;

    fn operation() -> Operation {
        Operation {
            id: "request-1".into(),
            instance: "process-1".into(),
            expires_at_ms: 10_000,
        }
    }

    #[test]
    fn anonymous_capacity_is_isolated_and_replays_still_work_when_full() {
        let anonymous = Operations::anonymous();
        let authenticated = Operations::default();
        let mut op = operation();
        // Completed failures still count against the anonymous endpoint quota.
        for i in 0..anonymous.per_endpoint {
            op.id = i.to_string();
            let invocation = anonymous
                .begin("peer", "process-1", &op, [1; 32], 1)
                .unwrap();
            anonymous.finish(
                "peer",
                &op.id,
                invocation.execute.unwrap(),
                Response::Err(Error::Unauthenticated).into(),
            );
        }
        op.id = "overflow".into();
        assert!(matches!(
            anonymous.begin("peer", "process-1", &op, [1; 32], 1),
            Err(Error::RateLimited { .. })
        ));
        assert!(
            authenticated
                .begin("peer", "process-1", &op, [1; 32], 1)
                .is_ok()
        );
        op.id = "0".into();
        assert!(
            anonymous
                .lookup("peer", "process-1", &op, [1; 32], 1)
                .unwrap()
                .unwrap()
                .execute
                .is_none()
        );
        assert!(matches!(
            anonymous.lookup("peer", "process-1", &op, [2; 32], 1),
            Err(Error::OperationConflict)
        ));
        // Independent endpoints can fill only the anonymous pool.
        let mut pending = Vec::new();
        for i in 0..8 {
            pending.push(anonymous.begin(&format!("other-{i}"), "process-1", &op, [1; 32], 1));
        }
        assert!(pending.iter().any(Result::is_err));
        assert!(
            authenticated
                .begin("legitimate", "process-1", &op, [1; 32], 1)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn simultaneous_duplicates_share_the_pending_and_completed_outcome() {
        let cache = Operations::default();
        let op = operation();
        let first = cache
            .begin("endpoint", "process-1", &op, [1; 32], 1)
            .unwrap();
        let mut duplicate = cache
            .begin("endpoint", "process-1", &op, [1; 32], 2)
            .unwrap();
        assert!(duplicate.execute.is_none());
        let reply: RpcReply = Response::Ok(Reply::Done).into();
        cache.finish("endpoint", &op.id, first.execute.unwrap(), reply.clone());
        duplicate.outcome.changed().await.unwrap();
        assert_eq!(*duplicate.outcome.borrow(), Some(reply.clone()));
        let replay = cache
            .begin("endpoint", "process-1", &op, [1; 32], 3)
            .unwrap();
        assert!(replay.execute.is_none());
        assert_eq!(*replay.outcome.borrow(), Some(reply));
    }

    #[test]
    fn an_id_cannot_change_payload_or_outlive_its_process_and_expiry() {
        let cache = Operations::default();
        let op = operation();
        let _pending = cache
            .begin("endpoint", "process-1", &op, [1; 32], 1)
            .unwrap();
        assert!(matches!(
            cache.begin("endpoint", "process-1", &op, [2; 32], 1),
            Err(Error::OperationConflict)
        ));
        assert!(matches!(
            cache.begin("endpoint", "process-2", &op, [1; 32], 1),
            Err(Error::ServerRestarted)
        ));
        assert!(matches!(
            cache.begin("endpoint", "process-1", &op, [1; 32], op.expires_at_ms),
            Err(Error::OperationExpired)
        ));
        assert!(
            cache
                .begin("another-endpoint", "process-1", &op, [1; 32], 1)
                .unwrap()
                .execute
                .is_some()
        );
    }

    #[test]
    fn admission_never_evicts_an_unexpired_invocation() {
        let cache = Operations::default();
        let mut op = operation();
        let mut pending = Vec::new();
        for i in 0..MAX_BYTES / RESERVATION {
            op.id = i.to_string();
            pending.push(
                cache
                    .begin("endpoint", "process-1", &op, [1; 32], 1)
                    .unwrap(),
            );
        }
        op.id = "overflow".into();
        assert!(matches!(
            cache.begin("endpoint", "process-1", &op, [1; 32], 1),
            Err(Error::RateLimited { .. })
        ));
        op.id = "0".into();
        assert!(
            cache
                .begin("endpoint", "process-1", &op, [1; 32], 1)
                .unwrap()
                .execute
                .is_none()
        );
        op.id = "fresh".into();
        op.expires_at_ms += OPERATION_WINDOW_MS;
        assert!(
            cache
                .begin("endpoint", "process-1", &op, [1; 32], 10_001)
                .unwrap()
                .execute
                .is_some()
        );
    }
}

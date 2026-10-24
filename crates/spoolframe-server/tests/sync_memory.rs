//! What the end of a paired server's sync holds in heap at once (C8-02),
//! measured by a counting allocator.
//!
//! A binary of its own (one test binary per crate is the rule elsewhere):
//! a `#[global_allocator]` is process-wide, and the rows are read on the
//! store's own thread, so the count is too. The binary holds this one test,
//! so nothing else allocates beside it.
//!
//! The end of a sync read every staged item's body into one vector before
//! writing any: an answer within the page and item limits, its summaries
//! padded with characters JSON escapes to six bytes, made gigabytes of it.
//! The peak of bytes held is the measure: unlike a timing it does not
//! depend on the machine, and it is what the finding is about. SQLite's own
//! memory is the C library's, not counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use spoolframe_protocol::federation::{MAX_ITEM_SUMMARY, RemoteItem, RemoteKind};
use spoolframe_server::store::Store;
use spoolframe_server::store::federation::NewPeer;
use spoolframe_server::store::federation_cache::{CacheSynced, OriginLimits};

struct Counting;

static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn held(bytes: usize) {
    let now = HELD.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn freed(bytes: usize) {
    HELD.fetch_sub(bytes, Ordering::Relaxed);
}

// SAFETY: every call is passed to `System` unchanged; only a count is kept.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            held(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            held(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        freed(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            freed(layout.size());
            held(new_size);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const ORIGIN: &str = "aaaaaaaaaaaaaaaa";

/// Film `n`, its summary as long as the protocol lets it be, all NULs.
fn padded(n: usize) -> RemoteItem {
    RemoteItem {
        id: format!("plex:movie:{n}"),
        kind: RemoteKind::Movie,
        title: format!("Film {n}"),
        parent: None,
        grandparent: None,
        parent_title: None,
        grandparent_title: None,
        index: None,
        parent_index: None,
        year: None,
        date: None,
        summary: Some("\0".repeat(MAX_ITEM_SUMMARY)),
        content_rating: None,
        duration_ms: None,
        count: None,
        genres: Vec::new(),
        artwork: Vec::new(),
        external_ids: Vec::new(),
        hash: String::new(),
    }
    .hashed()
}

/// C8-02: 2,000 films of padded summaries stage about 94 MiB of JSON;
/// writing them holds a batch of it at a time, a few MiB, not the whole.
#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_a_sync_holds_a_batch_of_it_not_the_whole() {
    let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-memory").await;
    let store = Store::open(&dir).await.unwrap();
    let peer = store
        .add_peer(
            NewPeer {
                server_id: ORIGIN,
                endpoint_id: &"0a".repeat(32),
                display_name: "Alice's server",
                addrs: &[],
                relays: &[],
            },
            "bob",
        )
        .await
        .unwrap();
    store.begin_cache_sync(ORIGIN, "plex:1").await.unwrap();
    let mut staged = 0u64;
    for page in 0..20 {
        let items: Vec<RemoteItem> = (page * 100..page * 100 + 100).map(padded).collect();
        staged += store
            .stage_cache_page(ORIGIN, "plex:1", &items, &[])
            .await
            .unwrap();
    }
    assert!(staged > 90 << 20, "{staged} bytes staged");

    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let synced = store
        .finish_cache_sync(ORIGIN, &peer.id, "plex:1", "c1", true, OriginLimits::NONE)
        .await
        .unwrap();
    assert_eq!(synced, CacheSynced::Written);
    let most = PEAK.load(Ordering::Relaxed) - before;
    assert!(
        most < 32 << 20,
        "{most} bytes held at once to write {staged} staged"
    );
    let (_, total) = store
        .cached_page(ORIGIN, "plex:1", "movie", 0, 1)
        .await
        .unwrap();
    assert_eq!(total, 2_000);
}

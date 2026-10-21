use super::*;
use crate::storage::MemoryStorage;

const S: &str = "server-key";
const PARENT: &str = "local:parent";
const CHILD: &str = "local:child";

/// Locks kept in `storage`, with the parent's PIN `1234` asked for here
/// and known from an online unlock, the parent's profile shown.
async fn parent_locked(storage: Arc<MemoryStorage>) -> Arc<Locks> {
    let locks = Arc::new(Locks::default());
    locks.load(storage).await.unwrap();
    locks.unlocked_online(S, PARENT, "1234").await;
    locks.set_active(S, CHILD);
    locks.set_active(S, PARENT);
    locks
}

#[tokio::test]
async fn a_profile_with_no_pin_asked_here_is_open() {
    let locks = Arc::new(Locks::default());
    locks.seen(S, PARENT, false);
    locks.set_active(S, PARENT);
    assert!(!locks.is_locked(S, PARENT));
}

#[tokio::test]
async fn a_profile_whose_pin_is_asked_here_starts_locked_and_its_pin_opens_it() {
    let locks = parent_locked(Arc::default()).await;
    assert!(locks.is_locked(S, PARENT));
    assert!(matches!(
        locks.unlock_offline("0000").await,
        Err(SdkError::InvalidCredentials)
    ));
    assert!(locks.is_locked(S, PARENT));
    locks.unlock_offline("1234").await.unwrap();
    assert!(!locks.is_locked(S, PARENT));
}

/// The child picks the parent's profile after it was unlocked and left:
/// locked again.
#[tokio::test]
async fn switching_away_and_back_locks_it_again() {
    let locks = parent_locked(Arc::default()).await;
    locks.unlock_offline("1234").await.unwrap();
    locks.set_active(S, PARENT);
    assert!(
        !locks.is_locked(S, PARENT),
        "the same profile again stays open"
    );
    locks.set_active(S, CHILD);
    assert!(!locks.is_locked(S, CHILD));
    locks.set_active(S, PARENT);
    assert!(locks.is_locked(S, PARENT));
}

#[tokio::test]
async fn after_three_wrong_pins_the_next_waits() {
    let locks = parent_locked(Arc::default()).await;
    for _ in 0..FREE_TRIES {
        assert!(matches!(
            locks.unlock_offline("0000").await,
            Err(SdkError::InvalidCredentials)
        ));
    }
    assert!(matches!(
        locks.unlock_offline("1234").await,
        Err(SdkError::RateLimited { retry_after }) if retry_after > Duration::ZERO
    ));
    assert!(locks.is_locked(S, PARENT));
}

/// Ten wrong in a row: neither the PIN nor the owner opens it offline,
/// until an online unlock.
#[tokio::test]
async fn ten_wrong_pins_keep_it_locked_until_an_online_unlock() {
    let locks = parent_locked(Arc::default()).await;
    // Nine wrong, long enough ago that the tenth need not wait.
    locks
        .profiles
        .lock()
        .get_mut(&key(S, PARENT))
        .unwrap()
        .wrong = MAX_WRONG - 1;
    assert!(matches!(
        locks.unlock_offline("0000").await,
        Err(SdkError::SignInRequired)
    ));
    assert!(matches!(
        locks.unlock_offline("1234").await,
        Err(SdkError::SignInRequired)
    ));
    assert!(matches!(
        locks.unlock_by_owner(),
        Err(SdkError::SignInRequired)
    ));
    locks.unlocked_online(S, PARENT, "1234").await;
    assert!(!locks.is_locked(S, PARENT));
    locks.set_active(S, CHILD);
    locks.set_active(S, PARENT);
    locks.unlock_offline("1234").await.unwrap();
}

/// A PIN never given here since the lock came: the owner's own unlock
/// opens it.
#[tokio::test]
async fn with_no_pin_known_here_the_owner_unlocks() {
    let locks = Arc::new(Locks::default());
    locks.session_locked(S, PARENT, true);
    locks.set_active(S, PARENT);
    assert!(locks.is_locked(S, PARENT));
    assert!(matches!(
        locks.unlock_offline("1234").await,
        Err(SdkError::NoOfflinePin)
    ));
    locks.unlock_by_owner().unwrap();
    assert!(!locks.is_locked(S, PARENT));
}

/// Locked offline as the device last knew it: what is kept comes back
/// with the client.
#[tokio::test]
async fn the_lock_is_kept_across_restarts() {
    let storage = Arc::new(MemoryStorage::default());
    parent_locked(storage.clone()).await;
    let again = Arc::new(Locks::default());
    again.load(storage).await.unwrap();
    again.set_active(S, PARENT);
    assert!(again.is_locked(S, PARENT));
    again.unlock_offline("1234").await.unwrap();
}

/// A session that started unlocked does not turn the lock off: its
/// connection may have been unlocked. The server saying it is off does.
#[tokio::test]
async fn only_the_server_s_setting_turns_it_off() {
    let locks = parent_locked(Arc::default()).await;
    locks.session_locked(S, PARENT, false);
    assert!(locks.is_locked(S, PARENT));
    locks.seen(S, PARENT, false);
    assert!(!locks.is_locked(S, PARENT));
    // And the hash with it: turned on again, the PIN is unknown here.
    locks.seen(S, PARENT, true);
    assert!(matches!(
        locks.unlock_offline("1234").await,
        Err(SdkError::NoOfflinePin)
    ));
}

/// A PIN set here replaces the one kept.
#[tokio::test]
async fn a_pin_set_here_is_the_one_checked() {
    let locks = parent_locked(Arc::default()).await;
    locks.pin_set(S, PARENT, "987654").await;
    assert!(matches!(
        locks.unlock_offline("1234").await,
        Err(SdkError::InvalidCredentials)
    ));
    locks.unlock_offline("987654").await.unwrap();
}

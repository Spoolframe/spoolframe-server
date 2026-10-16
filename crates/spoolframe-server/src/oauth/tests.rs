use std::time::Duration;

use spoolframe_types::Role;

use super::*;
use crate::clock::Clock;
use crate::store::tests::{TestStore, test_store};

const RETURN: &str = "https://claude.ai/api/mcp/auth_callback";
/// A PKCE verifier and its S256 challenge.
const VERIFIER: &str = "dBjftJeZ4CVP-mJ92K9b9sFvMDqpH9VZlIfqXMQ7fFZ";
const CHALLENGE: &str = "v5EpdpfSvdxcI3-JDrrt8W3BNL0gE6xL5Fxd211vTEY";

struct World {
    oauth: OAuth,
    mcp: McpSwitch,
    clock: Clock,
    alice: LocalUserId,
    _store: TestStore,
}

async fn world(test: &str) -> World {
    let store = test_store(&format!("oauth-{test}")).await;
    let clock = Clock::default();
    let alice = store
        .add_user("alice", None, "phc", Role::Consumer)
        .await
        .unwrap();
    let mcp = McpSwitch::default();
    World {
        oauth: OAuth::new(store.store.clone().with_clock(clock.clone()), mcp.clone()),
        mcp,
        clock,
        alice,
        _store: store,
    }
}

fn request(client_id: &str, scope: &str) -> AuthorizeRequest {
    AuthorizeRequest {
        response_type: Some("code".into()),
        client_id: Some(client_id.into()),
        redirect_uri: Some(RETURN.into()),
        scope: Some(scope.into()),
        state: Some("xyz".into()),
        code_challenge: Some(CHALLENGE.into()),
        code_challenge_method: Some("S256".into()),
        resource: None,
    }
}

/// The value of `name` in `url`'s query.
fn param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

impl World {
    async fn client(&self) -> String {
        self.oauth
            .register(Some("Claude"), &[RETURN.to_owned()])
            .await
            .unwrap()
            .id
    }

    /// `user` consents to `scopes` for `client_id`: the code.
    async fn code(&self, client_id: &str, user: LocalUserId, scopes: &[&str]) -> String {
        let id = self
            .oauth
            .authorize(&request(client_id, "library:read playlists:write"))
            .await
            .unwrap();
        let chosen: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        let to = self
            .oauth
            .decide(&id, user, false, true, &chosen)
            .await
            .unwrap();
        param(&to, "code").unwrap()
    }

    async fn tokens(&self, client_id: &str) -> Tokens {
        let code = self.code(client_id, self.alice, &["library:read"]).await;
        self.oauth
            .exchange(client_id, &code, RETURN, VERIFIER)
            .await
            .unwrap()
    }

    async fn works(&self, access: &str) -> bool {
        self.oauth.access(access).await.unwrap().is_some()
    }
}

#[test]
fn a_scope_the_server_does_not_have_is_dropped() {
    assert_eq!(
        offered("library:read playback playlists:write", false),
        [Scope::LibraryRead, Scope::PlaylistsWrite]
    );
    // Nothing the server has: the least it can offer.
    assert_eq!(offered("playback", false), [Scope::LibraryRead]);
    assert_eq!(offered("", true), [Scope::LibraryRead]);
}

#[test]
fn admin_is_offered_to_admins_only() {
    assert_eq!(offered("admin library:read", false), [Scope::LibraryRead]);
    assert_eq!(
        offered("admin library:read", true),
        [Scope::LibraryRead, Scope::Admin]
    );
}

#[test]
fn pkce_takes_the_verifier_the_challenge_was_made_from_only() {
    assert!(pkce_matches(VERIFIER, CHALLENGE));
    let other = "dBjftJeZ4CVP-mJ92K9b9sFvMDqpH9VZlIfqXMQ7fFz";
    assert!(!pkce_matches(other, CHALLENGE));
    assert!(valid_verifier(VERIFIER));
    assert!(!valid_verifier("short"));
    assert!(!valid_verifier(&format!("{VERIFIER} ")));
}

#[test]
fn a_redirect_uri_is_https_or_loopback() {
    for uri in [
        RETURN,
        "http://127.0.0.1:33418/callback",
        "http://localhost/cb",
        "http://[::1]:9/cb",
    ] {
        assert!(check_redirect(uri).is_ok(), "{uri}");
    }
    for uri in [
        "http://claude.ai/cb",
        "http://192.168.1.4/cb",
        "cursor://anysphere.cursor-retrieval/oauth",
        "javascript:alert(1)",
        "https://claude.ai/cb#frag",
        "https://user:pw@claude.ai/cb",
        "not a url",
    ] {
        assert!(check_redirect(uri).is_err(), "{uri}");
    }
}

#[test]
fn a_loopback_redirect_may_change_its_port_and_nothing_else() {
    let registered = "http://127.0.0.1:1000/callback";
    assert!(redirect_matches(
        registered,
        "http://127.0.0.1:53211/callback"
    ));
    assert!(!redirect_matches(
        registered,
        "http://127.0.0.1:53211/other"
    ));
    assert!(!redirect_matches(
        registered,
        "http://localhost:53211/callback"
    ));
    // Anywhere else, exactly the string registered.
    assert!(redirect_matches(RETURN, RETURN));
    assert!(!redirect_matches(
        RETURN,
        "https://claude.ai:443/api/mcp/auth_callback"
    ));
    assert!(!redirect_matches(RETURN, &format!("{RETURN}?x=1")));
    assert!(!redirect_matches(
        RETURN,
        "https://claude.ai.evil/api/mcp/auth_callback"
    ));
}

#[test]
fn consent_shows_the_host_the_code_returns_to() {
    assert_eq!(return_host(RETURN), "claude.ai");
    assert_eq!(return_host("http://127.0.0.1:4000/cb"), "127.0.0.1");
}

#[tokio::test]
async fn a_registration_is_held_to_its_rules() {
    let w = world("register").await;
    let long = "x".repeat(MAX_CLIENT_NAME + 1);
    let refused = [
        (Some(long.as_str()), vec![RETURN.to_owned()]),
        (Some("Bad\u{7}name"), vec![RETURN.to_owned()]),
        (Some("Claude"), vec![]),
        (Some("Claude"), vec!["http://claude.ai/cb".to_owned()]),
    ];
    for (name, uris) in refused {
        assert!(
            matches!(
                w.oauth.register(name, &uris).await,
                Err(RegisterError::Invalid { .. })
            ),
            "{name:?} {uris:?}"
        );
    }
    let unnamed = w.oauth.register(None, &[RETURN.to_owned()]).await.unwrap();
    assert_eq!(unnamed.name, "Unnamed app");
}

/// A client none of whose redirect URIs an admin allows is refused, and
/// one of them allowed is enough.
#[tokio::test]
async fn a_registration_needs_a_redirect_uri_the_admin_allows() {
    let w = world("register-allowed").await;
    let elsewhere = "https://agent.example/cb".to_owned();
    let refused = w
        .oauth
        .register(Some("Agent"), std::slice::from_ref(&elsewhere))
        .await;
    assert!(
        matches!(
            refused,
            Err(RegisterError::Invalid {
                code: "invalid_redirect_uri",
                ref message,
            }) if message.contains("Settings › MCP")
        ),
        "{refused:?}"
    );
    w.oauth
        .register(Some("Agent"), &[elsewhere.clone(), RETURN.to_owned()])
        .await
        .unwrap();
    w.mcp.set_allowed(vec![
        redirects::RedirectPattern::parse("Agent", "https://agent.example/cb").unwrap(),
    ]);
    w.oauth.register(Some("Agent"), &[elsewhere]).await.unwrap();
    assert!(
        w.oauth
            .register(Some("Claude"), &[RETURN.to_owned()])
            .await
            .is_err()
    );
}

/// An app the admin removes signs no one in from then on: `/authorize`
/// refuses it, and so does consent already on screen, with no redirect.
#[tokio::test]
async fn an_app_removed_from_the_list_signs_no_one_in() {
    let w = world("removed").await;
    let client = w.client().await;
    let waiting = w
        .oauth
        .authorize(&request(&client, "library:read"))
        .await
        .unwrap();
    w.mcp.set_allowed(Vec::new());
    assert!(matches!(
        w.oauth.authorize(&request(&client, "library:read")).await,
        Err(AuthorizeError::Broken(_))
    ));
    let chosen = ["library:read".to_owned()];
    assert!(matches!(
        w.oauth
            .decide(&waiting, w.alice, false, true, &chosen)
            .await,
        Err(DecideError::Invalid(_))
    ));
}

/// A change to a good request.
type Change = Box<dyn Fn(&mut AuthorizeRequest)>;

#[tokio::test]
async fn a_broken_authorization_request_is_refused_before_any_consent() {
    let w = world("broken").await;
    let client = w.client().await;
    let cases: [(&str, Change); 6] = [
        (
            "unknown client",
            Box::new(|r| r.client_id = Some("nope".into())),
        ),
        (
            "unregistered return",
            Box::new(|r| r.redirect_uri = Some("https://evil.example/cb".into())),
        ),
        ("no return", Box::new(|r| r.redirect_uri = None)),
        ("no PKCE", Box::new(|r| r.code_challenge = None)),
        (
            "plain PKCE",
            Box::new(|r| r.code_challenge_method = Some("plain".into())),
        ),
        (
            "a token",
            Box::new(|r| r.response_type = Some("token".into())),
        ),
    ];
    for (what, change) in cases {
        let mut asked = request(&client, "library:read");
        change(&mut asked);
        assert!(
            matches!(
                w.oauth.authorize(&asked).await,
                Err(AuthorizeError::Broken(_))
            ),
            "{what}"
        );
    }
}

#[tokio::test]
async fn consent_names_the_registered_client_and_offers_what_the_server_has() {
    let w = world("pending").await;
    let client = w.client().await;
    let id = w
        .oauth
        .authorize(&request(&client, "library:read playback admin"))
        .await
        .unwrap();
    let view = w.oauth.pending(&id, false).unwrap();
    assert_eq!(view.client_name, "Claude");
    assert_eq!(view.return_host, "claude.ai");
    assert_eq!(view.scopes, [Scope::LibraryRead]);
    assert_eq!(
        w.oauth.pending(&id, true).unwrap().scopes,
        [Scope::LibraryRead, Scope::Admin]
    );
    // Over after ten minutes.
    w.clock.advance(PENDING);
    assert_eq!(w.oauth.pending(&id, false), None);
}

#[tokio::test]
async fn a_request_for_playback_is_granted_without_it() {
    let w = world("playback").await;
    let client = w.client().await;
    let id = w
        .oauth
        .authorize(&request(&client, "library:read playback"))
        .await
        .unwrap();
    // Not offered, so not grantable either.
    let refused = w
        .oauth
        .decide(&id, w.alice, false, true, &["playback".into()])
        .await;
    assert!(matches!(refused, Err(DecideError::Invalid(_))));
    let to = w
        .oauth
        .decide(&id, w.alice, false, true, &["library:read".into()])
        .await
        .unwrap();
    let tokens = w
        .oauth
        .exchange(&client, &param(&to, "code").unwrap(), RETURN, VERIFIER)
        .await
        .unwrap();
    assert_eq!(tokens.scope, "library:read");
    assert_eq!(param(&to, "state").as_deref(), Some("xyz"));
}

#[tokio::test]
async fn a_non_admin_cannot_be_granted_admin() {
    let w = world("no-admin").await;
    let client = w.client().await;
    let id = w
        .oauth
        .authorize(&request(&client, "admin library:read"))
        .await
        .unwrap();
    let refused = w
        .oauth
        .decide(&id, w.alice, false, true, &["admin".into()])
        .await;
    assert!(matches!(refused, Err(DecideError::Invalid(_))));
}

#[tokio::test]
async fn denying_sends_access_denied_and_grants_nothing() {
    let w = world("deny").await;
    let client = w.client().await;
    let id = w
        .oauth
        .authorize(&request(&client, "library:read"))
        .await
        .unwrap();
    let to = w
        .oauth
        .decide(&id, w.alice, false, false, &[])
        .await
        .unwrap();
    assert_eq!(param(&to, "error").as_deref(), Some("access_denied"));
    assert_eq!(param(&to, "state").as_deref(), Some("xyz"));
    assert_eq!(param(&to, "code"), None);
    // Answered once.
    assert!(matches!(
        w.oauth
            .decide(&id, w.alice, false, true, &["library:read".into()])
            .await,
        Err(DecideError::NoSuchRequest)
    ));
}

#[tokio::test]
async fn a_code_is_traded_once_by_its_client_with_its_verifier() {
    let w = world("exchange").await;
    let client = w.client().await;
    let other = w.client().await;

    let code = w.code(&client, w.alice, &["library:read"]).await;
    let wrong = "dBjftJeZ4CVP-mJ92K9b9sFvMDqpH9VZlIfqXMQ7fFz";
    assert!(matches!(
        w.oauth.exchange(&client, &code, RETURN, wrong).await,
        Err(TokenError::InvalidGrant)
    ));
    // Spent by the failed try.
    assert!(matches!(
        w.oauth.exchange(&client, &code, RETURN, VERIFIER).await,
        Err(TokenError::InvalidGrant)
    ));

    let code = w.code(&client, w.alice, &["library:read"]).await;
    assert!(matches!(
        w.oauth.exchange(&other, &code, RETURN, VERIFIER).await,
        Err(TokenError::InvalidGrant)
    ));
    let code = w.code(&client, w.alice, &["library:read"]).await;
    assert!(matches!(
        w.oauth
            .exchange(&client, &code, "https://claude.ai/other", VERIFIER)
            .await,
        Err(TokenError::InvalidGrant)
    ));

    let code = w.code(&client, w.alice, &["library:read"]).await;
    let tokens = w
        .oauth
        .exchange(&client, &code, RETURN, VERIFIER)
        .await
        .unwrap();
    assert!(tokens.access_token.starts_with("sfo_"));
    let who = w.oauth.access(&tokens.access_token).await.unwrap().unwrap();
    assert_eq!(who.user, w.alice);
    assert_eq!(who.client_name, "Claude");
    assert_eq!(who.scopes, "library:read");
    // A second use is a leak: the tokens the first one got stop too.
    assert!(matches!(
        w.oauth.exchange(&client, &code, RETURN, VERIFIER).await,
        Err(TokenError::InvalidGrant)
    ));
    assert!(!w.works(&tokens.access_token).await);
}

#[tokio::test]
async fn a_code_lasts_a_minute() {
    let w = world("code-expiry").await;
    let client = w.client().await;
    let code = w.code(&client, w.alice, &["library:read"]).await;
    w.clock.advance(CODE);
    assert!(matches!(
        w.oauth.exchange(&client, &code, RETURN, VERIFIER).await,
        Err(TokenError::InvalidGrant)
    ));
}

#[tokio::test]
async fn an_access_token_lasts_an_hour() {
    let w = world("access-expiry").await;
    let client = w.client().await;
    let tokens = w.tokens(&client).await;
    w.clock.advance(ACCESS_TOKEN - Duration::from_secs(5));
    assert!(w.works(&tokens.access_token).await);
    w.clock.advance(Duration::from_secs(5));
    assert!(!w.works(&tokens.access_token).await);
}

#[tokio::test]
async fn a_refresh_token_is_used_once_and_a_second_use_ends_the_grant() {
    let w = world("refresh").await;
    let client = w.client().await;
    let other = w.client().await;
    let first = w.tokens(&client).await;
    // Another client's refresh is not this one's.
    assert!(matches!(
        w.oauth.refresh(&other, &first.refresh_token).await,
        Err(TokenError::InvalidGrant)
    ));
    let second = w
        .oauth
        .refresh(&client, &first.refresh_token)
        .await
        .unwrap();
    assert_eq!(second.scope, "library:read");
    assert!(w.works(&second.access_token).await);
    // The first refresh token again: someone else has it.
    assert!(matches!(
        w.oauth.refresh(&client, &first.refresh_token).await,
        Err(TokenError::InvalidGrant)
    ));
    assert!(!w.works(&second.access_token).await);
    assert!(matches!(
        w.oauth.refresh(&client, &second.refresh_token).await,
        Err(TokenError::InvalidGrant)
    ));
}

#[tokio::test]
async fn a_refresh_token_lasts_thirty_days_from_its_last_use() {
    let w = world("refresh-expiry").await;
    let client = w.client().await;
    let first = w.tokens(&client).await;
    w.clock.advance(REFRESH_TOKEN - Duration::from_secs(60));
    let second = w
        .oauth
        .refresh(&client, &first.refresh_token)
        .await
        .unwrap();
    w.clock.advance(REFRESH_TOKEN - Duration::from_secs(60));
    let third = w
        .oauth
        .refresh(&client, &second.refresh_token)
        .await
        .unwrap();
    w.clock.advance(REFRESH_TOKEN);
    assert!(matches!(
        w.oauth.refresh(&client, &third.refresh_token).await,
        Err(TokenError::InvalidGrant)
    ));
}

#[tokio::test]
async fn a_disabled_user_s_tokens_stop_at_once() {
    let w = world("disabled").await;
    let client = w.client().await;
    let tokens = w.tokens(&client).await;
    w.oauth.store().disable_user("alice").await.unwrap();
    assert!(!w.works(&tokens.access_token).await);
    assert!(matches!(
        w.oauth.refresh(&client, &tokens.refresh_token).await,
        Err(TokenError::InvalidGrant)
    ));
}

#[tokio::test]
async fn a_revoked_grant_s_tokens_stop_at_once() {
    let w = world("revoked").await;
    let client = w.client().await;
    let tokens = w.tokens(&client).await;
    let grant = w.oauth.access(&tokens.access_token).await.unwrap().unwrap();
    assert!(
        w.oauth
            .store()
            .revoke_oauth_grant(&grant.grant_id)
            .await
            .unwrap()
    );
    assert!(!w.works(&tokens.access_token).await);
    assert!(matches!(
        w.oauth.refresh(&client, &tokens.refresh_token).await,
        Err(TokenError::InvalidGrant)
    ));
}

#[tokio::test]
async fn consenting_again_replaces_the_grant() {
    let w = world("again").await;
    let client = w.client().await;
    let old = w.tokens(&client).await;
    let new = w.tokens(&client).await;
    assert!(!w.works(&old.access_token).await);
    assert!(w.works(&new.access_token).await);
}

#[tokio::test]
async fn calls_today_counts_a_client_s_calls_and_starts_over_the_next_day() {
    let w = world("calls").await;
    let client = w.client().await;
    let tokens = w.tokens(&client).await;
    let grant = w.oauth.access(&tokens.access_token).await.unwrap().unwrap();
    let store = w.oauth.store();
    assert_eq!(
        store.oauth_calls(&grant.grant_id).await.unwrap(),
        Some((0, None))
    );
    store.note_oauth_call(&grant.grant_id).await.unwrap();
    store.note_oauth_call(&grant.grant_id).await.unwrap();
    let (calls, last) = store.oauth_calls(&grant.grant_id).await.unwrap().unwrap();
    assert_eq!(calls, 2);
    assert_eq!(last, Some(store.now()));
    w.clock.advance(Duration::from_secs(24 * 3600));
    assert_eq!(
        store.oauth_calls(&grant.grant_id).await.unwrap().unwrap().0,
        0
    );
    store.note_oauth_call(&grant.grant_id).await.unwrap();
    assert_eq!(
        store.oauth_calls(&grant.grant_id).await.unwrap().unwrap().0,
        1
    );
}

#[tokio::test]
async fn a_client_nobody_granted_anything_goes_after_a_day() {
    let w = world("purge").await;
    let unused = w.client().await;
    let used = w.client().await;
    w.tokens(&used).await;
    w.clock.advance(UNUSED_CLIENT);
    w.oauth
        .store()
        .purge_oauth(UNUSED_CLIENT.as_secs() as i64)
        .await
        .unwrap();
    assert!(
        w.oauth
            .store()
            .oauth_client(&unused)
            .await
            .unwrap()
            .is_none()
    );
    assert!(w.oauth.store().oauth_client(&used).await.unwrap().is_some());
}

#[tokio::test]
async fn one_address_registers_a_few_clients_an_hour() {
    let w = world("register-limit").await;
    let here = Some("192.0.2.7".parse().unwrap());
    let there = Some("192.0.2.8".parse().unwrap());
    for _ in 0..MAX_REGISTRATIONS {
        assert!(w.oauth.may_register(here));
    }
    assert!(!w.oauth.may_register(here));
    assert!(w.oauth.may_register(there));
    w.clock.advance(REGISTRATION_WINDOW);
    assert!(w.oauth.may_register(here));
}

//! P8-07: subtitle search on a paired server's items. SB passes its users'
//! searches, downloads and removals of SA's film to SA, which asks its own
//! site (a stand-in on loopback) and keeps the file; every user of SB's
//! together gets one daily limit on SA, after SB's own limit per user.

use spoolframe_admin_api::{AdminApiClient, SubtitlePairUsage};
use spoolframe_protocol::{Hello, PROTOCOL_VERSION, Request, Response, SessionId};
use spoolframe_server::subtitle_downloads::Settings;
use spoolframe_types::{Platform, QuotaOf, SubtitleDownloaded, SubtitleSearch};
use wiremock::MockServer;

use super::*;
use crate::downloads::write_film;
use crate::playback_debug::admin_beside;
use crate::subtitle_downloads as subtitles_it;
use crate::support::TestClient;

const FEDERATION: &str = spoolframe_protocol::experimental::FEDERATION;

/// SA with real ffmpeg, its film, and a stand-in site, with a daily limit
/// of `per_peer` for each pair (and none per user); SB holding each user
/// to `sb_per_user`. Bob and Carol are SB's users.
async fn sharing_a_film(test: &str, site: &MockServer, per_peer: u32, sb_per_user: u32) -> Pair {
    let sa_sites = Settings {
        daily_limit_per_user: 0,
        daily_limit_per_peer: per_peer,
        ..subtitles_it::settings(site, 0)
    };
    let sb_sites = Settings {
        sites: Vec::new(),
        daily_limit_per_user: sb_per_user,
        daily_limit_per_peer: 0,
    };
    let pair = Pair::start_both(
        test,
        ServerOptions {
            real_media: true,
            subtitle_sites: sa_sites,
            ..ServerOptions::default()
        },
        ServerOptions {
            subtitle_sites: sb_sites,
            ..ServerOptions::default()
        },
    )
    .await;
    write_film(&pair.sa);
    pair.share("plex:1").await;
    pair.refresh().await;
    pair.sb.add_user("bob", "correct horse", None).await;
    pair.sb.add_user("carol", "correct horse", None).await;
    pair
}

/// `user`'s app on SB, signed in, searching subtitles of SA's items.
async fn on_sb(pair: &Pair, user: &str) -> (TestClient, SessionId) {
    let client = pair.sb.client().await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: Platform::Linux,
        device_name: None,
        capabilities: None,
        experimental: vec![FEDERATION.into()],
        features: spoolframe_protocol::Features::empty(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    let session = client.login(user, "correct horse").await;
    (client, session)
}

async fn search(app: &TestClient, session: &SessionId, id: &MediaId) -> SubtitleSearch {
    let request = Request::SearchSubtitles {
        id: id.clone(),
        language: None,
    };
    match app.call(Some(session), request).await {
        Response::Ok(Reply::SubtitleResults(found)) => found,
        other => panic!("no results: {other:?}"),
    }
}

async fn download(
    app: &TestClient,
    session: &SessionId,
    id: &MediaId,
    result: &str,
) -> Result<SubtitleDownloaded, Error> {
    let request = Request::DownloadSubtitle {
        id: id.clone(),
        result: result.into(),
    };
    match app.call(Some(session), request).await {
        Response::Ok(Reply::SubtitleDownloaded(got)) => Ok(got),
        Response::Err(e) => Err(e),
        other => panic!("{other:?}"),
    }
}

fn over_the_limit(answer: &Result<SubtitleDownloaded, Error>) -> bool {
    matches!(
        answer,
        Err(Error::QuotaExhausted {
            whose: QuotaOf::User,
            site: None,
            ..
        })
    )
}

/// Bob and Carol download SA's film's subtitles from SB: both count
/// against SB's one limit on SA; at it both are refused, and SA's own
/// Dave is not.
#[tokio::test]
async fn two_of_sb_s_users_share_sb_s_one_subtitle_limit_on_sa() {
    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-limit", &site, 2, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    let (carol_app, carol) = on_sb(&pair, "carol").await;
    let found = search(&bob_app, &bob, &alien).await;
    assert!(
        found.results.iter().any(|r| r.id == "opensubtitles:101"),
        "SA's site's results: {found:?}"
    );
    let first = download(&bob_app, &bob, &alien, "opensubtitles:101")
        .await
        .unwrap();
    assert_eq!(first.user_remaining, Some(1), "one left for the pair");
    let second = download(&carol_app, &carol, &alien, "opensubtitles:102")
        .await
        .unwrap();
    assert_eq!(second.user_remaining, Some(0), "Carol took the pair's last");
    assert!(over_the_limit(
        &download(&bob_app, &bob, &alien, "opensubtitles:103").await
    ));
    assert!(over_the_limit(
        &download(&carol_app, &carol, &alien, "opensubtitles:104").await
    ));
    // SA's own users are held to their own limit, not the pair's.
    pair.sa.add_user("dave", "correct horse", None).await;
    let (dave_app, dave) = subtitles_it::app(&pair.sa, "dave").await;
    search(&dave_app, &dave, movie().id()).await;
    download(&dave_app, &dave, movie().id(), "opensubtitles:103")
        .await
        .unwrap();
    // The files are SA's: SB keeps none.
    assert_eq!(
        pair.sa.store.all_subtitle_downloads().await.unwrap().len(),
        3
    );
    assert!(
        pair.sb
            .store
            .all_subtitle_downloads()
            .await
            .unwrap()
            .is_empty()
    );
}

/// SA's Settings › Subtitles shows SB as one row, by SB's name, and no
/// name or id of SB's users anywhere in the day's usage.
#[tokio::test]
async fn sa_s_usage_shows_the_pair_as_one_row_with_no_user_names() {
    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-usage", &site, 40, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    let (carol_app, carol) = on_sb(&pair, "carol").await;
    search(&bob_app, &bob, &alien).await;
    download(&bob_app, &bob, &alien, "opensubtitles:101")
        .await
        .unwrap();
    download(&carol_app, &carol, &alien, "opensubtitles:102")
        .await
        .unwrap();
    let (root, _alice) = admin_beside(&pair.sa).await;
    let usage = root.subtitle_usage().await.unwrap();
    assert_eq!(
        usage.pairs,
        [SubtitlePairUsage {
            name: "Bob's server".into(),
            downloads: 2,
            remaining: Some(38),
        }]
    );
    assert_eq!(usage.daily_limit_per_peer, 40);
    assert!(usage.users.is_empty(), "{:?}", usage.users);
    let said = serde_json::to_string(&usage).unwrap();
    for user in ["bob", "carol"] {
        let id = pair.sb.store.user_by_name(user).await.unwrap().unwrap().id;
        assert!(!said.contains(user), "{said}");
        assert!(!said.contains(&id.to_string()), "{said}");
    }
}

/// M8-01: SA's site account has five downloads left after Bob's first,
/// which are kept for SA's own users: Bob's next is refused within the
/// pair's limit, and never asked of the site, while SA's Dave downloads.
#[tokio::test]
async fn a_pair_leaves_what_is_kept_of_the_site_account_for_sa_s_own_users() {
    let site = subtitles_it::site_with_remaining(5).await;
    let pair = sharing_a_film("fed-subs-kept-back", &site, 40, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    search(&bob_app, &bob, &alien).await;
    let first = download(&bob_app, &bob, &alien, "opensubtitles:101")
        .await
        .unwrap();
    assert_eq!(first.user_remaining, Some(39));
    assert!(over_the_limit(
        &download(&bob_app, &bob, &alien, "opensubtitles:102").await
    ));
    assert_eq!(
        subtitles_it::grants_asked(&site).await,
        1,
        "the site was not asked"
    );
    pair.sa.add_user("dave", "correct horse", None).await;
    let (dave_app, dave) = subtitles_it::app(&pair.sa, "dave").await;
    search(&dave_app, &dave, movie().id()).await;
    download(&dave_app, &dave, movie().id(), "opensubtitles:102")
        .await
        .unwrap();
}

/// Z8-05: the site grants Bob's download, saying five are left, and its
/// file then fails: the count is kept though no file is, so Bob's next
/// download is refused without asking the site again.
#[tokio::test]
async fn what_the_site_says_in_granting_is_kept_though_the_file_fails() {
    let site = subtitles_it::site_serving(5, 500).await;
    let pair = sharing_a_film("fed-subs-grant-kept", &site, 40, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    search(&bob_app, &bob, &alien).await;
    assert!(
        download(&bob_app, &bob, &alien, "opensubtitles:101")
            .await
            .is_err()
    );
    assert_eq!(subtitles_it::grants_asked(&site).await, 1);
    assert!(over_the_limit(
        &download(&bob_app, &bob, &alien, "opensubtitles:102").await
    ));
    assert_eq!(
        subtitles_it::grants_asked(&site).await,
        1,
        "not asked again"
    );
}

/// G8-03: a pair's searches that ask SA's site are held to ten for each
/// download of its daily limit; one SA answers from the day's kept
/// results is not counted.
#[tokio::test]
async fn a_pair_s_searches_of_the_site_are_held_to_a_day_s_budget() {
    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-searches", &site, 1, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    let in_language = |language: &str| Request::SearchSubtitles {
        id: alien.clone(),
        language: Some(language.into()),
    };
    let languages = ["en", "fr", "de", "es", "it", "pt", "nl", "sv", "da", "fi"];
    for language in languages {
        assert!(
            matches!(
                bob_app.call(Some(&bob), in_language(language)).await,
                Response::Ok(Reply::SubtitleResults(_))
            ),
            "{language}"
        );
    }
    assert!(matches!(
        bob_app.call(Some(&bob), in_language("en")).await,
        Response::Ok(Reply::SubtitleResults(_))
    ));
    assert!(matches!(
        bob_app.call(Some(&bob), in_language("pl")).await,
        Response::Err(Error::QuotaExhausted {
            whose: QuotaOf::User,
            site: None,
            ..
        })
    ));
}

/// SB's own limit per user applies first: Bob's second download is
/// refused by SB, never asked of SA, and Carol still has hers.
#[tokio::test]
async fn sb_s_own_limit_per_user_applies_first() {
    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-sb-limit", &site, 10, 1).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    let (carol_app, carol) = on_sb(&pair, "carol").await;
    search(&bob_app, &bob, &alien).await;
    let first = download(&bob_app, &bob, &alien, "opensubtitles:101")
        .await
        .unwrap();
    assert_eq!(first.user_remaining, Some(0), "SB's limit is the fewer");
    assert!(over_the_limit(
        &download(&bob_app, &bob, &alien, "opensubtitles:102").await
    ));
    assert_eq!(
        pair.sa.store.all_subtitle_downloads().await.unwrap().len(),
        1,
        "SA was not asked"
    );
    download(&carol_app, &carol, &alien, "opensubtitles:102")
        .await
        .unwrap();
}

/// What SA downloaded for the pair, any user of SB's may remove; SB's
/// users of a server that shares nothing more are refused as not found.
#[tokio::test]
async fn any_of_sb_s_users_removes_the_pair_s_download() {
    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-remove", &site, 10, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    let (carol_app, carol) = on_sb(&pair, "carol").await;
    search(&bob_app, &bob, &alien).await;
    download(&bob_app, &bob, &alien, "opensubtitles:101")
        .await
        .unwrap();
    let kept = pair.sa.store.all_subtitle_downloads().await.unwrap();
    let request = Request::RemoveSubtitleDownload {
        id: alien.clone(),
        download: kept[0].id.clone(),
    };
    assert!(matches!(
        carol_app.call(Some(&carol), request).await,
        Response::Ok(Reply::Done)
    ));
    assert!(
        pair.sa
            .store
            .all_subtitle_downloads()
            .await
            .unwrap()
            .is_empty()
    );

    federation(&pair.sa)
        .unshare(&pair.sb_on_sa, "plex:1", "alice")
        .await
        .unwrap();
    let request = Request::SearchSubtitles {
        id: alien.clone(),
        language: None,
    };
    assert!(matches!(
        bob_app.call(Some(&bob), request).await,
        Response::Err(Error::NotFound { .. })
    ));
}

/// Downloads at once never both take the last of a limit: Bob's three at
/// once on SB, with one a day his there, make one; Bob's and Carol's at
/// once on SA, with one a day for SB there, make one.
#[tokio::test]
async fn downloads_at_once_never_pass_a_limit() {
    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-at-once", &site, 1, 0).await;
    let alien = pair.fed(movie().id());
    let (bob_app, bob) = on_sb(&pair, "bob").await;
    let (carol_app, carol) = on_sb(&pair, "carol").await;
    search(&bob_app, &bob, &alien).await;
    let (a, b) = tokio::join!(
        download(&bob_app, &bob, &alien, "opensubtitles:101"),
        download(&carol_app, &carol, &alien, "opensubtitles:102"),
    );
    let made = [&a, &b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(made, 1, "SA's pair limit: {a:?} {b:?}");
    assert!(over_the_limit(if a.is_ok() { &b } else { &a }));

    let site = subtitles_it::site().await;
    let pair = sharing_a_film("fed-subs-at-once-sb", &site, 10, 1).await;
    let alien = pair.fed(movie().id());
    let mut apps = Vec::new();
    for _ in 0..3 {
        apps.push(on_sb(&pair, "bob").await);
    }
    search(&apps[0].0, &apps[0].1, &alien).await;
    let results = [
        "opensubtitles:101",
        "opensubtitles:102",
        "opensubtitles:103",
    ];
    let answers = futures_util::future::join_all(
        apps.iter()
            .zip(results)
            .map(|((app, bob), result)| download(app, bob, &alien, result)),
    )
    .await;
    assert_eq!(
        answers.iter().filter(|r| r.is_ok()).count(),
        1,
        "SB's limit per user: {answers:?}"
    );
    assert_eq!(
        pair.sa.store.all_subtitle_downloads().await.unwrap().len(),
        1,
        "SA was asked once"
    );
}

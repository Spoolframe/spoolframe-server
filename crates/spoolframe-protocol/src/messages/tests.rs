//! The wire format: every message round-trips, and what an older peer
//! does not know is skipped or named, never a decode failure.

use std::fmt::Debug;
use std::time::Duration;

use crate::TrackPreferencesInfo;
use crate::codec::{self, CodecError, MAX_FRAME};
use crate::{
    AudioAction, BitmapSubtitles, ChosenTracks, ConvertedAudio, ConvertedSubtitle, ConvertedVideo,
    Quality, SubtitleAction, TrackReason,
};
use crate::{
    AuthMethod, ConfirmPassword, Error, Event, Hello, LoginPassword, PROTOCOL_VERSION, PinProof,
    Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId, SessionInfo, UserSummary,
    WhoAmIReply,
};
use crate::{
    FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyInfo, PasskeyRegistrationBegun,
    PasskeySignInBegun,
};
use crate::{PlannedAudio, PlannedSubtitle, PlaybackPlan, Selection, SubtitleDelivery};
use crate::{SearchCounts, SearchGroup, SearchRequest, SearchResults, TopResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_types::Delivery;
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, SubtitleFormat, TrackPreferences, VideoCodec,
};
use spoolframe_types::{
    Item, Library, LibraryId, LibraryKind, Marker, MarkerKind, MediaId, MediaKind, MediaTracks,
    Movie, Namespace, Person, PersonId, PersonRole, PlaybackState, SearchKind, SearchScope,
};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T) {
    let bytes = codec::encode(&value).unwrap();
    assert_eq!(codec::decode::<T>(&bytes).unwrap(), value);
}

fn movie() -> Item {
    Item::Movie(Movie {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        title: "Alien".into(),
        year: Some(1979),
        release_date: None,
        summary: None,
        content_rating: None,
        duration: Some(Duration::from_secs(7020)),
        cast: Vec::new(),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn markers() -> Vec<Marker> {
    vec![
        Marker {
            kind: MarkerKind::Intro,
            start_ms: 258_865,
            end_ms: 319_865,
            to_end: false,
        },
        Marker {
            kind: MarkerKind::Credits,
            start_ms: 2_662_962,
            end_ms: 2_709_958,
            to_end: true,
        },
    ]
}

fn search_results() -> SearchResults {
    SearchResults {
        top: Some(TopResult::Item(movie())),
        groups: vec![
            SearchGroup {
                kind: SearchKind::People,
                items: Vec::new(),
                people: vec![Person {
                    id: PersonId::new(Namespace::Plex, "actor-134469").unwrap(),
                    name: "Keanu Reeves".into(),
                    role: PersonRole::Actor,
                    count: Some(9),
                }],
                more: true,
                total: None,
                hidden: 0,
            },
            SearchGroup {
                kind: SearchKind::Episodes,
                items: Vec::new(),
                people: Vec::new(),
                more: false,
                total: Some(3),
                hidden: 3,
            },
        ],
        counts: SearchCounts {
            all: Some(12),
            movies: Some(1),
            shows: None,
            episodes: Some(3),
            music: Some(0),
        },
        year: Some(1979),
        without_year: vec![movie()],
    }
}

fn hello() -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "spoolframe-linux".into(),
        client_version: "0.1.0".into(),
        platform: spoolframe_types::Platform::Linux,
        device_name: Some("Living room".into()),
        capabilities: Some(ClientCapabilities {
            video: [VideoCodec::H264].into(),
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Matroska].into(),
            subtitles: [SubtitleFormat::Srt].into(),
            ..ClientCapabilities::default()
        }),
        experimental: Vec::new(),
        features: vec![crate::features::PLAYLISTS.to_owned()],
    }
}

#[test]
fn every_request_round_trips() {
    let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
    for request in [
        Request::GetPreviews { id: id.clone() },
        Request::Hello(hello()),
        Request::AuthMethods,
        Request::LoginPassword(LoginPassword {
            username: "alice".into(),
            password: "hunter2".into(),
            remember: true,
        }),
        Request::Ping,
        Request::BeginDeviceSignIn { remember: true },
        Request::BeginDeviceSignIn { remember: false },
        Request::TrustedProfiles,
        Request::ResumeProfile {
            user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
        },
        Request::SetDeviceTrust {
            device: None,
            trusted: true,
            auto_login: true,
        },
        Request::SetDeviceTrust {
            device: Some("ab".repeat(32)),
            trusted: false,
            auto_login: false,
        },
        Request::ForgetDevice {
            device: "ab".repeat(32),
        },
        Request::AwaitDeviceSignIn {
            request: "r".into(),
        },
        Request::LookupDeviceSignIn {
            code: "BCDFGHJK".into(),
        },
        Request::BeginPasskeySignIn { username: None },
        Request::BeginPasskeySignIn {
            username: Some("alice".into()),
        },
        Request::FinishPasskeySignIn {
            signed: signed(),
            remember: false,
        },
        Request::FinishPasskeySignIn {
            signed: unhandled(),
            remember: false,
        },
        Request::BeginPasskeyRegistration,
        Request::FinishPasskeyRegistration(made()),
        Request::ListPasskeys,
        Request::RenamePasskey {
            id: "AQID".into(),
            name: "Alice's iPad".into(),
        },
        Request::RemovePasskey { id: "AQID".into() },
        Request::ConfirmPassword(ConfirmPassword {
            password: "hunter2".into(),
        }),
        Request::BeginConfirmPasskey,
        Request::FinishConfirmPasskey(signed()),
        Request::FinishConfirmPasskey(unhandled()),
        Request::ApproveDeviceSignIn {
            code: "BCDFGHJK".into(),
            remember: true,
        },
        Request::DenyDeviceSignIn {
            code: "BCDFGHJK".into(),
        },
        Request::Libraries,
        Request::Items {
            library: LibraryId::new(Namespace::Plex, "2").unwrap(),
            offset: 40,
            limit: 20,
        },
        Request::Item { id: id.clone() },
        Request::Search(SearchRequest {
            query: "alien 1979".into(),
            limit: 10,
            scope: SearchScope::Movies,
            kind: None,
            library: Some(LibraryId::new(Namespace::Plex, "2").unwrap()),
        }),
        Request::Search(SearchRequest {
            query: "keanu".into(),
            limit: 50,
            scope: SearchScope::All,
            kind: Some(SearchKind::People),
            library: None,
        }),
        Request::PersonItems {
            person: PersonId::new(Namespace::Plex, "director-131768").unwrap(),
            offset: 0,
            limit: 50,
        },
        Request::Tracks { id },
        Request::WhoAmI,
        Request::Logout,
        Request::ListSessions,
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Some(Selection::default()),
            start_ms: Some(1_800_000),
            bitmap: BitmapSubtitles::Text,
            quality: Default::default(),
            display: None,
        },
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: None,
            start_ms: None,
            bitmap: BitmapSubtitles::Auto,
            quality: Default::default(),
            display: None,
        },
        Request::StopPlayback {
            handle: crate::PlaybackHandle("h".into()),
        },
        Request::SetPin {
            pin: "4321".into(),
            proof: None,
        },
        Request::SetPin {
            pin: "4321".into(),
            proof: Some(PinProof::Pin { pin: "1234".into() }),
        },
        Request::ClearPin {
            proof: PinProof::Password {
                password: "correct horse".into(),
            },
        },
        Request::UnlockProfile { pin: "4321".into() },
        Request::ListDevices,
        Request::GetPlexLink,
        Request::SetPinRequired {
            device: None,
            required: false,
        },
        Request::ChangeSubtitle {
            handle: crate::PlaybackHandle("h".into()),
            subtitle: Some(2),
            bitmap: BitmapSubtitles::BurnIn,
            position_ms: 61_000,
            display: None,
        },
        Request::ReadSubtitleSegment {
            handle: crate::PlaybackHandle("h".into()),
            n: 7,
        },
        Request::GetManifest {
            handle: crate::PlaybackHandle("h".into()),
            kind: crate::ManifestKind::Subtitles,
            variant: 0,
        },
        Request::SeekPlayback {
            handle: crate::PlaybackHandle("h".into()),
            position_ms: 600_000,
        },
        Request::GetManifest {
            handle: crate::PlaybackHandle("h".into()),
            kind: crate::ManifestKind::Media,
            variant: 0,
        },
        Request::GetTrackPreferences,
        Request::SetTrackPreferences {
            preferences: TrackPreferences::default(),
        },
        Request::SetTrackOverride {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap(),
            selection: Selection {
                audio: Some(2),
                subtitle: None,
            },
        },
        Request::ClearTrackOverride {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap(),
        },
        Request::UpdatePlaybackProgress {
            handle: crate::PlaybackHandle("handle".into()),
            sequence: 1,
            position_ms: 61_000,
        },
        Request::MarkWatched {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        },
        Request::MarkUnwatched {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        },
        Request::HideFromContinueWatching {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "70").unwrap(),
        },
        Request::UnhideFromContinueWatching {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "70").unwrap(),
        },
        Request::Home,
        Request::ReadSubtitle {
            handle: crate::PlaybackHandle("h".into()),
            offset: 0,
            version: None,
        },
        Request::ReadSubtitle {
            handle: crate::PlaybackHandle("h".into()),
            offset: crate::SUBTITLE_CHUNK,
            version: Some(0x1234_5678_9abc_def0),
        },
        Request::PlaybackPlan {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Some(Selection {
                audio: Some(1),
                subtitle: None,
            }),
            bitmap: BitmapSubtitles::BurnIn,
            display: None,
        },
        Request::IdentityRecord { user: p7_08_user() },
        Request::IdentityRecordByHandle { handle: [0x5a; 32] },
        Request::OfferPairing(p7_08_offer()),
        Request::FetchPairing {
            code: p7_08_offer().code,
        },
        Request::AnswerPairing {
            code: p7_08_offer().code,
            answer: p7_08_answer(),
        },
        Request::CollectPairing {
            code: p7_08_offer().code,
        },
    ] {
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request,
        });
    }
}

#[test]
fn every_reply_round_trips() {
    for reply in [
        Reply::ServerInfo(ServerInfo {
            name: "Home Media".into(),
            endpoint_id: "e9c1".into(),
            server_id: "mzxw6ytbojtg633c".into(),
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.1.0".into(),
            auth_methods: vec![AuthMethod::Password],
            web_url: Some("https://media.example.org".into()),
            experimental: Vec::new(),
            features: Vec::new(),
        }),
        Reply::AuthMethods {
            methods: vec![AuthMethod::Password, AuthMethod::DeviceApproval],
        },
        Reply::PasskeyRegistrationBegun(creation()),
        Reply::Passkeys {
            passkeys: vec![
                PasskeyInfo {
                    id: "AQID".into(),
                    name: "Alice's iPhone".into(),
                    created_at: 1,
                    last_used_at: Some(2),
                    rp_id: "spoolframe.app".into(),
                    app: true,
                    usable_here: true,
                },
                PasskeyInfo {
                    id: "BAUG".into(),
                    name: "YubiKey".into(),
                    created_at: 3,
                    last_used_at: None,
                    rp_id: "media.example.org".into(),
                    app: false,
                    usable_here: false,
                },
            ],
        },
        Reply::Passkeys {
            passkeys: Vec::new(),
        },
        Reply::PasskeySignInBegun(PasskeySignInBegun {
            nonce: vec![7; 32],
            allow_credential_ids: vec![vec![1; 16], vec![2; 16]],
        }),
        Reply::PasskeySignInBegun(PasskeySignInBegun {
            nonce: vec![7; 32],
            allow_credential_ids: Vec::new(),
        }),
        Reply::Previews(crate::Previews {
            interval_ms: 10_000,
            tile_width: 320,
            tile_height: 134,
            columns: 10,
            rows: 10,
            count: 150,
            sheets: vec![
                "preview:0f3a/0".parse().unwrap(),
                "preview:0f3a/1".parse().unwrap(),
            ],
        }),
        Reply::DeviceSignInStarted {
            request: "r".into(),
            code: "BCDFGHJK".into(),
            expires_in_ms: 300_000,
            web_url: Some("https://media.example.org/auth/device?code=BCDFGHJK".into()),
        },
        Reply::DeviceSignInStarted {
            request: "r".into(),
            code: "BCDFGHJK".into(),
            expires_in_ms: 300_000,
            web_url: None,
        },
        Reply::DeviceSignInPending,
        Reply::DeviceSignInRequest(crate::DeviceSignInRequest {
            device_name: Some("Living room TV".into()),
            client_name: "spoolframe-tvos".into(),
            platform: spoolframe_types::Platform::Tvos,
            requested_at: 1_790_000_000,
            remember: true,
            requested_from: Some(crate::DeviceAddress {
                address: "192.168.86.20".into(),
                network: crate::AddressNetwork::Local,
            }),
        }),
        Reply::DeviceSignInRequest(crate::DeviceSignInRequest {
            device_name: None,
            client_name: "spoolframe-tvos".into(),
            platform: spoolframe_types::Platform::Tvos,
            requested_at: 1_790_000_000,
            remember: false,
            requested_from: None,
        }),
        Reply::LoggedIn {
            session: SessionId("s".into()),
            remembered: false,
            password_change_required: true,
            features: vec![crate::features::PLAYLISTS.to_owned()],
        },
        Reply::TrustedProfiles {
            profiles: vec![crate::TrustedProfile {
                user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
                username: "alice".into(),
                display_name: "Alice".into(),
                pin_required: true,
                auto_login: false,
            }],
        },
        Reply::Pong,
        Reply::Libraries {
            libraries: vec![Library {
                id: LibraryId::new(Namespace::Plex, "2").unwrap(),
                title: "Movies".into(),
                kind: LibraryKind::Movies,
            }],
        },
        Reply::Items {
            items: vec![movie()],
            total: 770,
        },
        Reply::Item {
            item: movie(),
            children: Vec::new(),
            markers: markers(),
            playlists: None,
            liked: None,
            liked_children: None,
        },
        Reply::Item {
            item: movie(),
            children: Vec::new(),
            markers: Vec::new(),
            playlists: Some(vec![crate::playlists::PlaylistRef {
                id: "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
                name: "Heists".into(),
                owner_kind: crate::playlists::PlaylistOwnerKind::SharedWithMe,
                builtin: None,
                owner_name: None,
                artwork: None,
            }]),
            liked: None,
            liked_children: None,
        },
        Reply::Search(search_results()),
        Reply::Tracks(MediaTracks::default()),
        Reply::WhoAmI(WhoAmIReply {
            user: UserSummary {
                user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
                username: "alice".into(),
                display_name: "Alice".into(),
                role: spoolframe_types::Role::Consumer,
                locked: true,
                pin: Some(crate::PinState { digits: Some(6) }),
                portable: ties(),
            },
            can_leave: true,
            features: vec![crate::features::PLAYLISTS.to_owned()],
        }),
        Reply::Devices {
            devices: vec![crate::DeviceInfo {
                endpoint_id: "ab".repeat(32),
                name: Some("Living room TV".into()),
                platform: Some(spoolframe_types::Platform::Tvos),
                last_seen_at: 1_790_000_000,
                pin_required: true,
                trusted: true,
                auto_login: false,
                this_device: false,
            }],
        },
        Reply::PlexLink { link: None },
        Reply::PlexLink {
            link: Some(crate::PlexLink {
                username: Some("alice".into()),
                email: Some("alice@example.com".into()),
                by_config: false,
                linked_at: Some(1_790_000_000),
                last_sync: Some(crate::PlexSyncSummary {
                    at: 1_790_000_100,
                    libraries: 2,
                    pulled: 3,
                    pushed: 4,
                    skipped: 5,
                    failed: 0,
                }),
                syncing: true,
            }),
        },
        Reply::LoggedOut,
        Reply::Done,
        Reply::Confirmed,
        Reply::PlaybackSeeked { epoch: 1 },
        Reply::PlaybackChanged {
            plan: PlaybackPlan::Unsupported {
                reasons: Vec::new(),
            },
            size: 0,
            epoch: 3,
        },
        Reply::Manifest {
            text: "#EXTM3U\n".into(),
        },
        Reply::Home(crate::HomeScreen {
            continue_watching: vec![movie()],
            next_up: Vec::new(),
            recently_added: vec![movie()],
        }),
        Reply::PlaybackStarted {
            handle: crate::PlaybackHandle("h".into()),
            plan: PlaybackPlan::Unsupported { reasons: vec![] },
            size: 12_345,
            epoch: 0,
            chosen: Some(ChosenTracks {
                selection: Selection {
                    audio: Some(2),
                    subtitle: Some(6),
                },
                audio: TrackReason::Preferences,
                subtitle: TrackReason::Override,
            }),
            markers: markers(),
        },
        Reply::TrackPreferences(TrackPreferencesInfo {
            preferences: TrackPreferences::default(),
            ocr_offered: true,
        }),
        Reply::Subtitle {
            format: SubtitleFormat::Srt,
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
        },
        Reply::SubtitleChunk {
            format: SubtitleFormat::Srt,
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
            total: 1_600_000,
            version: u64::MAX,
        },
        Reply::PlaybackPlan {
            plan: PlaybackPlan::Direct {
                container: Container::Matroska,
                video: Some(VideoCodec::Hevc),
                audio: Some(PlannedAudio {
                    index: 1,
                    codec: AudioCodec::Aac,
                }),
                subtitle: Some(PlannedSubtitle {
                    index: 0,
                    format: SubtitleFormat::Srt,
                    delivery: SubtitleDelivery::External,
                }),
            },
            chosen: None,
        },
        Reply::Sessions {
            sessions: vec![SessionInfo {
                device_name: Some("Living room".into()),
                platform: Some(spoolframe_types::Platform::Linux),
                created_at: 1,
                last_used_at: 2,
                current: true,
            }],
        },
        Reply::IdentityRecord(crate::portable::IdentityRecord {
            user: p7_08_user(),
            genesis: None,
            state: None,
        }),
        Reply::PairingOffered(p7_08_offer()),
        Reply::PairingAnswered {
            answer: Some(p7_08_answer()),
        },
        Reply::PairingAnswered { answer: None },
    ] {
        round_trip(Response::Ok(reply));
    }
}

#[test]
fn every_error_round_trips() {
    for error in [
        Error::HandshakeRequired,
        Error::Unauthenticated,
        Error::Unsupported {
            request: "join_federation".into(),
        },
        Error::Malformed {
            message: "x".into(),
        },
        Error::NotFound {
            what: "plex:movie:1".into(),
        },
        Error::InvalidCredentials,
        Error::DeviceSignInDenied,
        Error::DeviceSignInExpired,
        Error::CannotPlay {
            reasons: vec!["audio 0: dts not supported".into()],
        },
        Error::PlaybackExpired,
        Error::RateLimited {
            retry_after_ms: 60_000,
        },
        Error::ReplyTooLarge { bytes: 1_600_000 },
        Error::Unavailable {
            message: "Plex is down".into(),
        },
        Error::NotSetUp,
        Error::NoPassword,
        Error::RecentSignInRequired,
        Error::LastSignInMethod,
        Error::PreviewsNotReady { done_percent: 7 },
        Error::NoPreviews,
        Error::Internal,
    ] {
        round_trip(Response::Err(error));
    }
}

#[test]
fn every_plan_kind_round_trips_phase_2_ones_included() {
    for plan in [
        // A remux: every stream copied.
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Mp4,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: false,
            }),
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Copy {
                    codec: AudioCodec::Eac3,
                },
            }),
            subtitle: None,
            ladder: Vec::new(),
            reasons: vec!["container: matroska not supported".into()],
        },
        PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: Some(8_000_000),
                tone_map: true,
            }),
            audio: Some(ConvertedAudio {
                index: 1,
                action: AudioAction::Encode {
                    codec: AudioCodec::Aac,
                    channels: 2,
                    bitrate: 192_000,
                },
            }),
            subtitle: Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::BurnIn {
                    format: SubtitleFormat::Pgs,
                },
            }),
            ladder: Vec::new(),
            reasons: vec!["subtitle 0: pgs cannot be shown; burned into the video".into()],
        },
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: false,
            }),
            audio: None,
            subtitle: Some(ConvertedSubtitle {
                index: 2,
                action: SubtitleAction::Ocr {
                    from: SubtitleFormat::Pgs,
                    to: SubtitleFormat::Srt,
                },
            }),
            ladder: Vec::new(),
            reasons: vec!["subtitle 2: pgs read as text".into()],
        },
        PlaybackPlan::Unsupported {
            reasons: vec!["audio 0: dts not supported".into()],
        },
    ] {
        round_trip(Response::Ok(Reply::PlaybackPlan { plan, chosen: None }));
    }
}

#[test]
fn plan_parts_from_a_newer_peer_decode_as_unknown() {
    // Over the wire's own encoding: a newer peer's CBOR.
    fn wire<T: DeserializeOwned>(value: serde_json::Value) -> T {
        codec::decode(&codec::encode(&value).unwrap()).unwrap()
    }
    let plan: PlaybackPlan = wire(serde_json::json!({"type": "dash_ladder", "rungs": 4}));
    assert_eq!(plan, PlaybackPlan::Unknown);
    let video: ConvertedVideo = wire(serde_json::json!({"type": "super_resolve"}));
    assert_eq!(video, ConvertedVideo::Unknown);
    let delivery: Delivery = wire(serde_json::json!("dash"));
    assert_eq!(delivery, Delivery::Unknown);
}

/// A rung from a server before the remux ladder (PX-21) has no `video`:
/// the plan's, as before. A remux ladder's lower rung names its own.
#[test]
fn a_rung_without_a_video_is_the_plan_s() {
    fn wire<T: DeserializeOwned>(value: serde_json::Value) -> T {
        codec::decode(&codec::encode(&value).unwrap()).unwrap()
    }
    let rung: crate::playback::Rung =
        wire(serde_json::json!({"height": 720, "max_bitrate": 3_000_000}));
    assert_eq!(rung.video, None);
    round_trip(crate::playback::Rung {
        height: 720,
        max_bitrate: 3_000_000,
        video: Some(ConvertedVideo::Encode {
            codec: spoolframe_types::VideoCodec::H264,
            height: 720,
            max_bitrate: Some(3_000_000),
            tone_map: false,
        }),
    });
}

#[test]
fn artwork_messages_round_trip() {
    round_trip(crate::ArtworkRequest {
        session: SessionId("s".into()),
        id: spoolframe_types::ArtworkId::new(Namespace::Plex, "/library/metadata/1/thumb/2")
            .unwrap(),
        size: crate::ArtworkSize::Medium,
    });
    round_trip(crate::ArtworkResponse::Ok(crate::ArtworkReply {
        content_type: "image/jpeg".into(),
        length: 1234,
        etag: "abc".into(),
    }));
    round_trip(crate::ArtworkResponse::Err(Error::Unauthenticated));
}

#[test]
fn an_artwork_size_from_a_newer_client_is_unknown() {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Newer {
        Huge,
    }
    let bytes = codec::encode(&Newer::Huge).unwrap();
    assert_eq!(
        codec::decode::<crate::ArtworkSize>(&bytes).unwrap(),
        crate::ArtworkSize::Unknown
    );
}

#[test]
fn media_reads_round_trip() {
    round_trip(crate::MediaRead {
        session: SessionId("s".into()),
        handle: crate::PlaybackHandle("h".into()),
        offset: 1 << 30,
        length: crate::MAX_READ,
        epoch: 2,
        segment: Some(14),
        variant: 0,
    });
    round_trip(crate::MediaReadResponse::Ok(crate::MediaReadReply {
        sent: 4096,
        size: 1 << 33,
        complete: false,
        epoch: 2,
    }));
    for error in [
        Error::PlaybackExpired,
        Error::PlaybackStopped,
        Error::Busy,
        Error::PlaybackFailed {
            reason: "Invalid data found when processing input".into(),
        },
        Error::PlaybackRestarted { epoch: 3 },
        Error::SubtitleNotReady { done_percent: 42 },
        Error::SubtitleChanged,
        Error::ProfileLocked,
        Error::NotSetUp,
    ] {
        round_trip(crate::MediaReadResponse::Err(error));
    }
}

/// A reply from a server before P2-04 has neither `complete` nor `epoch`.
/// It was once read as direct play; since PX-15 it is refused.
#[test]
fn an_older_server_s_read_reply_does_not_decode() {
    #[derive(Serialize)]
    struct OldReply {
        sent: u64,
        size: u64,
    }
    let bytes = codec::encode(&OldReply { sent: 10, size: 20 }).unwrap();
    let error = codec::decode::<crate::MediaReadReply>(&bytes).unwrap_err();
    assert!(error.to_string().contains("missing field"), "{error}");
}

#[test]
fn subtitle_bytes_are_a_cbor_byte_string() {
    let segment = Reply::Subtitle {
        format: SubtitleFormat::Srt,
        bytes: vec![0xAB; 100],
    };
    let chunk = Reply::SubtitleChunk {
        format: SubtitleFormat::Srt,
        bytes: vec![0xAB; 100],
        total: 100,
        version: 1,
    };
    // A byte string is about as long as its bytes; an array of integers
    // would take about two bytes each.
    for reply in [segment, chunk] {
        assert!(codec::encode(&reply).unwrap().len() < 170);
    }
}

fn creation() -> PasskeyRegistrationBegun {
    PasskeyRegistrationBegun {
        nonce: vec![7; 32],
        user_id: vec![9; 16],
        user_name: "alice on Home Media".into(),
        display_name: "Alice on Home Media".into(),
        exclude_credential_ids: vec![vec![1; 16]],
    }
}

fn made() -> FinishPasskeyRegistration {
    FinishPasskeyRegistration {
        credential_id: vec![1; 16],
        attestation_object: vec![0xA3; 64],
        client_data_json: b"{}".to_vec(),
        name: "Alice's iPhone".into(),
    }
}

fn signed() -> FinishPasskeySignIn {
    FinishPasskeySignIn {
        credential_id: vec![1; 16],
        authenticator_data: vec![0x49; 37],
        client_data_json: b"{}".to_vec(),
        signature: vec![0x30; 70],
        user_handle: Some(vec![9; 16]),
    }
}

/// [`signed`] by an authenticator that gave no user handle.
fn unhandled() -> FinishPasskeySignIn {
    FinishPasskeySignIn {
        user_handle: None,
        ..signed()
    }
}

/// The `user_handle` of `signed`, as its CBOR map has it.
fn cbor_user_handle(signed: &FinishPasskeySignIn) -> Option<ciborium::Value> {
    let bytes = codec::encode(signed).unwrap();
    let value: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();
    value
        .into_map()
        .unwrap()
        .into_iter()
        .find(|(key, _)| key.as_text() == Some("user_handle"))
        .map(|(_, value)| value)
}

/// `FinishPasskeySignIn` as revision 4 sent it before `user_handle` was
/// an `Option`: always bytes, empty for none.
#[derive(Serialize)]
struct BytesHandleSignIn {
    #[serde(with = "serde_bytes")]
    credential_id: Vec<u8>,
    #[serde(with = "serde_bytes")]
    authenticator_data: Vec<u8>,
    #[serde(with = "serde_bytes")]
    client_data_json: Vec<u8>,
    #[serde(with = "serde_bytes")]
    signature: Vec<u8>,
    #[serde(with = "serde_bytes")]
    user_handle: Vec<u8>,
}

impl BytesHandleSignIn {
    fn of(signed: FinishPasskeySignIn, user_handle: Vec<u8>) -> Self {
        Self {
            credential_id: signed.credential_id,
            authenticator_data: signed.authenticator_data,
            client_data_json: signed.client_data_json,
            signature: signed.signature,
            user_handle,
        }
    }
}

/// A handle goes as a CBOR byte string, as it did when it was not an
/// `Option`.
#[test]
fn a_user_handle_is_a_cbor_byte_string() {
    assert_eq!(
        cbor_user_handle(&signed()),
        Some(ciborium::Value::Bytes(vec![9; 16]))
    );
    assert_eq!(
        codec::encode(&signed()).unwrap(),
        codec::encode(&BytesHandleSignIn::of(signed(), vec![9; 16])).unwrap()
    );
}

/// No handle is sent as an explicit null, never left out.
#[test]
fn no_user_handle_is_an_explicit_null() {
    assert_eq!(cbor_user_handle(&unhandled()), Some(ciborium::Value::Null));
}

/// A message without the field is not one with no handle: the field is
/// required, as every other field of it.
#[test]
fn a_sign_in_that_leaves_the_user_handle_out_does_not_decode() {
    #[derive(Serialize)]
    struct NoHandleField {
        #[serde(with = "serde_bytes")]
        credential_id: Vec<u8>,
        #[serde(with = "serde_bytes")]
        authenticator_data: Vec<u8>,
        #[serde(with = "serde_bytes")]
        client_data_json: Vec<u8>,
        #[serde(with = "serde_bytes")]
        signature: Vec<u8>,
    }
    let signed = signed();
    let bytes = codec::encode(&NoHandleField {
        credential_id: signed.credential_id,
        authenticator_data: signed.authenticator_data,
        client_data_json: signed.client_data_json,
        signature: signed.signature,
    })
    .unwrap();
    assert!(codec::decode::<FinishPasskeySignIn>(&bytes).is_err());
}

/// A revision-4 server's `ServerInfo` has no `server_id`; it does not
/// decode, so no client takes another relying party for a missing one.
#[test]
fn server_info_without_a_server_id_does_not_decode() {
    let mut value = ciborium::Value::serialized(&ServerInfo {
        name: "Home Media".into(),
        endpoint_id: "e9c1".into(),
        server_id: "mzxw6ytbojtg633c".into(),
        protocol_version: 4,
        server_version: "0.1.0".into(),
        auth_methods: vec![AuthMethod::Password],
        web_url: None,
        experimental: Vec::new(),
        features: Vec::new(),
    })
    .unwrap();
    let ciborium::Value::Map(fields) = &mut value else {
        panic!("{value:?}");
    };
    fields.retain(|(key, _)| key.as_text() != Some("server_id"));
    let bytes = codec::encode(&value).unwrap();
    assert!(codec::decode::<ServerInfo>(&bytes).is_err());
}

/// An older client's empty handle is `Some` of nothing, for the server
/// to refuse, not `None`: an older client's bytes still decode.
#[test]
fn an_older_client_s_empty_user_handle_decodes_as_some_empty() {
    let bytes = codec::encode(&BytesHandleSignIn::of(signed(), Vec::new())).unwrap();
    assert_eq!(
        codec::decode::<FinishPasskeySignIn>(&bytes).unwrap(),
        FinishPasskeySignIn {
            user_handle: Some(Vec::new()),
            ..signed()
        }
    );
}

/// Passkey bytes go as CBOR byte strings, lists of them too: an app's
/// platform API gives raw bytes, and arrays of integers would double them.
#[test]
fn passkey_bytes_are_cbor_byte_strings() {
    let ids = vec![vec![0xAB; 100]; 3];
    let request = Reply::PasskeySignInBegun(PasskeySignInBegun {
        nonce: vec![0xAB; 100],
        allow_credential_ids: ids.clone(),
    });
    assert!(codec::encode(&request).unwrap().len() < 470);
    let creation = Reply::PasskeyRegistrationBegun(PasskeyRegistrationBegun {
        exclude_credential_ids: ids,
        ..creation()
    });
    assert!(codec::encode(&creation).unwrap().len() < 600);
    let signed = Request::FinishPasskeySignIn {
        signed: FinishPasskeySignIn {
            authenticator_data: vec![0xAB; 100],
            client_data_json: vec![0xAB; 100],
            signature: vec![0xAB; 100],
            ..signed()
        },
        remember: true,
    };
    assert!(codec::encode(&signed).unwrap().len() < 500);
}

#[test]
fn events_round_trip() {
    round_trip(Event::ShuttingDown);
    round_trip(server_addresses_event());
    round_trip(Event::ServerAddresses {
        addrs: Vec::new(),
        relays: Vec::new(),
        lookup: None,
    });
}

fn server_addresses_event() -> Event {
    Event::ServerAddresses {
        addrs: vec![
            "10.10.0.2:4433".parse().unwrap(),
            "[2001:db8::1]:4433".parse().unwrap(),
        ],
        relays: vec!["https://relay.example.net./".into()],
        lookup: Some("https://dns.iroh.link/pkarr".into()),
    }
}

/// The addresses a signed-in device is pushed (PX-12) go as text, as the
/// rest of the protocol's addresses do, and the relays as their URLs.
#[test]
fn the_server_addresses_event_goes_as_text() {
    assert_eq!(
        serde_json::to_value(server_addresses_event()).unwrap(),
        serde_json::json!({
            "type": "server_addresses",
            "addrs": ["10.10.0.2:4433", "[2001:db8::1]:4433"],
            "relays": ["https://relay.example.net./"],
            "lookup": "https://dns.iroh.link/pkarr",
        })
    );
    let bad: Result<Event, _> = serde_json::from_value(serde_json::json!({
        "type": "server_addresses", "addrs": ["nimble.local:4433"], "relays": [], "lookup": null
    }));
    assert!(bad.is_err(), "{bad:?}");
}

/// No field may be left out: an empty list is sent as empty, no lookup
/// as `null`.
#[test]
fn the_server_addresses_event_missing_a_field_is_refused() {
    assert_each_required(&server_addresses_event(), &["addrs", "relays", "lookup"]);
}

/// What a newer peer might send.
mod newer {
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Request {
        JoinFederation { code: String },
    }

    #[derive(Serialize)]
    pub struct Envelope {
        pub session: Option<String>,
        pub request: Request,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Error {
        PairingExpired { at: u64 },
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Event {
        WatchStateChanged { id: String },
    }

    #[derive(Serialize)]
    pub struct Hello {
        pub protocol_version: u32,
        pub client_name: String,
        pub client_version: String,
        /// A platform this build has never heard of.
        pub platform: String,
        pub device_name: Option<String>,
        pub capabilities: Option<spoolframe_types::ClientCapabilities>,
        /// A field this build has never heard of.
        pub screen_inches: u32,
    }
}

#[test]
fn an_unknown_request_decodes_as_unknown_and_can_be_named() {
    let bytes = codec::encode(&newer::Envelope {
        session: None,
        request: newer::Request::JoinFederation { code: "abc".into() },
    })
    .unwrap();
    let envelope: RequestEnvelope = codec::decode(&bytes).unwrap();
    assert_eq!(envelope.request, Request::Unknown);
    assert_eq!(
        codec::request_type(&bytes).as_deref(),
        Some("join_federation")
    );
}

#[test]
fn an_unknown_error_decodes_as_unknown() {
    let bytes = codec::encode(&newer::Error::PairingExpired { at: 1 }).unwrap();
    assert_eq!(codec::decode::<Error>(&bytes).unwrap(), Error::Unknown);
}

#[test]
fn an_unknown_event_decodes_as_unknown() {
    let bytes = codec::encode(&newer::Event::WatchStateChanged { id: "x".into() }).unwrap();
    assert_eq!(codec::decode::<Event>(&bytes).unwrap(), Event::Unknown);
}

#[test]
fn a_field_added_by_a_newer_client_is_ignored() {
    let bytes = codec::encode(&newer::Hello {
        protocol_version: 2,
        client_name: "next".into(),
        client_version: "9".into(),
        platform: "vision_os".into(),
        device_name: None,
        capabilities: None,
        screen_inches: 65,
    })
    .unwrap();
    let hello: Hello = codec::decode(&bytes).unwrap();
    assert_eq!(hello.protocol_version, 2);
    assert_eq!(hello.platform, spoolframe_types::Platform::Unknown);
}

#[test]
fn a_search_is_flat_on_the_wire() {
    let request = Request::Search(SearchRequest {
        query: "dune 2021".into(),
        limit: 6,
        scope: SearchScope::Music,
        kind: Some(SearchKind::Artists),
        library: None,
    });
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::json!({
            "type": "search",
            "query": "dune 2021",
            "limit": 6,
            "scope": "music",
            "kind": "artists",
            "library": null,
        })
    );
    let reply = serde_json::to_value(Reply::Search(search_results())).unwrap();
    assert_eq!(reply["type"], "search");
    assert_eq!(reply["year"], 1979);
    assert_eq!(reply["groups"][0]["kind"], "people");
    assert_eq!(reply["groups"][0]["people"][0]["id"], "plex:actor-134469");
    assert_eq!(reply["groups"][1]["hidden"], 3);
    assert_eq!(reply["counts"]["shows"], serde_json::Value::Null);
    assert_eq!(reply["top"]["item"]["kind"], "movie");
    assert_eq!(reply["groups"][1]["total"], 3);
    assert_eq!(reply["groups"][0]["total"], serde_json::Value::Null);
}

#[test]
fn a_person_at_the_top_is_tagged_person_on_the_wire() {
    let keanu = Person {
        id: PersonId::new(Namespace::Plex, "actor-134469").unwrap(),
        name: "Keanu Reeves".into(),
        role: PersonRole::Actor,
        count: Some(9),
    };
    let top = TopResult::Person(keanu);
    let json = serde_json::to_value(&top).unwrap();
    assert_eq!(json["person"]["name"], "Keanu Reeves");
    assert_eq!(json["person"]["id"], "plex:actor-134469");
    round_trip(top);
}

#[test]
fn request_types_are_snake_case() {
    let bytes = codec::encode(&RequestEnvelope {
        session: None,
        request: Request::AuthMethods,
    })
    .unwrap();
    assert_eq!(codec::request_type(&bytes).as_deref(), Some("auth_methods"));
}

#[test]
fn only_hello_auth_methods_logins_and_ping_are_pre_auth() {
    assert!(Request::Hello(hello()).is_pre_auth());
    assert!(Request::Ping.is_pre_auth());
    assert!(Request::BeginDeviceSignIn { remember: true }.is_pre_auth());
    // A passkey signs in (P3-19); only a session makes one.
    assert!(Request::BeginPasskeySignIn { username: None }.is_pre_auth());
    assert!(
        Request::FinishPasskeySignIn {
            signed: signed(),
            remember: false
        }
        .is_pre_auth()
    );
    assert!(!Request::BeginPasskeyRegistration.is_pre_auth());
    assert!(!Request::FinishPasskeyRegistration(made()).is_pre_auth());
    assert!(!Request::ListPasskeys.is_pre_auth());
    assert!(!Request::RemovePasskey { id: "AQID".into() }.is_pre_auth());
    // Confirming is the session user's: there is no one to confirm without
    // one.
    assert!(
        !Request::ConfirmPassword(ConfirmPassword {
            password: "hunter2".into()
        })
        .is_pre_auth()
    );
    assert!(!Request::BeginConfirmPasskey.is_pre_auth());
    assert!(!Request::FinishConfirmPasskey(signed()).is_pre_auth());
    assert!(
        Request::AwaitDeviceSignIn {
            request: "r".into()
        }
        .is_pre_auth()
    );
    // Approving signs someone in: only a session may.
    assert!(
        !Request::ApproveDeviceSignIn {
            code: "BCDFGHJK".into(),
            remember: true,
        }
        .is_pre_auth()
    );
    // A trusted device whose sessions ended has none to send (P4-01).
    assert!(Request::TrustedProfiles.is_pre_auth());
    assert!(
        Request::ResumeProfile {
            user_id: "local:x".into()
        }
        .is_pre_auth()
    );
    // Trusting is the session user's.
    assert!(
        !Request::SetDeviceTrust {
            device: None,
            trusted: true,
            auto_login: false,
        }
        .is_pre_auth()
    );
    // Pairing through a server, and an identity's record (P7-08): devices
    // with no session do both.
    assert!(Request::IdentityRecord { user: p7_08_user() }.is_pre_auth());
    assert!(Request::IdentityRecordByHandle { handle: [0x5a; 32] }.is_pre_auth());
    assert!(Request::OfferPairing(p7_08_offer()).is_pre_auth());
    assert!(
        Request::CollectPairing {
            code: p7_08_offer().code
        }
        .is_pre_auth()
    );
    assert!(!Request::Libraries.is_pre_auth());
    assert!(!Request::WhoAmI.is_pre_auth());
    assert!(!Request::Unknown.is_pre_auth());
}

#[test]
fn credentials_stay_out_of_debug_output() {
    let login = LoginPassword {
        username: "alice".into(),
        password: "hunter2".into(),
        remember: true,
    };
    assert!(!format!("{login:?}").contains("hunter2"));
    assert!(!format!("{:?}", SessionId("tok3n".into())).contains("tok3n"));
    let clear = Request::ClearPin {
        proof: PinProof::Password {
            password: "hunter2".into(),
        },
    };
    assert!(!format!("{clear:?}").contains("hunter2"));
    let change = Request::SetPin {
        pin: "2468".into(),
        proof: Some(PinProof::Pin { pin: "1357".into() }),
    };
    assert!(!format!("{change:?}").contains("1357"));
    let confirm = Request::ConfirmPassword(ConfirmPassword {
        password: "hunter2".into(),
    });
    assert!(!format!("{confirm:?}").contains("hunter2"));
}

/// What the Apple and Linux apps encode by hand must not drift: the tags
/// and the password's field name.
#[test]
fn confirm_requests_have_their_wire_names() {
    let encoded = |request: Request| {
        codec::encode(&RequestEnvelope {
            session: None,
            request,
        })
        .unwrap()
    };
    let password = encoded(Request::ConfirmPassword(ConfirmPassword {
        password: "hunter2".into(),
    }));
    assert_eq!(
        codec::request_type(&password).as_deref(),
        Some("confirm_password")
    );
    let value: ciborium::Value = ciborium::from_reader(password.as_slice()).unwrap();
    let request = value
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_text() == Some("request")))
        .map(|(_, v)| v.clone())
        .unwrap();
    let field = request
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_text() == Some("password")))
        .and_then(|(_, v)| v.as_text().map(str::to_owned));
    assert_eq!(field.as_deref(), Some("hunter2"));
    assert_eq!(
        codec::request_type(&encoded(Request::BeginConfirmPasskey)).as_deref(),
        Some("begin_confirm_passkey")
    );
    assert_eq!(
        codec::request_type(&encoded(Request::FinishConfirmPasskey(signed()))).as_deref(),
        Some("finish_confirm_passkey")
    );
}

/// Confirming may count a failure or use up a challenge: a transport retry
/// must not send it again on its own.
#[test]
fn confirm_requests_are_not_retried() {
    use crate::policy::retry_safe;
    assert!(!retry_safe(&Request::ConfirmPassword(ConfirmPassword {
        password: "hunter2".into(),
    })));
    assert!(!retry_safe(&Request::BeginConfirmPasskey));
    assert!(!retry_safe(&Request::FinishConfirmPasskey(signed())));
}

#[tokio::test]
async fn frames_round_trip_over_a_stream() {
    let (mut a, mut b) = tokio::io::duplex(64);
    let writer = tokio::spawn(async move {
        codec::write_frame(&mut a, &Request::Ping).await.unwrap();
        codec::write_frame(&mut a, &Event::ShuttingDown)
            .await
            .unwrap();
    });
    assert_eq!(
        codec::read_frame::<_, Request>(&mut b).await.unwrap(),
        Request::Ping
    );
    assert_eq!(
        codec::read_frame::<_, Event>(&mut b).await.unwrap(),
        Event::ShuttingDown
    );
    writer.await.unwrap();
}

#[tokio::test]
async fn an_oversized_frame_is_refused_before_it_is_read() {
    let mut bytes = (MAX_FRAME + 1).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"no payload follows");
    let err = codec::read_frame_bytes(&mut bytes.as_slice())
        .await
        .unwrap_err();
    assert!(matches!(err, CodecError::TooLarge { .. }), "{err}");
}

/// A client from before P2-03 and P2-07 sends `StartPlayback` with no
/// `start_ms` or `bitmap`. It was once read with their defaults; since
/// PX-15 it is refused.
#[test]
fn an_older_client_s_start_playback_does_not_decode() {
    #[derive(Serialize)]
    struct OldStart {
        #[serde(rename = "type")]
        kind: &'static str,
        id: &'static str,
        selection: Selection,
    }
    #[derive(Serialize)]
    struct Envelope {
        session: Option<SessionId>,
        request: OldStart,
    }
    let bytes = codec::encode(&Envelope {
        session: None,
        request: OldStart {
            kind: "start_playback",
            id: "plex:movie:1",
            selection: Selection::default(),
        },
    })
    .unwrap();
    let error = codec::decode::<RequestEnvelope>(&bytes).unwrap_err();
    assert!(error.to_string().contains("missing field"), "{error}");
}

/// A revision 14 client from before the display field (build 32) sends
/// `StartPlayback` without `display`: read as `None`, Hello's capabilities
/// (PGPU-06), not refused.
#[test]
fn a_start_playback_without_a_display_decodes_as_none() {
    #[derive(Serialize)]
    struct Start {
        #[serde(rename = "type")]
        kind: &'static str,
        id: &'static str,
        selection: Option<Selection>,
        start_ms: Option<u64>,
        bitmap: BitmapSubtitles,
        quality: Quality,
    }
    #[derive(Serialize)]
    struct Envelope {
        session: Option<SessionId>,
        request: Start,
    }
    let bytes = codec::encode(&Envelope {
        session: None,
        request: Start {
            kind: "start_playback",
            id: "plex:movie:1",
            selection: None,
            start_ms: None,
            bitmap: BitmapSubtitles::Auto,
            quality: Quality::Original,
        },
    })
    .unwrap();
    let envelope = codec::decode::<RequestEnvelope>(&bytes).unwrap();
    assert!(
        matches!(
            envelope.request,
            Request::StartPlayback { display: None, .. }
        ),
        "{:?}",
        envelope.request
    );
}

/// One that sends a display has it read back.
#[test]
fn a_start_playback_s_display_round_trips() {
    let display = spoolframe_types::DisplayCapabilities {
        bit_depth: Some(10),
        hdr: Some([spoolframe_types::HdrFormat::Hdr10].into()),
    };
    let request = Request::StartPlayback {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        selection: None,
        start_ms: None,
        bitmap: BitmapSubtitles::Auto,
        quality: Quality::Original,
        display: Some(display.clone()),
    };
    let decoded: Request = codec::decode(&codec::encode(&request).unwrap()).unwrap();
    assert!(
        matches!(&decoded, Request::StartPlayback { display: Some(d), .. } if *d == display),
        "{decoded:?}"
    );
}

#[test]
fn a_bitmap_mode_from_a_newer_client_is_unknown() {
    let mode: BitmapSubtitles = codec::decode(&codec::encode(&"draw_in_3d").unwrap()).unwrap();
    assert_eq!(mode, BitmapSubtitles::Unknown);
}

/// The item and started-playback replies as they were before P2-10, with
/// no `markers` (nor `chosen`): what an older server sent and an older
/// client reads.
mod before_markers {
    use crate::{PlaybackHandle, PlaybackPlan};
    use serde::{Deserialize, Serialize};
    use spoolframe_types::Item;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[expect(
        clippy::large_enum_variant,
        reason = "an older peer's reply, made once to be encoded; as the real Reply"
    )]
    pub enum Reply {
        Item {
            item: Item,
            children: Vec<Item>,
        },
        PlaybackStarted {
            handle: PlaybackHandle,
            plan: PlaybackPlan,
            size: u64,
            epoch: u32,
        },
    }
}

fn playback_started(markers: Vec<Marker>) -> Reply {
    Reply::PlaybackStarted {
        handle: crate::PlaybackHandle("h".into()),
        plan: PlaybackPlan::Unsupported { reasons: vec![] },
        size: 12_345,
        chosen: None,
        epoch: 0,
        markers,
    }
}

/// An older server's replies were once read with no markers; since
/// PX-15 they are refused.
#[test]
fn an_older_server_s_replies_without_markers_do_not_decode() {
    let item = codec::encode(&before_markers::Reply::Item {
        item: movie(),
        children: Vec::new(),
    })
    .unwrap();
    let started = codec::encode(&before_markers::Reply::PlaybackStarted {
        handle: crate::PlaybackHandle("h".into()),
        plan: PlaybackPlan::Unsupported { reasons: vec![] },
        size: 12_345,
        epoch: 0,
    })
    .unwrap();
    for bytes in [item, started] {
        let error = codec::decode::<Reply>(&bytes).unwrap_err();
        assert!(error.to_string().contains("missing field"), "{error}");
    }
}

#[test]
fn an_older_client_reads_an_item_that_has_markers() {
    let bytes = codec::encode(&Reply::Item {
        item: movie(),
        children: Vec::new(),
        markers: markers(),
        playlists: None,
        liked: None,
        liked_children: None,
    })
    .unwrap();
    let before_markers::Reply::Item { item, children } = codec::decode(&bytes).unwrap() else {
        panic!("not an item")
    };
    assert_eq!((item, children), (movie(), Vec::new()));
}

#[test]
fn an_older_client_reads_a_started_playback_that_has_markers() {
    let bytes = codec::encode(&playback_started(markers())).unwrap();
    let before_markers::Reply::PlaybackStarted { handle, size, .. } =
        codec::decode(&bytes).unwrap()
    else {
        panic!("not a started playback")
    };
    assert_eq!((handle.0.as_str(), size), ("h", 12_345));
}

/// A reply is one frame: a whole chunk has to fit one, with its framing.
#[test]
fn a_whole_subtitle_chunk_fits_a_frame() {
    let reply = Response::Ok(Reply::SubtitleChunk {
        format: SubtitleFormat::WebVtt,
        bytes: vec![0xAB; crate::SUBTITLE_CHUNK as usize],
        total: u64::MAX,
        version: u64::MAX,
    });
    assert!(codec::encode(&reply).unwrap().len() < MAX_FRAME as usize);
}

/// A marker kind from a newer server does not cost the client the item or
/// its other markers.
#[test]
fn a_marker_kind_from_a_newer_server_is_unknown() {
    #[derive(Serialize)]
    struct NewerMarker {
        kind: &'static str,
        start_ms: u64,
        end_ms: u64,
        to_end: bool,
    }
    #[derive(Serialize)]
    struct NewerItem {
        #[serde(rename = "type")]
        kind: &'static str,
        item: Item,
        children: Vec<Item>,
        markers: Vec<NewerMarker>,
    }
    let bytes = codec::encode(&NewerItem {
        kind: "item",
        item: movie(),
        children: Vec::new(),
        markers: vec![
            NewerMarker {
                kind: "recap",
                start_ms: 0,
                end_ms: 45_000,
                to_end: false,
            },
            NewerMarker {
                kind: "intro",
                start_ms: 45_000,
                end_ms: 105_000,
                to_end: false,
            },
        ],
    })
    .unwrap();
    let Reply::Item { markers, .. } = codec::decode(&bytes).unwrap() else {
        panic!("not an item")
    };
    assert_eq!(
        markers.iter().map(|m| m.kind).collect::<Vec<_>>(),
        [MarkerKind::Unknown, MarkerKind::Intro]
    );
}

/// A resume makes a session and a trust change can end them: neither may be
/// sent again on its own; listing the trusted profiles may (P4-01).
#[test]
fn trust_requests_are_retried_only_when_they_read() {
    use crate::policy::retry_safe;
    assert!(retry_safe(&Request::TrustedProfiles));
    assert!(!retry_safe(&Request::ResumeProfile {
        user_id: "local:x".into()
    }));
    assert!(!retry_safe(&Request::SetDeviceTrust {
        device: None,
        trusted: false,
        auto_login: false,
    }));
}

/// Deleting a device (P4-03) is the session user's, ends sessions, and so
/// is never sent twice on its own; the apps encode it by hand. Its device
/// is always named: there is no "this device" form, which is `Logout`.
#[test]
fn forgetting_a_device_needs_a_session_is_not_retried_and_names_the_device() {
    use crate::policy::retry_safe;
    let forget = Request::ForgetDevice {
        device: "ab".into(),
    };
    assert!(!forget.is_pre_auth());
    assert!(!retry_safe(&forget));
    assert_eq!(
        serde_json::to_value(&forget).unwrap(),
        serde_json::json!({"type": "forget_device", "device": "ab"})
    );
    let unnamed: Result<Request, _> =
        serde_json::from_value(serde_json::json!({"type": "forget_device"}));
    assert!(unnamed.is_err(), "{unnamed:?}");
}

/// What the apps encode by hand (P4-01): the tags and field names.
#[test]
fn trust_requests_have_their_wire_names() {
    let value = |request: Request| -> serde_json::Value { serde_json::to_value(request).unwrap() };
    assert_eq!(
        value(Request::TrustedProfiles),
        serde_json::json!({"type": "trusted_profiles"})
    );
    assert_eq!(
        value(Request::ResumeProfile {
            user_id: "local:u".into()
        }),
        serde_json::json!({"type": "resume_profile", "user_id": "local:u"})
    );
    assert_eq!(
        value(Request::SetDeviceTrust {
            device: None,
            trusted: true,
            auto_login: false
        }),
        serde_json::json!({
            "type": "set_device_trust", "device": null, "trusted": true, "auto_login": false
        })
    );
    assert_eq!(
        value(Request::BeginDeviceSignIn { remember: true }),
        serde_json::json!({"type": "begin_device_sign_in", "remember": true})
    );
}

/// "Remember" is never assumed (P4-01): a sign-in without it is refused,
/// not read as a sign-in that remembers or forgets.
#[test]
fn a_sign_in_must_say_whether_to_remember() {
    let without: Result<Request, _> = serde_json::from_value(serde_json::json!({
        "type": "login_password", "username": "alice", "password": "hunter2"
    }));
    assert!(without.is_err(), "{without:?}");
    let without: Result<Request, _> =
        serde_json::from_value(serde_json::json!({"type": "begin_device_sign_in"}));
    assert!(without.is_err(), "{without:?}");
}

/// The server's addresses come only as an event since revision 7: a
/// request for them is one this build does not know, and the server
/// answers it `Unsupported`.
#[test]
fn the_server_addresses_request_is_gone() {
    let request: Request =
        serde_json::from_value(serde_json::json!({"type": "server_addresses"})).unwrap();
    assert_eq!(request, Request::Unknown);
}

/// `value` in CBOR, then each of `fields` left out of its map in turn:
/// every one must be refused as missing, not filled in (PX-15).
fn assert_each_required<T: Serialize + DeserializeOwned + Debug>(value: &T, fields: &[&str]) {
    let full: ciborium::Value =
        ciborium::from_reader(codec::encode(value).unwrap().as_slice()).unwrap();
    for field in fields {
        let ciborium::Value::Map(mut entries) = full.clone() else {
            panic!("{value:?} is not a map");
        };
        let before = entries.len();
        entries.retain(|(key, _)| key.as_text() != Some(field));
        assert_eq!(
            entries.len(),
            before - 1,
            "{field} is not a key of {value:?}"
        );
        let error = codec::decode::<T>(&codec::encode(&ciborium::Value::Map(entries)).unwrap())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("missing field `{field}`")),
            "{field}: {error}"
        );
    }
}

fn movie_id() -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap()
}

/// What was `serde(default)` before PX-15 is required: the requests.
#[test]
fn a_request_missing_a_field_is_refused() {
    assert_each_required(
        &RequestEnvelope {
            session: None,
            request: Request::Ping,
        },
        &["session"],
    );
    assert_each_required(&hello(), &["device_name", "capabilities"]);
    assert_each_required(
        &Request::PlaybackPlan {
            id: movie_id(),
            selection: None,
            bitmap: BitmapSubtitles::Auto,
            display: None,
        },
        &["selection", "bitmap"],
    );
    assert_each_required(
        &Request::StartPlayback {
            id: movie_id(),
            selection: None,
            start_ms: None,
            bitmap: BitmapSubtitles::Auto,
            quality: Default::default(),
            display: None,
        },
        &["selection", "start_ms", "bitmap"],
    );
    assert_each_required(
        &Request::SetPinRequired {
            device: None,
            required: true,
        },
        &["device"],
    );
    assert_each_required(
        &Request::ChangeSubtitle {
            handle: crate::PlaybackHandle("h".into()),
            subtitle: None,
            bitmap: BitmapSubtitles::Auto,
            position_ms: 0,
            display: None,
        },
        &["bitmap"],
    );
    assert_each_required(
        &Selection {
            audio: None,
            subtitle: None,
        },
        &["audio", "subtitle"],
    );
}

/// And the replies.
#[test]
fn a_reply_missing_a_field_is_refused() {
    assert_each_required(
        &Reply::Item {
            item: movie(),
            children: Vec::new(),
            markers: Vec::new(),
            playlists: None,
            liked: None,
            liked_children: None,
        },
        &["markers"],
    );
    assert_each_required(
        &Reply::PlaybackPlan {
            plan: PlaybackPlan::Unsupported { reasons: vec![] },
            chosen: None,
        },
        &["chosen"],
    );
    assert_each_required(
        &playback_started(Vec::new()),
        &["chosen", "epoch", "markers"],
    );
    assert_each_required(
        &UserSummary {
            user_id: "local:1".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            role: spoolframe_types::Role::Consumer,
            locked: false,
            pin: None,
            portable: Vec::new(),
        },
        &["locked", "portable"],
    );
    assert_each_required(
        &crate::DeviceInfo {
            endpoint_id: "ab".into(),
            name: None,
            platform: None,
            last_seen_at: 0,
            pin_required: false,
            trusted: false,
            auto_login: false,
            this_device: false,
        },
        &["name"],
    );
    assert_each_required(
        &crate::DeviceSignInRequest {
            device_name: None,
            client_name: "spoolframe-tvos".into(),
            platform: spoolframe_types::Platform::Tvos,
            requested_at: 0,
            remember: false,
            requested_from: None,
        },
        &["device_name", "requested_from"],
    );
    assert_each_required(
        &crate::DeviceAddress {
            address: "127.0.0.1".into(),
            network: crate::AddressNetwork::Loopback,
        },
        &["address", "network"],
    );
}

/// And the media reads.
#[test]
fn a_media_read_or_its_reply_missing_a_field_is_refused() {
    assert_each_required(
        &crate::MediaRead {
            session: SessionId("s".into()),
            handle: crate::PlaybackHandle("h".into()),
            offset: 0,
            length: 1,
            epoch: 0,
            segment: None,
            variant: 0,
        },
        &["epoch", "segment"],
    );
    assert_each_required(
        &crate::MediaReadReply {
            sent: 1,
            size: 1,
            complete: true,
            epoch: 0,
        },
        &["complete", "epoch"],
    );
}

/// Where a device asked to sign in from (PX-09 finding 27): each network
/// by its wire name, and one a newer server names is `Unknown`.
#[test]
fn a_device_address_s_network_has_its_wire_name() {
    use crate::{AddressNetwork, DeviceAddress};
    for (network, name) in [
        (AddressNetwork::Loopback, "loopback"),
        (AddressNetwork::Local, "local"),
        (AddressNetwork::Internet, "internet"),
    ] {
        let address = DeviceAddress {
            address: "203.0.113.9".into(),
            network,
        };
        round_trip(address.clone());
        let value: ciborium::Value =
            ciborium::from_reader(codec::encode(&address).unwrap().as_slice()).unwrap();
        let ciborium::Value::Map(entries) = value else {
            panic!("not a map");
        };
        assert!(
            entries
                .iter()
                .any(|(key, value)| key.as_text() == Some("network")
                    && value.as_text() == Some(name)),
            "{network:?}"
        );
    }
    #[derive(Serialize)]
    struct Newer {
        address: &'static str,
        network: &'static str,
    }
    let bytes = codec::encode(&Newer {
        address: "203.0.113.9",
        network: "satellite",
    })
    .unwrap();
    assert_eq!(
        codec::decode::<DeviceAddress>(&bytes).unwrap().network,
        AddressNetwork::Unknown
    );
}

fn redeem() -> crate::RedeemInvite {
    crate::RedeemInvite {
        code: "QW7K2MFPXR4T".into(),
        username: "mum".into(),
        display_name: Some("Mum".into()),
        password: "correct horse".into(),
    }
}

/// Redeeming an invite (P6-01) is a sign-in a stranger makes: next to
/// `Hello` and `LoginPassword`, before any session.
#[test]
fn redeeming_an_invite_round_trips_and_is_pre_auth() {
    round_trip(RequestEnvelope {
        session: None,
        request: Request::RedeemInvite(redeem()),
    });
    assert!(Request::RedeemInvite(redeem()).is_pre_auth());
    let bytes = codec::encode(&RequestEnvelope {
        session: None,
        request: Request::RedeemInvite(redeem()),
    })
    .unwrap();
    assert_eq!(
        codec::request_type(&bytes).as_deref(),
        Some("redeem_invite")
    );
}

/// Every field of it is required, the display name too, sent as `null`
/// for none.
#[test]
fn a_redemption_missing_a_field_is_refused() {
    assert_each_required(
        &Request::RedeemInvite(crate::RedeemInvite {
            display_name: None,
            ..redeem()
        }),
        &["code", "username", "display_name", "password"],
    );
}

/// It makes an account and uses up the invite: never sent twice on its
/// own.
#[test]
fn a_redemption_is_not_retried() {
    assert!(!crate::policy::retry_safe(&Request::RedeemInvite(redeem())));
}

#[test]
fn a_redemption_keeps_its_code_and_password_out_of_debug_output() {
    let shown = format!("{:?}", Request::RedeemInvite(redeem()));
    assert!(!shown.contains("QW7K"), "{shown}");
    assert!(!shown.contains("correct horse"), "{shown}");
    assert!(shown.contains("mum"), "{shown}");
}

#[test]
fn the_invite_errors_round_trip() {
    for error in [
        Error::InviteInvalid,
        Error::InviteExpired,
        Error::InviteExhausted,
        Error::UsernameTaken,
    ] {
        round_trip(Response::Err(error));
    }
}

/// P7-04's root: seed 7, as the portable vectors.
fn tie_root() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7; 32])
}

fn tie_user() -> crate::PortableUserId {
    crate::PortableUserId::from_bytes(tie_root().verifying_key().to_bytes()).unwrap()
}

fn tie_proofs() -> Vec<crate::TieProof> {
    use ed25519_dalek::Signer;
    let message = crate::tie_message(&[0x5a; 32], &[0xa5; 32], &tie_user(), "alice").unwrap();
    let device = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
    let credential = crate::UserDeviceCredential::new(
        tie_user(),
        crate::DevicePublicKey::from_bytes(device.verifying_key().to_bytes()).unwrap(),
        1,
        1_791_504_000,
        1_791_504_000 + 86_400,
        |m| tie_root().sign(m),
    );
    vec![
        crate::TieProof::Root {
            signature: tie_root().sign(&message).into(),
            root: None,
            genesis: None,
        },
        crate::TieProof::Device {
            credential,
            signature: device.sign(&message),
        },
    ]
}

/// P7-04's requests and replies go over the wire as they are, and each
/// needs a session.
#[test]
fn the_tie_messages_round_trip() {
    let mut requests = vec![Request::TieBegin, Request::Untie { user: tie_user() }];
    requests.extend(tie_proofs().into_iter().map(|proof| Request::TieFinish {
        user: tie_user(),
        proof,
    }));
    for request in requests {
        assert!(!request.is_pre_auth(), "{request:?}");
        round_trip(request);
    }
    round_trip(Response::Ok(Reply::TieChallenge {
        nonce: [0xa5; 32],
        server: [0x5a; 32],
        expires_in_secs: 300,
    }));
    for error in [Error::TieChallengeExpired, Error::PortableIdTaken] {
        round_trip(Response::Err(error));
    }
}

/// An account's ties (P7-09): two, one used and one never, as `WhoAmI`
/// lists them.
fn ties() -> Vec<crate::PortableTie> {
    vec![
        crate::PortableTie {
            id: tie_user(),
            tied_at: 1_791_504_000,
            last_used_at: Some(1_791_590_400),
        },
        crate::PortableTie {
            id: crate::PortableUserId::from_bytes([0x33; 32]).unwrap(),
            tied_at: 1_791_504_100,
            last_used_at: None,
        },
    ]
}

/// P7-09: `UserSummary.portable` lists the account's ties, each with its
/// identity, when it was tied and when it was last used, every field
/// required; `Untie` names the identity to sever.
#[test]
fn an_account_s_ties_are_a_list_of_whole_ties() {
    round_trip(Reply::WhoAmI(admin_who_am_i(true)));
    for tie in ties() {
        assert_each_required(&tie, &["id", "tied_at", "last_used_at"]);
    }
    assert_each_required(&Request::Untie { user: tie_user() }, &["user"]);
}

/// Its nonce and server id are CBOR byte strings, and every field is
/// required.
#[test]
fn a_tie_challenge_is_bytes_and_whole() {
    let challenge = Reply::TieChallenge {
        nonce: [0xa5; 32],
        server: [0x5a; 32],
        expires_in_secs: 300,
    };
    let value: ciborium::Value =
        ciborium::from_reader(codec::encode(&challenge).unwrap().as_slice()).unwrap();
    let ciborium::Value::Map(entries) = value else {
        panic!("not a map");
    };
    let nonce = entries
        .iter()
        .find(|(key, _)| key.as_text() == Some("nonce"))
        .map(|(_, value)| value.clone());
    assert_eq!(nonce, Some(ciborium::Value::Bytes(vec![0xa5; 32])));
    assert_each_required(&challenge, &["nonce", "server", "expires_in_secs"]);
    assert_each_required(
        &Request::TieFinish {
            user: tie_user(),
            proof: tie_proofs().remove(0),
        },
        &["user", "proof"],
    );
}

/// Neither is a read: a tie uses its nonce up, a sever changes the
/// account. A second challenge only replaces the first.
#[test]
fn a_tie_is_never_sent_twice_but_a_challenge_may_be() {
    use crate::policy::retry_safe;
    assert!(retry_safe(&Request::TieBegin));
    assert!(!retry_safe(&Request::Untie { user: tie_user() }));
    for proof in tie_proofs() {
        assert!(!retry_safe(&Request::TieFinish {
            user: tie_user(),
            proof
        }));
    }
}

fn p7_08_user() -> crate::PortableUserId {
    crate::PortableUserId::from_bytes([0x33; 32]).unwrap()
}

fn p7_08_offer() -> crate::pairing::PairingOffer {
    use ed25519_dalek::SigningKey;
    crate::pairing::PairingOffer {
        code: crate::pairing::RelayCode::parse("ABCDE-FGHJK").unwrap(),
        device_key: crate::DevicePublicKey::from_bytes(
            SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes(),
        )
        .unwrap(),
        name: "Living room".into(),
    }
}

fn p7_08_answer() -> crate::pairing::PairingAnswer {
    use ed25519_dalek::{Signer, SigningKey};
    let root = SigningKey::from_bytes(&[7; 32]);
    let user = crate::PortableUserId::from_bytes(root.verifying_key().to_bytes()).unwrap();
    crate::pairing::PairingAnswer {
        credential: Box::new(crate::UserDeviceCredential::new(
            user,
            p7_08_offer().device_key,
            1,
            1,
            2,
            |m| root.sign(m),
        )),
        state: None,
        servers: vec![],
    }
}

#[test]
fn a_relay_code_reads_as_a_person_types_it() {
    use crate::pairing::RelayCode;
    let code = RelayCode::parse("abcde fghjk").unwrap();
    assert_eq!(code.as_str(), "ABCDEFGHJK");
    assert_eq!(code.display(), "ABCDE-FGHJK");
    // I and L read as 1, O as 0.
    assert_eq!(
        RelayCode::parse("IL0O1-23456").unwrap().as_str(),
        "11001".to_owned() + "23456"
    );
    assert!(RelayCode::parse("ABCDE").is_none());
    assert!(RelayCode::parse("ABCDE-FGHJU").is_none());
    assert!(serde_json::from_str::<RelayCode>("\"UUUUUUUUUU\"").is_err());
    let random = RelayCode::from_random([255; 10]);
    assert_eq!(RelayCode::parse(random.as_str()), Some(random));
}

/// P7-08: a lookup by genesis handle is its 64 hex digits on the wire,
/// as the genesis writes its handle.
#[test]
fn an_identity_record_by_handle_is_its_hex() {
    let request = Request::IdentityRecordByHandle { handle: [0xab; 32] };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"type": "identity_record_by_handle", "handle": "ab".repeat(32)})
    );
    assert_eq!(serde_json::from_value::<Request>(json).unwrap(), request);
    assert!(
        serde_json::from_value::<Request>(
            serde_json::json!({"type": "identity_record_by_handle", "handle": "ab"})
        )
        .is_err()
    );
}

/// A server from before one-time passwords sends `LoggedIn` without
/// `password_change_required`: read as false, not refused.
#[test]
fn a_logged_in_without_password_change_required_decodes_as_false() {
    #[derive(Serialize)]
    struct LoggedIn {
        #[serde(rename = "type")]
        kind: &'static str,
        session: SessionId,
        remembered: bool,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Response {
        Ok(LoggedIn),
    }
    let bytes = codec::encode(&Response::Ok(LoggedIn {
        kind: "logged_in",
        session: SessionId("s".into()),
        remembered: true,
    }))
    .unwrap();
    let decoded = codec::decode::<crate::Response>(&bytes).unwrap();
    assert!(
        matches!(
            decoded,
            crate::Response::Ok(Reply::LoggedIn {
                remembered: true,
                password_change_required: false,
                ..
            })
        ),
        "{decoded:?}"
    );
}

/// `ChangePassword` round-trips, and its debug form shows no password.
#[test]
fn a_change_password_keeps_its_passwords_out_of_the_log() {
    let request = Request::ChangePassword(crate::ChangePassword {
        current_password: "ABCD-EFGH-JKMN-PQRS".into(),
        new_password: "correct horse battery".into(),
    });
    let bytes = codec::encode(&request).unwrap();
    assert_eq!(codec::decode::<Request>(&bytes).unwrap(), request);
    let shown = format!("{request:?}");
    assert!(
        !shown.contains("ABCD") && !shown.contains("horse"),
        "{shown}"
    );
}

/// PX-47: leaving goes over the wire as it is, needs a session, and is
/// never sent twice; `LastAdmin` is its refusal.
#[test]
fn leaving_a_server_round_trips_and_is_never_retried() {
    let request = Request::LeaveServer;
    assert!(!request.is_pre_auth());
    assert!(!crate::policy::retry_safe(&request));
    round_trip(request);
    round_trip(Response::Err(Error::LastAdmin));
}

fn admin_who_am_i(can_leave: bool) -> WhoAmIReply {
    WhoAmIReply {
        user: UserSummary {
            user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            username: "root".into(),
            display_name: "Root".into(),
            role: spoolframe_types::Role::Admin,
            locked: false,
            pin: None,
            portable: ties(),
        },
        can_leave,
        features: Vec::new(),
    }
}

/// Whether the user may leave rides in `WhoAmI`, beside the user's own
/// fields: an older client, which reads the answer as a `UserSummary`,
/// still reads it.
#[test]
fn who_am_i_carries_whether_the_user_can_leave() {
    for can_leave in [false, true] {
        round_trip(Reply::WhoAmI(admin_who_am_i(can_leave)));
    }
    let reply = admin_who_am_i(false);
    let older: UserSummary = codec::decode(&codec::encode(&reply).unwrap()).unwrap();
    assert_eq!(older, reply.user);
}

/// A server before `can_leave` leaves it out: the app offers leaving,
/// and that server's `LastAdmin` refusal is what stops the last admin.
#[test]
fn a_who_am_i_without_can_leave_lets_the_user_leave() {
    let full: ciborium::Value = ciborium::from_reader(
        codec::encode(&Reply::WhoAmI(admin_who_am_i(false)))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    let ciborium::Value::Map(mut entries) = full else {
        panic!("a reply is a map");
    };
    entries.retain(|(key, _)| key.as_text() != Some("can_leave"));
    let older = codec::encode(&ciborium::Value::Map(entries)).unwrap();
    assert_eq!(
        codec::decode::<Reply>(&older).unwrap(),
        Reply::WhoAmI(admin_who_am_i(true))
    );
}

/// PX-49: the experimental features a Hello asks for, and those a
/// server's answer names.
mod experimental_features {
    use super::*;
    use crate::MAX_HELLO_TEXT;
    use crate::experimental::{self, HTTP_PLAYBACK, HttpPlaybackRefusal};

    /// `Hello` as a server before PX-49 reads it, and as such a client
    /// sends it.
    #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
    struct OlderHello {
        protocol_version: u32,
        client_name: String,
        client_version: String,
        platform: spoolframe_types::Platform,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        device_name: Option<String>,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        capabilities: Option<ClientCapabilities>,
    }

    fn older(hello: &Hello) -> OlderHello {
        OlderHello {
            protocol_version: hello.protocol_version,
            client_name: hello.client_name.clone(),
            client_version: hello.client_version.clone(),
            platform: hello.platform,
            device_name: hello.device_name.clone(),
            capabilities: hello.capabilities.clone(),
        }
    }

    fn asking(names: &[&str]) -> Hello {
        Hello {
            experimental: names.iter().map(|n| n.to_string()).collect(),
            features: Vec::new(),
            ..hello()
        }
    }

    #[test]
    fn a_hello_round_trips_with_and_without_the_features_it_asks_for() {
        round_trip(Request::Hello(asking(&[])));
        round_trip(Request::Hello(asking(&[HTTP_PLAYBACK, "teleport"])));
    }

    #[test]
    fn a_hello_asking_for_none_is_what_an_older_client_sends() {
        assert_eq!(
            codec::encode(&asking(&[])).unwrap(),
            codec::encode(&older(&hello())).unwrap()
        );
    }

    #[test]
    fn an_older_client_s_hello_asks_for_none() {
        let bytes = codec::encode(&older(&hello())).unwrap();
        let hello: Hello = codec::decode(&bytes).unwrap();
        assert!(hello.experimental.is_empty());
    }

    #[test]
    fn an_older_server_reads_a_hello_that_asks_for_features() {
        let asked = asking(&[HTTP_PLAYBACK]);
        let bytes = codec::encode(&asked).unwrap();
        assert_eq!(codec::decode::<OlderHello>(&bytes).unwrap(), older(&asked));
    }

    fn info(experimental: Vec<String>) -> ServerInfo {
        ServerInfo {
            name: "Home Media".into(),
            endpoint_id: "e9c1".into(),
            server_id: "mzxw6ytbojtg633c".into(),
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.8.0".into(),
            auth_methods: vec![AuthMethod::Password],
            web_url: None,
            experimental,
            features: Vec::new(),
        }
    }

    #[test]
    fn a_server_s_answer_names_what_it_turned_on_and_an_older_one_names_none() {
        round_trip(Reply::ServerInfo(info(vec![HTTP_PLAYBACK.into()])));
        let mut value = ciborium::Value::serialized(&info(vec![HTTP_PLAYBACK.into()])).unwrap();
        let ciborium::Value::Map(fields) = &mut value else {
            panic!("{value:?}");
        };
        fields.retain(|(key, _)| key.as_text() != Some("experimental"));
        let older = codec::decode::<ServerInfo>(&codec::encode(&value).unwrap()).unwrap();
        assert!(older.experimental.is_empty());
        // And none named is no field: what an older server sends.
        let none: ciborium::Value =
            ciborium::from_reader(codec::encode(&info(Vec::new())).unwrap().as_slice()).unwrap();
        let ciborium::Value::Map(fields) = none else {
            panic!("{none:?}");
        };
        assert!(
            fields
                .iter()
                .all(|(k, _)| k.as_text() != Some("experimental"))
        );
    }

    #[test]
    fn a_server_turns_on_only_what_it_knows_has_on_and_was_asked() {
        let asked: Vec<String> = ["teleport", HTTP_PLAYBACK, HTTP_PLAYBACK]
            .map(String::from)
            .to_vec();
        assert_eq!(
            experimental::turned_on(&asked, &[HTTP_PLAYBACK]),
            vec![HTTP_PLAYBACK.to_owned()]
        );
        assert!(experimental::turned_on(&asked, &[]).is_empty());
        assert!(experimental::turned_on(&[], &[HTTP_PLAYBACK]).is_empty());
    }

    #[test]
    fn a_hello_keeps_a_bounded_list_of_bounded_names() {
        let long = "x".repeat(MAX_HELLO_TEXT + 5);
        let many: Vec<String> = (0..experimental::MAX_ASKED + 3)
            .map(|n| format!("{long}{n}"))
            .collect();
        let hello = Hello {
            experimental: many,
            ..hello()
        }
        .within_bounds();
        assert_eq!(hello.experimental.len(), experimental::MAX_ASKED);
        assert!(
            hello
                .experimental
                .iter()
                .all(|n| n.chars().count() == MAX_HELLO_TEXT)
        );
    }

    #[test]
    fn the_http_playback_request_and_its_answers_round_trip() {
        let handle = crate::PlaybackHandle("h".into());
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request: Request::HttpPlaybackUrl {
                handle: handle.clone(),
            },
        });
        let bytes = codec::encode(&RequestEnvelope {
            session: None,
            request: Request::HttpPlaybackUrl { handle },
        })
        .unwrap();
        assert_eq!(
            codec::request_type(&bytes).as_deref(),
            Some("http_playback_url")
        );
        round_trip(Reply::HttpPlaybackUrl {
            url: "http://192.168.1.10:7422/abc/master.m3u8".into(),
        });
        for reason in [
            HttpPlaybackRefusal::Off,
            HttpPlaybackRefusal::RelayOnly,
            HttpPlaybackRefusal::NoSuchPlayback,
            HttpPlaybackRefusal::Unsupported,
            HttpPlaybackRefusal::NoAddress,
        ] {
            round_trip(Reply::HttpPlaybackRefused { reason });
        }
    }

    #[test]
    fn a_refusal_from_a_newer_server_is_unknown() {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Newer {
            HttpPlaybackRefused { reason: &'static str },
        }
        let bytes = codec::encode(&Newer::HttpPlaybackRefused { reason: "on_fire" }).unwrap();
        assert_eq!(
            codec::decode::<Reply>(&bytes).unwrap(),
            Reply::HttpPlaybackRefused {
                reason: HttpPlaybackRefusal::Unknown
            }
        );
    }

    #[test]
    fn asking_for_a_url_needs_a_session_and_may_be_asked_again() {
        let request = Request::HttpPlaybackUrl {
            handle: crate::PlaybackHandle("h".into()),
        };
        assert!(!request.is_pre_auth());
        assert!(crate::policy::retry_safe(&request));
        assert_eq!(crate::policy::server_wait(&request), Duration::ZERO);
    }
}

/// PX-40's messages, and what an older peer makes of them.
mod playlists {
    use super::*;
    use crate::playlists::{
        ItemKind, Kinds, PinNote, PinNoteReason, PlaylistEdit, PlaylistEntry, PlaylistOwnerKind,
        PlaylistPage, PlaylistShare, PlaylistSummary, ShareCandidate, ShareCandidatePage, UserKind,
        Visibility,
    };

    const EPISODES: Kinds = Kinds {
        movies: false,
        episodes: true,
        tracks: false,
    };

    fn summary() -> PlaylistSummary {
        PlaylistSummary {
            id: "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            name: "Heists".into(),
            kinds: EPISODES,
            owner_kind: PlaylistOwnerKind::Mine,
            visibility: Visibility::Shared {
                with: vec![PlaylistShare::new(
                    "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4d",
                    true,
                )],
            },
            can_edit: true,
            can_manage: true,
            revision: 7,
            pinned: Some(0),
            builtin: None,
            owner_name: None,
            artwork: None,
            custom_art: false,
        }
    }

    /// `spoolframe:playlist-art` (PX-53): a summary's picture, and the two
    /// requests; the fields are left out when not given, and an older
    /// peer's summary, without them, reads as none.
    #[test]
    fn playlist_art_round_trips_and_is_left_out_when_not_given() {
        let cover: spoolframe_types::ArtworkId = "image:playlist/0f3a".parse().unwrap();
        let with = PlaylistSummary {
            artwork: Some(cover.clone()),
            custom_art: true,
            ..summary()
        };
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["artwork"], "image:playlist/0f3a");
        assert_eq!(json["custom_art"], true);
        round_trip(Response::Ok(Reply::PlaylistInfo(with)));
        let json = serde_json::to_value(summary()).unwrap();
        assert!(json.get("artwork").is_none(), "{json}");
        assert!(json.get("custom_art").is_none(), "{json}");
        let remove = Request::RemovePlaylistArt { id: "x".into() };
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request: remove.clone(),
        });
        assert!(crate::policy::retry_safe(&remove));
        round_trip(crate::resource::ResourceRequest::SetPlaylistArt {
            session: SessionId("s".into()),
            id: "x".into(),
            length: 1234,
        });
        round_trip(crate::resource::ResourceResponse::Ready {
            length: 0,
            metadata: crate::resource::ResourceMetadata::PlaylistArt { artwork: cover },
        });
        for error in [
            Error::NotAllowed,
            Error::ImageTooLarge {
                max_bytes: crate::playlists::MAX_ART_BYTES,
                max_pixels: crate::playlists::MAX_ART_PIXELS,
            },
            Error::ImageUnreadable,
        ] {
            round_trip(Response::Err(error));
        }
    }

    #[test]
    fn every_playlist_message_round_trips() {
        let id = "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".to_owned();
        for request in [
            Request::ListPlaylists,
            Request::GetPlaylist {
                id: id.clone(),
                offset: 0,
                limit: 50,
            },
            Request::CreatePlaylist {
                name: "Heists".into(),
                kinds: EPISODES,
                visibility: Visibility::ServerWide,
            },
            Request::UpdatePlaylist {
                id: id.clone(),
                name: None,
                kinds: Some(EPISODES),
                visibility: Some(Visibility::Personal),
            },
            Request::EditPlaylist {
                id: id.clone(),
                revision: 7,
                edits: vec![
                    PlaylistEdit::Add {
                        id: movie().id().clone(),
                        before: Some(3),
                    },
                    PlaylistEdit::Remove { entry: 4 },
                    PlaylistEdit::Move {
                        entry: 5,
                        before: None,
                    },
                ],
            },
            Request::DeletePlaylist { id: id.clone() },
            Request::PinPlaylist {
                id: id.clone(),
                position: 2,
            },
            Request::PlayPlaylist {
                id: id.clone(),
                from: Some(3),
                shuffle: false,
                repeat: crate::playlists::Repeat::One,
                skip_watched: true,
            },
            Request::NextInPlaylist {
                id: id.clone(),
                after: 3,
            },
            Request::ImportPlexPlaylists { dry_run: true },
            Request::UnpinPlaylist { id },
            Request::DismissPinNotes,
            Request::Like {
                items: vec![movie().id().clone()],
            },
            Request::Unlike {
                items: vec![movie().id().clone()],
            },
        ] {
            assert!(!request.is_pre_auth(), "{request:?}");
            round_trip(RequestEnvelope {
                session: Some(SessionId("s".into())),
                request,
            });
        }
        for reply in [
            Reply::Playlists {
                playlists: vec![summary()],
                pin_notes: vec![PinNote {
                    playlist_id: "x".into(),
                    name: "Gone".into(),
                    reason: PinNoteReason::Deleted,
                }],
            },
            Reply::Playlist(PlaylistPage {
                playlist: summary(),
                entries: vec![PlaylistEntry {
                    entry: 3,
                    item: movie(),
                    unavailable: true,
                    liked: false,
                }],
                offset: 0,
                total: 18,
                playable: 17,
                place: Some(3),
                settings: crate::playlists::PlaySettings {
                    shuffle: true,
                    repeat: crate::playlists::Repeat::All,
                    skip_watched: true,
                },
            }),
            Reply::PlaylistItem(crate::playlists::PlaylistNext {
                entry: 3,
                item: movie(),
                markers: markers(),
                liked: true,
            }),
            Reply::PlaylistFinished,
            Reply::PlexImport(crate::playlists::PlexImport {
                made: vec![crate::playlists::PlexImported {
                    name: "Heists".into(),
                    playlist_id: None,
                    entries: 18,
                }],
                unmapped: vec![crate::playlists::PlexUnmapped {
                    playlist: "Heists".into(),
                    title: Some("A trailer".into()),
                    reason: "a clip is not something Spoolframe plays".into(),
                }],
                notes: vec!["Cabin skipped".into()],
            }),
            Reply::PlaylistInfo(summary()),
            Reply::PlaylistEdited { revision: 8 },
        ] {
            round_trip(Response::Ok(reply));
        }
        for error in [
            Error::PlaylistReadOnly,
            Error::KindNotAllowed {
                kind: ItemKind::Movie,
            },
            Error::PlaylistConflict { revision: 9 },
        ] {
            round_trip(Response::Err(error));
        }
        for event in [
            Event::PlaylistChanged {
                id: "x".into(),
                revision: 1,
            },
            Event::PlaylistGone { id: "x".into() },
            Event::PinsChanged,
        ] {
            round_trip(event);
        }
    }

    #[test]
    fn reads_are_retried_and_changes_are_not() {
        use crate::policy::retry_safe;
        assert!(retry_safe(&Request::ListPlaylists));
        assert!(retry_safe(&Request::GetPlaylist {
            id: "x".into(),
            offset: 0,
            limit: 1,
        }));
        assert!(!retry_safe(&Request::EditPlaylist {
            id: "x".into(),
            revision: 0,
            edits: Vec::new(),
        }));
        assert!(!retry_safe(&Request::PinPlaylist {
            id: "x".into(),
            position: 0,
        }));
        // Liking twice is liking once.
        assert!(retry_safe(&Request::Like { items: Vec::new() }));
        assert!(retry_safe(&Request::Unlike { items: Vec::new() }));
    }

    /// `spoolframe:last-viewed`: a playback state carries when it was
    /// last viewed, and leaves the key out when it does not say.
    #[test]
    fn a_last_view_round_trips_and_is_left_out_when_unknown() {
        let mut seen = movie();
        if let spoolframe_types::Item::Movie(m) = &mut seen {
            m.playback.last_viewed_at = Some(1_791_000_000);
        }
        let json = serde_json::to_value(&seen).unwrap();
        assert_eq!(json["playback"]["last_viewed_at"], 1_791_000_000);
        round_trip(Response::Ok(Reply::Home(crate::HomeScreen {
            continue_watching: vec![seen],
            next_up: Vec::new(),
            recently_added: Vec::new(),
        })));
        let json = serde_json::to_value(movie()).unwrap();
        assert!(json["playback"].get("last_viewed_at").is_none(), "{json}");
        let read: spoolframe_types::Item = serde_json::from_value(json).unwrap();
        assert_eq!(read, movie());
    }

    /// PX-43: a built-in playlist says which it is, an album says how
    /// much of it is liked and which of its tracks, and an entry or the
    /// one playing has its heart.
    #[test]
    fn likes_and_builtins_round_trip() {
        let album: MediaId = "plex:album:31".parse().unwrap();
        let track: MediaId = "plex:track:32".parse().unwrap();
        let liked = PlaylistSummary {
            name: "Liked".into(),
            builtin: Some(crate::playlists::Builtin::Liked),
            ..summary()
        };
        round_trip(Response::Ok(Reply::PlaylistInfo(liked)));
        round_trip(Response::Ok(Reply::Playlists {
            playlists: vec![PlaylistSummary {
                builtin: Some(crate::playlists::Builtin::All),
                ..summary()
            }],
            pin_notes: Vec::new(),
        }));
        let item = Reply::Item {
            item: movie(),
            children: Vec::new(),
            markers: Vec::new(),
            playlists: Some(Vec::new()),
            liked: Some(crate::playlists::Liked::Some),
            liked_children: Some(vec![track.clone()]),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["liked"], "some");
        assert_eq!(json["liked_children"][0], "plex:track:32");
        round_trip(Response::Ok(item));
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request: Request::Like {
                items: vec![album, track],
            },
        });
    }

    /// `spoolframe:playlist-sharing`: whom a playlist can be shared
    /// with, a page at a time; and the names of who it is shared with
    /// and of its owner, left out when not given.
    #[test]
    fn share_candidates_and_names_round_trip() {
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request: Request::ListShareCandidates {
                id: "x".into(),
                query: "ali".into(),
                offset: 0,
                limit: 50,
            },
        });
        assert!(crate::policy::retry_safe(&Request::ListShareCandidates {
            id: "x".into(),
            query: String::new(),
            offset: 0,
            limit: 50,
        }));
        let page = Reply::ShareCandidates(ShareCandidatePage {
            candidates: vec![
                ShareCandidate {
                    user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4d".into(),
                    display_name: "Alice".into(),
                    username: Some("alice".into()),
                    kind: UserKind::Local,
                },
                ShareCandidate {
                    user_id: format!("portable:{}", "ab".repeat(32)),
                    display_name: "Bob".into(),
                    username: None,
                    kind: UserKind::Portable,
                },
            ],
            offset: 0,
            total: 2,
        });
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["type"], "share_candidates");
        assert_eq!(json["candidates"][0]["kind"], "local");
        assert!(json["candidates"][1].get("username").is_none(), "{json}");
        round_trip(Response::Ok(page));
        let named = PlaylistSummary {
            visibility: Visibility::Shared {
                with: vec![PlaylistShare {
                    display_name: Some("Alice".into()),
                    kind: Some(UserKind::Local),
                    ..PlaylistShare::new("local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4d", true)
                }],
            },
            owner_name: Some("Carol".into()),
            ..summary()
        };
        let json = serde_json::to_value(&named).unwrap();
        assert_eq!(json["owner_name"], "Carol");
        assert_eq!(json["visibility"]["with"][0]["display_name"], "Alice");
        assert_eq!(json["visibility"]["with"][0]["kind"], "local");
        round_trip(Response::Ok(Reply::PlaylistInfo(named)));
        let json = serde_json::to_value(summary()).unwrap();
        assert!(json.get("owner_name").is_none(), "{json}");
        // An item's reference to a playlist names its owner too, and
        // leaves the name out when it has none.
        let held = crate::playlists::PlaylistRef {
            id: "x".into(),
            name: "Heists".into(),
            owner_kind: PlaylistOwnerKind::SharedWithMe,
            builtin: None,
            owner_name: Some("Carol".into()),
            artwork: None,
        };
        assert_eq!(serde_json::to_value(&held).unwrap()["owner_name"], "Carol");
        round_trip(held.clone());
        let unnamed = crate::playlists::PlaylistRef {
            owner_name: None,
            ..held
        };
        let json = serde_json::to_value(&unnamed).unwrap();
        assert!(json.get("owner_name").is_none(), "{json}");
        round_trip(unnamed);
        assert!(
            json["visibility"]["with"][0].get("display_name").is_none(),
            "{json}"
        );
    }

    /// A client without the feature never gets `playlists`; one with it
    /// reads an older server's `Item`, which has none, as `None`.
    #[test]
    fn an_item_s_playlists_are_left_out_when_not_given() {
        let without = Reply::Item {
            item: movie(),
            children: Vec::new(),
            markers: Vec::new(),
            playlists: None,
            liked: None,
            liked_children: None,
        };
        let json = serde_json::to_value(&without).unwrap();
        assert!(json.get("playlists").is_none(), "{json}");
        let read: Reply = serde_json::from_value(json).unwrap();
        assert_eq!(read, without);
    }

    #[test]
    fn a_hello_or_a_session_without_features_reads_as_none() {
        let mut older = hello();
        older.features = Vec::new();
        let json = serde_json::to_value(&older).unwrap();
        assert!(json.get("features").is_none(), "{json}");
        let read: Hello = serde_json::from_value(json).unwrap();
        assert!(read.features.is_empty());
        let logged_in: Reply = serde_json::from_value(serde_json::json!({
            "type": "logged_in",
            "session": "s",
            "remembered": false,
        }))
        .unwrap();
        assert!(matches!(logged_in, Reply::LoggedIn { features, .. } if features.is_empty()));
    }

    #[test]
    fn a_hello_keeps_a_bounded_list_of_features() {
        let mut long = hello();
        long.features = vec!["x".repeat(1_000); 100];
        let kept = long.within_bounds();
        assert_eq!(kept.features.len(), crate::features::MAX_LISTED);
        assert_eq!(kept.features[0].chars().count(), crate::MAX_HELLO_TEXT);
    }

    #[test]
    fn playlist_parts_from_a_newer_peer_decode_as_unknown() {
        let visibility: Visibility =
            serde_json::from_value(serde_json::json!({"type": "friends_of_friends"})).unwrap();
        assert_eq!(visibility, Visibility::Unknown);
        let kind: PlaylistOwnerKind = serde_json::from_value(serde_json::json!("family")).unwrap();
        assert_eq!(kind, PlaylistOwnerKind::Unknown);
    }
}

/// PX-52, `spoolframe:play-history`: Recently played asks for a page,
/// of some kinds or all, and an item's playback state carries its play
/// count and last play, each left out when there is none, so an older
/// peer reads what it did.
mod play_history {
    use super::*;
    use crate::playlists::Kinds;

    #[test]
    fn recently_played_round_trips_and_is_safe_to_retry() {
        let music = Request::RecentlyPlayed {
            kinds: Some(Kinds {
                tracks: true,
                ..Kinds::default()
            }),
            offset: 20,
            limit: 20,
        };
        let json = serde_json::to_value(&music).unwrap();
        assert_eq!(json["type"], "recently_played");
        assert_eq!(json["kinds"]["tracks"], true);
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request: music.clone(),
        });
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request: Request::RecentlyPlayed {
                kinds: None,
                offset: 0,
                limit: 50,
            },
        });
        assert!(crate::policy::retry_safe(&music));
    }

    #[test]
    fn a_play_count_round_trips_and_is_left_out_when_none() {
        let mut played = movie();
        if let Item::Movie(m) = &mut played {
            m.playback.play_count = 2;
            m.playback.last_played_at = Some(1_791_000_000);
        }
        let json = serde_json::to_value(&played).unwrap();
        assert_eq!(json["playback"]["play_count"], 2);
        assert_eq!(json["playback"]["last_played_at"], 1_791_000_000);
        round_trip(Response::Ok(Reply::Items {
            items: vec![played],
            total: 1,
        }));
        let json = serde_json::to_value(movie()).unwrap();
        assert!(json["playback"].get("play_count").is_none(), "{json}");
        assert!(json["playback"].get("last_played_at").is_none(), "{json}");
        let read: Item = serde_json::from_value(json).unwrap();
        assert_eq!(read, movie());
    }
}

/// `spoolframe:subtitle-sync` (PX-03): the three requests, their reply and
/// the event round-trip in CBOR; each is retried safely and may probe.
#[test]
fn subtitle_timing_round_trips() {
    let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
    let timing = spoolframe_types::SubtitleTiming {
        sync: spoolframe_types::SubtitleSync::Uncertain { score: 12 },
        offset_ms: -1_500,
        offset_by: Some("Alice".into()),
        offset_at: Some(1_760_000_000),
    };
    for request in [
        Request::SyncSubtitle {
            id: id.clone(),
            track: 2,
        },
        Request::UseOriginalSubtitleTiming {
            id: id.clone(),
            track: 2,
        },
        Request::SetSubtitleOffset {
            id: id.clone(),
            track: 2,
            offset_ms: -1_500,
        },
    ] {
        assert!(crate::policy::retry_safe(&request));
        assert_eq!(
            crate::policy::server_wait(&request),
            crate::policy::wait::PROBE
        );
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request,
        });
    }
    round_trip(Response::Ok(Reply::SubtitleTiming(timing.clone())));
    round_trip(Event::SubtitleTimingChanged {
        id,
        track: 2,
        timing,
    });
}

/// `spoolframe:subtitle-search` (PX-02): the three requests, their replies
/// and the two errors round-trip in CBOR; each is retried safely, and the
/// two that ask the site wait for it.
#[test]
fn subtitle_search_round_trips() {
    use spoolframe_types::{
        QuotaOf, SubtitleDownloaded, SubtitleOrigin, SubtitleResult, SubtitleRole, SubtitleSearch,
        SubtitleSourceState, SubtitleSourceStatus, SubtitleTrack,
    };
    let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
    for (request, wait) in [
        (
            Request::SearchSubtitles {
                id: id.clone(),
                language: Some("pt-BR".into()),
            },
            crate::policy::wait::SUBTITLE_SITE,
        ),
        (
            Request::SearchSubtitles {
                id: id.clone(),
                language: None,
            },
            crate::policy::wait::SUBTITLE_SITE,
        ),
        (
            Request::DownloadSubtitle {
                id: id.clone(),
                result: "subdl:3172974-3188291".into(),
            },
            crate::policy::wait::SUBTITLE_SITE,
        ),
        (
            Request::RemoveSubtitleDownload {
                id: id.clone(),
                download: "d1".into(),
            },
            Duration::ZERO,
        ),
    ] {
        assert!(crate::policy::retry_safe(&request));
        assert_eq!(crate::policy::server_wait(&request), wait);
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request,
        });
    }
    round_trip(Response::Ok(Reply::SubtitleResults(SubtitleSearch {
        languages: vec!["en".into()],
        results: vec![SubtitleResult {
            id: "opensubtitles:1234567".into(),
            source: "opensubtitles".into(),
            release: "Film.2019.1080p.BluRay".into(),
            language: "en".into(),
            format: Some("srt".into()),
            fps_milli: Some(23_976),
            downloads: 5_000,
            rating: Some(87),
            hearing_impaired: true,
            origin: SubtitleOrigin::Ai,
            hash_match: true,
            rejected: false,
            downloaded: false,
        }],
        sources: vec![
            SubtitleSourceStatus {
                source: "opensubtitles".into(),
                state: SubtitleSourceState::Found { found: 1 },
            },
            SubtitleSourceStatus {
                source: "subdl".into(),
                state: SubtitleSourceState::QuotaExhausted { resets_at: None },
            },
            SubtitleSourceStatus {
                source: "subdl".into(),
                state: SubtitleSourceState::RateLimited {
                    retry_after_ms: 1_000,
                },
            },
            SubtitleSourceStatus {
                source: "subdl".into(),
                state: SubtitleSourceState::Failed {
                    message: "SubDL cannot be reached".into(),
                },
            },
        ],
    })));
    // A site's state this build does not know reads as Unknown.
    let later: SubtitleSourceStatus = serde_json::from_value(serde_json::json!({
        "source": "subdl",
        "state": "paused",
        "until": 5,
    }))
    .unwrap();
    assert_eq!(later.state, SubtitleSourceState::Unknown);
    let found = serde_json::to_value(SubtitleSourceStatus {
        source: "subdl".into(),
        state: SubtitleSourceState::Found { found: 12 },
    })
    .unwrap();
    assert_eq!(
        found,
        serde_json::json!({ "source": "subdl", "state": "found", "found": 12 })
    );
    round_trip(Response::Ok(Reply::SubtitleDownloaded(
        SubtitleDownloaded {
            track: 3,
            subtitle: SubtitleTrack {
                language: Some("eng".into()),
                title: Some("English (OpenSubtitles)".into()),
                forced: false,
                default: false,
                external: true,
                role: SubtitleRole::Full,
                timing: None,
                download: None,
            },
            account_remaining: Some(19),
            user_remaining: None,
        },
    )));
    round_trip(Response::Err(Error::QuotaExhausted {
        whose: QuotaOf::Account,
        resets_at: Some(1_760_000_000),
        site: Some("subdl".into()),
    }));
    round_trip(Response::Err(Error::QuotaExhausted {
        whose: QuotaOf::User,
        resets_at: None,
        site: None,
    }));
    round_trip(Response::Err(Error::NoSubtitleSource));
}

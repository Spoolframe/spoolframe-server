//! The wire format: every message round-trips, and what an older peer
//! does not know is skipped or named, never a decode failure.

use std::fmt::Debug;
use std::time::Duration;

use crate::TrackPreferencesInfo;
use crate::codec::{self, CodecError, MAX_FRAME};
use crate::{
    AudioAction, BitmapSubtitles, ChosenTracks, ConvertedAudio, ConvertedSubtitle, ConvertedVideo,
    SubtitleAction, TrackReason,
};
use crate::{
    AuthMethod, ConfirmPassword, Error, Event, Hello, LoginPassword, PROTOCOL_VERSION, PinProof,
    Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId, SessionInfo, UserSummary,
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
        summary: None,
        content_rating: None,
        duration: Some(Duration::from_secs(7020)),
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
        },
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: None,
            start_ms: None,
            bitmap: BitmapSubtitles::Auto,
            quality: Default::default(),
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
        },
        Reply::Search(search_results()),
        Reply::Tracks(MediaTracks::default()),
        Reply::WhoAmI(UserSummary {
            user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            role: spoolframe_types::Role::Consumer,
            locked: true,
            pin: Some(crate::PinState { digits: Some(6) }),
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
    });
}

fn server_addresses_event() -> Event {
    Event::ServerAddresses {
        addrs: vec![
            "10.10.0.2:4433".parse().unwrap(),
            "[2001:db8::1]:4433".parse().unwrap(),
        ],
        relays: vec!["https://relay.example.net./".into()],
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
        })
    );
    let bad: Result<Event, _> = serde_json::from_value(serde_json::json!({
        "type": "server_addresses", "addrs": ["nimble.local:4433"], "relays": []
    }));
    assert!(bad.is_err(), "{bad:?}");
}

/// Neither list may be left out: an empty one is sent as empty.
#[test]
fn the_server_addresses_event_missing_a_field_is_refused() {
    assert_each_required(&server_addresses_event(), &["addrs", "relays"]);
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
        },
        &["locked"],
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

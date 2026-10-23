//! The dialling server's side of `spoolframe/federation/1`: redeeming
//! another server's pairing invite, telling a paired server it is
//! unpaired, and replicating what a paired server shares (P8-02).

use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, VarInt};
use iroh::{Endpoint, EndpointAddr};
use spoolframe_protocol::FEDERATION_ALPN;
use spoolframe_protocol::codec::{MAX_FRAME, read_frame_within, read_reply, write_frame};
use spoolframe_protocol::federation::{
    ArtworkFetch, AssertUser, AssertionRefused, MAX_ARTWORK_BYTES, MAX_REQUEST, Paired,
    PairingRefused, PeerHello, PeerInfo, PeerReply, PeerRequest, RedeemPairing, SharedLibrary,
    SyncLibrary, SyncMode, SyncPage, TokenGranted, Unpaired, libraries_within_bounds,
};

/// How long dialling may take: one attempt, over every hint given.
const DIAL_WAIT: Duration = Duration::from_secs(15);

/// How long a reply may take.
const REPLY_WAIT: Duration = Duration::from_secs(15);

/// How long a sync's first page may take: the other server walks the
/// whole library before it answers.
const FIRST_PAGE_WAIT: Duration = Duration::from_secs(600);

/// How long each page after the first may take.
const PAGE_WAIT: Duration = Duration::from_secs(60);

/// How long an image may take, the whole of it.
const ARTWORK_WAIT: Duration = Duration::from_secs(60);

/// Why talking to another server did not go as asked.
#[derive(Debug, thiserror::Error)]
pub enum DialError {
    /// It could not be reached.
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    /// The connection or a stream broke, or a reply did not come in time
    /// or did not read.
    #[error("the server stopped answering: {0}")]
    Broken(String),
    /// It refused.
    #[error("the server refused: {0}")]
    Refused(PairingRefused),
    /// It answered with something that is not an answer to what was
    /// asked.
    #[error("the server answered out of turn")]
    OutOfTurn,
    /// What was asked of it is not shared with this server (any more).
    #[error("the server does not share that with this one")]
    NotShared,
    /// Shared, but not there now.
    #[error("the server has no such thing now")]
    NotFound,
    /// It cannot answer now.
    #[error("the server cannot answer now")]
    Unavailable,
    /// It minted no token for an assertion (P8-03).
    #[error("the server refused the assertion: {0}")]
    NotGranted(AssertionRefused),
}

/// The pages of a sync's answer, read one at a time from its stream.
#[derive(Debug)]
pub struct Pages {
    recv: RecvStream,
    /// The first page's mode and cursor: every page must say the same.
    first: Option<(SyncMode, String)>,
    done: bool,
}

impl Pages {
    /// The next page, cut to the bounds; `None` once the last was read.
    pub async fn next(&mut self) -> Result<Option<SyncPage>, DialError> {
        if self.done {
            return Ok(None);
        }
        let wait = if self.first.is_none() {
            FIRST_PAGE_WAIT
        } else {
            PAGE_WAIT
        };
        let reply = tokio::time::timeout(wait, read_frame_within(&mut self.recv, MAX_FRAME))
            .await
            .map_err(|_| DialError::Broken("no page in time".into()))?
            .map_err(|e| DialError::Broken(e.to_string()))?;
        let page = match reply {
            PeerReply::Page(page) => page.within_bounds(),
            PeerReply::NotShared if self.first.is_none() => return Err(DialError::NotShared),
            PeerReply::Unavailable if self.first.is_none() => return Err(DialError::Unavailable),
            _ => return Err(DialError::OutOfTurn),
        };
        let said = (page.mode, page.cursor.clone());
        match &self.first {
            None => self.first = Some(said),
            Some(first) if *first != said => return Err(DialError::OutOfTurn),
            Some(_) => {}
        }
        self.done = !page.more;
        Ok(Some(page))
    }
}

/// A connection to another server, its `Hello` answered.
#[derive(Debug)]
pub struct Dialed {
    connection: Connection,
}

impl Dialed {
    /// Dial `addr` from `endpoint` (the server's own: its key is who it
    /// is to the other) and say `hello`: the connection and what the
    /// other said of itself and of this server.
    pub async fn connect(
        endpoint: &Endpoint,
        addr: EndpointAddr,
        hello: PeerHello,
    ) -> Result<(Self, PeerInfo), DialError> {
        let connection = tokio::time::timeout(DIAL_WAIT, endpoint.connect(addr, FEDERATION_ALPN))
            .await
            .map_err(|_| DialError::Unreachable("no answer in time".into()))?
            .map_err(|e| DialError::Unreachable(e.to_string()))?;
        let dialed = Self { connection };
        match dialed.ask(&PeerRequest::Hello(hello)).await? {
            PeerReply::Info(info) => Ok((dialed, info)),
            PeerReply::Refused { reason } => {
                dialed.close();
                Err(DialError::Refused(reason))
            }
            _ => {
                dialed.close();
                Err(DialError::OutOfTurn)
            }
        }
    }

    /// Redeem the other server's pairing invite.
    pub async fn redeem(&self, redeem: RedeemPairing) -> Result<Paired, DialError> {
        match self.ask(&PeerRequest::RedeemPairing(redeem)).await? {
            PeerReply::Paired(paired) => Ok(paired),
            PeerReply::Refused { reason } => Err(DialError::Refused(reason)),
            _ => Err(DialError::OutOfTurn),
        }
    }

    /// Tell the other server this one unpaired.
    pub async fn unpaired(&self) -> Result<(), DialError> {
        match self.ask(&PeerRequest::Unpaired(Unpaired {})).await? {
            PeerReply::Done => Ok(()),
            PeerReply::Refused { reason } => Err(DialError::Refused(reason)),
            _ => Err(DialError::OutOfTurn),
        }
    }

    /// The libraries the other server shares with this one now (P8-02),
    /// cut to the bounds.
    pub async fn libraries(&self) -> Result<Vec<SharedLibrary>, DialError> {
        match self.ask_within(&PeerRequest::Libraries, MAX_FRAME).await? {
            PeerReply::Libraries { libraries } => Ok(libraries_within_bounds(libraries)),
            PeerReply::Unavailable => Err(DialError::Unavailable),
            _ => Err(DialError::OutOfTurn),
        }
    }

    /// Ask for a shared library's changes (P8-02): the pages of the
    /// answer, to read in turn. Nothing of it is to be kept until the last
    /// page is read.
    pub async fn sync(&self, sync: SyncLibrary) -> Result<Pages, DialError> {
        let recv = self.send(&PeerRequest::Sync(sync)).await?;
        Ok(Pages {
            recv,
            first: None,
            done: false,
        })
    }

    /// Image `id` of an item the other server shares with this one
    /// (P8-02): its media type and bytes.
    pub async fn artwork(&self, id: &str) -> Result<(String, Vec<u8>), DialError> {
        let mut recv = self
            .send(&PeerRequest::Artwork(ArtworkFetch { id: id.to_owned() }))
            .await?;
        let reply = tokio::time::timeout(ARTWORK_WAIT, read_reply(&mut recv))
            .await
            .map_err(|_| DialError::Broken("no image in time".into()))?
            .map_err(|e| DialError::Broken(e.to_string()))?;
        match reply {
            PeerReply::Artwork {
                content_type,
                bytes,
            } if bytes.len() <= MAX_ARTWORK_BYTES => Ok((content_type, bytes)),
            PeerReply::NotShared => Err(DialError::NotShared),
            PeerReply::NotFound => Err(DialError::NotFound),
            PeerReply::Unavailable => Err(DialError::Unavailable),
            _ => Err(DialError::OutOfTurn),
        }
    }

    /// Vouch for one of this server's users to the other, which shares
    /// the item they want (P8-03): its token.
    pub async fn assert_user(&self, assertion: AssertUser) -> Result<TokenGranted, DialError> {
        match self
            .ask_within(&PeerRequest::AssertUser(assertion), MAX_FRAME)
            .await?
        {
            PeerReply::Token(granted) => Ok(granted),
            PeerReply::AssertionRefused { reason } => Err(DialError::NotGranted(reason)),
            _ => Err(DialError::OutOfTurn),
        }
    }

    /// The connection, to wait for its close.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// A connection that has said nothing yet.
    #[cfg(test)]
    pub(crate) fn for_tests(connection: Connection) -> Self {
        Self { connection }
    }

    /// Done with it.
    pub fn close(self) {
        self.connection.close(VarInt::from_u32(0), b"done");
    }

    /// `request` on a stream of its own, and its reply: at most
    /// [`MAX_REQUEST`], as a hello, a redemption or an unpairing answers,
    /// the more so from a server not paired yet.
    pub(crate) async fn ask(&self, request: &PeerRequest) -> Result<PeerReply, DialError> {
        self.ask_within(request, MAX_REQUEST).await
    }

    /// [`Dialed::ask`], its reply at most `bound`: a whole frame for the
    /// answers that list (`Libraries`).
    async fn ask_within(&self, request: &PeerRequest, bound: u32) -> Result<PeerReply, DialError> {
        let mut recv = self.send(request).await?;
        tokio::time::timeout(REPLY_WAIT, read_frame_within(&mut recv, bound))
            .await
            .map_err(|_| DialError::Broken("no reply in time".into()))?
            .map_err(|e| DialError::Broken(e.to_string()))
    }

    /// `request` on a stream of its own: where its reply comes.
    async fn send(&self, request: &PeerRequest) -> Result<RecvStream, DialError> {
        let broken = |e: String| DialError::Broken(e);
        let (mut send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| broken(e.to_string()))?;
        write_frame(&mut send, request)
            .await
            .map_err(|e| broken(e.to_string()))?;
        send.finish().map_err(|e| broken(e.to_string()))?;
        Ok(recv)
    }
}

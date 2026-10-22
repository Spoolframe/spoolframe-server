//! The dialling server's side of `spoolframe/federation/1`: redeeming
//! another server's pairing invite, and telling a paired server it is
//! unpaired.

use std::time::Duration;

use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr};
use spoolframe_protocol::FEDERATION_ALPN;
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::federation::{
    MAX_REQUEST, Paired, PairingRefused, PeerHello, PeerInfo, PeerReply, PeerRequest,
    RedeemPairing, Unpaired,
};

/// How long dialling may take: one attempt, over every hint given.
const DIAL_WAIT: Duration = Duration::from_secs(15);

/// How long a reply may take.
const REPLY_WAIT: Duration = Duration::from_secs(15);

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

    /// `request` on a stream of its own, and its reply.
    pub(crate) async fn ask(&self, request: &PeerRequest) -> Result<PeerReply, DialError> {
        let broken = |e: String| DialError::Broken(e);
        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| broken(e.to_string()))?;
        write_frame(&mut send, request)
            .await
            .map_err(|e| broken(e.to_string()))?;
        send.finish().map_err(|e| broken(e.to_string()))?;
        tokio::time::timeout(REPLY_WAIT, read_frame_within(&mut recv, MAX_REQUEST))
            .await
            .map_err(|_| broken("no reply in time".into()))?
            .map_err(|e| broken(e.to_string()))
    }
}

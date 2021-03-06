//! Implementation of a Lighthouse's peer management system.

pub use self::peerdb::*;
use crate::discovery::{Discovery, DiscoveryEvent};
use crate::rpc::{MetaData, Protocol, RPCError, RPCResponseErrorCode};
use crate::{error, metrics};
use crate::{Enr, EnrExt, NetworkConfig, NetworkGlobals, PeerId};
use futures::prelude::*;
use futures::Stream;
use hashset_delay::HashSetDelay;
use libp2p::core::multiaddr::Protocol as MProtocol;
use libp2p::identify::IdentifyInfo;
use slog::{crit, debug, error};
use smallvec::SmallVec;
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use types::{EthSpec, SubnetId};

pub use libp2p::core::{identity::Keypair, Multiaddr};

pub mod client;
mod peer_info;
mod peer_sync_status;
mod peerdb;

pub use peer_info::{PeerConnectionStatus::*, PeerInfo};
pub use peer_sync_status::{PeerSyncStatus, SyncInfo};
/// The minimum reputation before a peer is disconnected.
// Most likely this needs tweaking.
const _MIN_REP_BEFORE_BAN: Rep = 10;
/// The time in seconds between re-status's peers.
const STATUS_INTERVAL: u64 = 300;
/// The time in seconds between PING events. We do not send a ping if the other peer as PING'd us within
/// this time frame (Seconds)
const PING_INTERVAL: u64 = 30;

/// The heartbeat performs regular updates such as updating reputations and performing discovery
/// requests. This defines the interval in seconds.  
const HEARTBEAT_INTERVAL: u64 = 30;

/// The main struct that handles peer's reputation and connection status.
pub struct PeerManager<TSpec: EthSpec> {
    /// Storage of network globals to access the `PeerDB`.
    network_globals: Arc<NetworkGlobals<TSpec>>,
    /// A queue of events that the `PeerManager` is waiting to produce.
    events: SmallVec<[PeerManagerEvent; 16]>,
    /// A collection of peers awaiting to be Ping'd.
    ping_peers: HashSetDelay<PeerId>,
    /// A collection of peers awaiting to be Status'd.
    status_peers: HashSetDelay<PeerId>,
    /// The target number of peers we would like to connect to.
    target_peers: usize,
    /// The discovery service.
    discovery: Discovery<TSpec>,
    /// The heartbeat interval to perform routine maintenance.
    heartbeat: tokio::time::Interval,
    /// The logger associated with the `PeerManager`.
    log: slog::Logger,
}

/// A collection of actions a peer can perform which will adjust its reputation.
/// Each variant has an associated reputation change.
// To easily assess the behaviour of reputation changes the number of variants should stay low, and
// somewhat generic.
pub enum PeerAction {
    /// We should not communicate more with this peer.
    /// This action will cause the peer to get banned.
    Fatal,
    /// An error occurred with this peer but it is not necessarily malicious.
    /// We have high tolerance for this actions: several occurrences are needed for a peer to get
    /// kicked.
    /// NOTE: ~15 occurrences will get the peer banned
    HighToleranceError,
    /// An error occurred with this peer but it is not necessarily malicious.
    /// We have high tolerance for this actions: several occurrences are needed for a peer to get
    /// kicked.
    /// NOTE: ~10 occurrences will get the peer banned
    MidToleranceError,
    /// This peer's action is not malicious but will not be tolerated. A few occurrences will cause
    /// the peer to get kicked.
    /// NOTE: ~5 occurrences will get the peer banned
    LowToleranceError,
    /// Received an expected message.
    _ValidMessage,
}

impl PeerAction {
    fn rep_change(&self) -> RepChange {
        match self {
            PeerAction::Fatal => RepChange::worst(),
            PeerAction::LowToleranceError => RepChange::bad(60),
            PeerAction::MidToleranceError => RepChange::bad(25),
            PeerAction::HighToleranceError => RepChange::bad(15),
            PeerAction::_ValidMessage => RepChange::good(20),
        }
    }
}

/// The events that the `PeerManager` outputs (requests).
pub enum PeerManagerEvent {
    /// Dial a PeerId.
    Dial(PeerId),
    /// Inform libp2p that our external socket addr has been updated.
    SocketUpdated(Multiaddr),
    /// Sends a STATUS to a peer.
    Status(PeerId),
    /// Sends a PING to a peer.
    Ping(PeerId),
    /// Request METADATA from a peer.
    MetaData(PeerId),
    /// The peer should be disconnected.
    DisconnectPeer(PeerId),
}

impl<TSpec: EthSpec> PeerManager<TSpec> {
    // NOTE: Must be run inside a tokio executor.
    pub fn new(
        local_key: &Keypair,
        config: &NetworkConfig,
        network_globals: Arc<NetworkGlobals<TSpec>>,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        // start the discovery service
        let mut discovery = Discovery::new(local_key, config, network_globals.clone(), log)?;

        // start searching for peers
        discovery.discover_peers();

        let heartbeat = tokio::time::interval(tokio::time::Duration::from_secs(HEARTBEAT_INTERVAL));

        Ok(PeerManager {
            network_globals,
            events: SmallVec::new(),
            ping_peers: HashSetDelay::new(Duration::from_secs(PING_INTERVAL)),
            status_peers: HashSetDelay::new(Duration::from_secs(STATUS_INTERVAL)),
            target_peers: config.max_peers, //TODO: Add support for target peers and max peers
            discovery,
            heartbeat,
            log: log.clone(),
        })
    }

    /* Public accessible functions */

    /* Discovery Requests */

    /// Provides a reference to the underlying discovery service.
    pub fn discovery(&self) -> &Discovery<TSpec> {
        &self.discovery
    }

    /// Provides a mutable reference to the underlying discovery service.
    pub fn discovery_mut(&mut self) -> &mut Discovery<TSpec> {
        &mut self.discovery
    }

    /// A request to find peers on a given subnet.
    pub fn discover_subnet_peers(&mut self, subnet_id: SubnetId, min_ttl: Option<Instant>) {
        // Extend the time to maintain peers if required.
        if let Some(min_ttl) = min_ttl {
            self.network_globals
                .peers
                .write()
                .extend_peers_on_subnet(subnet_id, min_ttl);
        }

        // request the subnet query from discovery
        self.discovery.discover_subnet_peers(subnet_id, min_ttl);
    }

    /// A STATUS message has been received from a peer. This resets the status timer.
    pub fn peer_statusd(&mut self, peer_id: &PeerId) {
        self.status_peers.insert(peer_id.clone());
    }

    /// Updates the state of the peer as disconnected.
    pub fn notify_disconnect(&mut self, peer_id: &PeerId) {
        //self.update_reputations();
        self.network_globals.peers.write().disconnect(peer_id);

        // remove the ping and status timer for the peer
        self.ping_peers.remove(peer_id);
        self.status_peers.remove(peer_id);
        metrics::inc_counter(&metrics::PEER_DISCONNECT_EVENT_COUNT);
        metrics::set_gauge(
            &metrics::PEERS_CONNECTED,
            self.network_globals.connected_peers() as i64,
        );
    }

    /// Sets a peer as connected as long as their reputation allows it
    /// Informs if the peer was accepted
    pub fn connect_ingoing(&mut self, peer_id: &PeerId) -> bool {
        self.connect_peer(peer_id, ConnectingType::IngoingConnected)
    }

    /// Sets a peer as connected as long as their reputation allows it
    /// Informs if the peer was accepted
    pub fn connect_outgoing(&mut self, peer_id: &PeerId) -> bool {
        self.connect_peer(peer_id, ConnectingType::OutgoingConnected)
    }

    /// Updates the database informing that a peer is being dialed.
    pub fn dialing_peer(&mut self, peer_id: &PeerId) -> bool {
        self.connect_peer(peer_id, ConnectingType::Dialing)
    }

    /// Updates the database informing that a peer is being disconnected.
    pub fn _disconnecting_peer(&mut self, _peer_id: &PeerId) -> bool {
        // TODO: implement
        true
    }

    /// Reports a peer for some action.
    ///
    /// If the peer doesn't exist, log a warning and insert defaults.
    pub fn report_peer(&mut self, peer_id: &PeerId, action: PeerAction) {
        //TODO: Check these. There are double disconnects for example
        // self.update_reputations();
        self.network_globals
            .peers
            .write()
            .add_reputation(peer_id, action.rep_change());
        // self.update_reputations();
    }

    /// Updates `PeerInfo` with `identify` information.
    pub fn identify(&mut self, peer_id: &PeerId, info: &IdentifyInfo) {
        if let Some(peer_info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            peer_info.client = client::Client::from_identify_info(info);
            peer_info.listening_addresses = info.listen_addrs.clone();
        } else {
            crit!(self.log, "Received an Identify response from an unknown peer"; "peer_id" => peer_id.to_string());
        }
    }

    pub fn handle_rpc_error(&mut self, peer_id: &PeerId, protocol: Protocol, err: &RPCError) {
        let client = self.network_globals.client(peer_id);
        debug!(self.log, "RPCError"; "protocol" => protocol.to_string(), "err" => err.to_string(), "client" => client.to_string());

        // Map this error to a `PeerAction` (if any)
        let peer_action = match err {
            RPCError::IncompleteStream => {
                // They closed early, this could mean poor connection
                PeerAction::MidToleranceError
            }
            RPCError::InternalError(_) | RPCError::HandlerRejected => {
                // Our fault. Do nothing
                return;
            }
            RPCError::InvalidData => {
                // Peer is not complying with the protocol. This is considered a malicious action
                PeerAction::Fatal
            }
            RPCError::IoError(_e) => {
                // this could their fault or ours, so we tolerate this
                PeerAction::HighToleranceError
            }
            RPCError::ErrorResponse(code, _) => match code {
                RPCResponseErrorCode::Unknown => PeerAction::HighToleranceError,
                RPCResponseErrorCode::ServerError => PeerAction::MidToleranceError,
                RPCResponseErrorCode::InvalidRequest => PeerAction::LowToleranceError,
            },
            RPCError::SSZDecodeError(_) => PeerAction::Fatal,
            RPCError::UnsupportedProtocol => {
                // Not supporting a protocol shouldn't be considered a malicious action, but
                // it is an action that in some cases will make the peer unfit to continue
                // communicating.
                // TODO: To avoid punishing a peer repeatedly for not supporting a protocol, this
                // information could be stored and used to prevent sending requests for the given
                // protocol to this peer. Similarly, to avoid blacklisting a peer for a protocol
                // forever, if stored this information should expire.
                match protocol {
                    Protocol::Ping => PeerAction::Fatal,
                    Protocol::BlocksByRange => return,
                    Protocol::BlocksByRoot => return,
                    Protocol::Goodbye => return,
                    Protocol::MetaData => PeerAction::LowToleranceError,
                    Protocol::Status => PeerAction::LowToleranceError,
                }
            }
            RPCError::StreamTimeout => match protocol {
                Protocol::Ping => PeerAction::LowToleranceError,
                Protocol::BlocksByRange => PeerAction::MidToleranceError,
                Protocol::BlocksByRoot => PeerAction::MidToleranceError,
                Protocol::Goodbye => return,
                Protocol::MetaData => return,
                Protocol::Status => return,
            },
            RPCError::NegotiationTimeout => PeerAction::HighToleranceError,
        };

        self.report_peer(peer_id, peer_action);
    }

    /// A ping request has been received.
    // NOTE: The behaviour responds with a PONG automatically
    // TODO: Update last seen
    pub fn ping_request(&mut self, peer_id: &PeerId, seq: u64) {
        if let Some(peer_info) = self.network_globals.peers.read().peer_info(peer_id) {
            // received a ping
            // reset the to-ping timer for this peer
            debug!(self.log, "Received a ping request"; "peer_id" => peer_id.to_string(), "seq_no" => seq);
            self.ping_peers.insert(peer_id.clone());

            // if the sequence number is unknown send an update the meta data of the peer.
            if let Some(meta_data) = &peer_info.meta_data {
                if meta_data.seq_number < seq {
                    debug!(self.log, "Requesting new metadata from peer";
                        "peer_id" => peer_id.to_string(), "known_seq_no" => meta_data.seq_number, "ping_seq_no" => seq);
                    self.events
                        .push(PeerManagerEvent::MetaData(peer_id.clone()));
                }
            } else {
                // if we don't know the meta-data, request it
                debug!(self.log, "Requesting first metadata from peer";
                    "peer_id" => peer_id.to_string());
                self.events
                    .push(PeerManagerEvent::MetaData(peer_id.clone()));
            }
        } else {
            crit!(self.log, "Received a PING from an unknown peer";
                "peer_id" => peer_id.to_string());
        }
    }

    /// A PONG has been returned from a peer.
    // TODO: Update last seen
    pub fn pong_response(&mut self, peer_id: &PeerId, seq: u64) {
        if let Some(peer_info) = self.network_globals.peers.read().peer_info(peer_id) {
            // received a pong

            // if the sequence number is unknown send update the meta data of the peer.
            if let Some(meta_data) = &peer_info.meta_data {
                if meta_data.seq_number < seq {
                    debug!(self.log, "Requesting new metadata from peer";
                        "peer_id" => peer_id.to_string(), "known_seq_no" => meta_data.seq_number, "pong_seq_no" => seq);
                    self.events
                        .push(PeerManagerEvent::MetaData(peer_id.clone()));
                }
            } else {
                // if we don't know the meta-data, request it
                debug!(self.log, "Requesting first metadata from peer";
                    "peer_id" => peer_id.to_string());
                self.events
                    .push(PeerManagerEvent::MetaData(peer_id.clone()));
            }
        } else {
            crit!(self.log, "Received a PONG from an unknown peer"; "peer_id" => peer_id.to_string());
        }
    }

    /// Received a metadata response from a peer.
    // TODO: Update last seen
    pub fn meta_data_response(&mut self, peer_id: &PeerId, meta_data: MetaData<TSpec>) {
        if let Some(peer_info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            if let Some(known_meta_data) = &peer_info.meta_data {
                if known_meta_data.seq_number < meta_data.seq_number {
                    debug!(self.log, "Updating peer's metadata";
                        "peer_id" => peer_id.to_string(), "known_seq_no" => known_meta_data.seq_number, "new_seq_no" => meta_data.seq_number);
                    peer_info.meta_data = Some(meta_data);
                } else {
                    debug!(self.log, "Received old metadata";
                        "peer_id" => peer_id.to_string(), "known_seq_no" => known_meta_data.seq_number, "new_seq_no" => meta_data.seq_number);
                }
            } else {
                // we have no meta-data for this peer, update
                debug!(self.log, "Obtained peer's metadata";
                    "peer_id" => peer_id.to_string(), "new_seq_no" => meta_data.seq_number);
                peer_info.meta_data = Some(meta_data);
            }
        } else {
            crit!(self.log, "Received METADATA from an unknown peer";
                "peer_id" => peer_id.to_string());
        }
    }

    // Handles the libp2p request to obtain multiaddrs for peer_id's in order to dial them.
    pub fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        if let Some(enr) = self.discovery.enr_of_peer(peer_id) {
            // ENR's may have multiple Multiaddrs. The multi-addr associated with the UDP
            // port is removed, which is assumed to be associated with the discv5 protocol (and
            // therefore irrelevant for other libp2p components).
            let mut out_list = enr.multiaddr();
            out_list.retain(|addr| {
                addr.iter()
                    .find(|v| match v {
                        MProtocol::Udp(_) => true,
                        _ => false,
                    })
                    .is_none()
            });

            out_list
        } else {
            // PeerId is not known
            Vec::new()
        }
    }

    /* Internal functions */

    // The underlying discovery server has updated our external IP address. We send this up to
    // notify libp2p.
    fn socket_updated(&mut self, socket: SocketAddr) {
        // Build a multiaddr to report to libp2p
        let mut multiaddr = Multiaddr::from(socket.ip());
        // NOTE: This doesn't actually track the external TCP port. More sophisticated NAT handling
        // should handle this.
        multiaddr.push(MProtocol::Tcp(self.network_globals.listen_port_tcp()));
        self.events.push(PeerManagerEvent::SocketUpdated(multiaddr));
    }

    /// Peers that have been returned by discovery requests are dialed here if they are suitable.
    ///
    /// NOTE: By dialing `PeerId`s and not multiaddrs, libp2p requests the multiaddr associated
    /// with a new `PeerId` which involves a discovery routing table lookup. We could dial the
    /// multiaddr here, however this could relate to duplicate PeerId's etc. If the lookup
    /// proves resource constraining, we should switch to multiaddr dialling here.
    fn peers_discovered(&mut self, peers: Vec<Enr>, min_ttl: Option<Instant>) {
        for enr in peers {
            let peer_id = enr.peer_id();

            // if we need more peers, attempt a connection
            if self.network_globals.connected_or_dialing_peers() < self.target_peers
                && !self
                    .network_globals
                    .peers
                    .read()
                    .is_connected_or_dialing(&peer_id)
                && !self.network_globals.peers.read().peer_banned(&peer_id)
            {
                debug!(self.log, "Dialing discovered peer"; "peer_id"=> peer_id.to_string());
                // TODO: Update output
                // This should be updated with the peer dialing. In fact created once the peer is
                // dialed
                if let Some(min_ttl) = min_ttl {
                    self.network_globals
                        .peers
                        .write()
                        .update_min_ttl(&peer_id, min_ttl);
                }
                self.events.push(PeerManagerEvent::Dial(peer_id));
            }
        }
    }

    /// Registers a peer as connected. The `ingoing` parameter determines if the peer is being
    /// dialed or connecting to us.
    ///
    /// This is called by `connect_ingoing` and `connect_outgoing`.
    ///
    /// This informs if the peer was accepted in to the db or not.
    // TODO: Drop peers if over max_peer limit
    fn connect_peer(&mut self, peer_id: &PeerId, connection: ConnectingType) -> bool {
        // TODO: remove after timed updates
        //self.update_reputations();

        {
            let mut peerdb = self.network_globals.peers.write();
            if peerdb.connection_status(peer_id).map(|c| c.is_banned()) == Some(true) {
                // don't connect if the peer is banned
                // TODO: Handle this case. If peer is banned this shouldn't be reached. It will put
                // our connection/disconnection out of sync with libp2p
                // return false;
            }

            match connection {
                ConnectingType::Dialing => peerdb.dialing_peer(peer_id),
                ConnectingType::IngoingConnected => peerdb.connect_outgoing(peer_id),
                ConnectingType::OutgoingConnected => peerdb.connect_ingoing(peer_id),
            }
        }

        // start a ping and status timer for the peer
        self.ping_peers.insert(peer_id.clone());
        self.status_peers.insert(peer_id.clone());

        // increment prometheus metrics
        metrics::inc_counter(&metrics::PEER_CONNECT_EVENT_COUNT);
        metrics::set_gauge(
            &metrics::PEERS_CONNECTED,
            self.network_globals.connected_peers() as i64,
        );

        true
    }

    /// Notifies the peer manager that this peer is being dialed.
    pub fn _dialing_peer(&mut self, peer_id: &PeerId) {
        self.network_globals.peers.write().dialing_peer(peer_id);
    }

    /// Updates the reputation of known peers according to their connection
    /// status and the time that has passed.
    ///
    /// **Disconnected peers** get a 1rep hit every hour they stay disconnected.
    /// **Banned peers** get a 1rep gain for every hour to slowly allow them back again.
    ///
    /// A banned(disconnected) peer that gets its rep above(below) MIN_REP_BEFORE_BAN is
    /// now considered a disconnected(banned) peer.
    // TODO: Implement when reputation is added.
    fn _update_reputations(&mut self) {
        /*
        // avoid locking the peerdb too often
        // TODO: call this on a timer

        let now = Instant::now();

        // Check for peers that get banned, unbanned and that should be disconnected
        let mut ban_queue = Vec::new();
        let mut unban_queue = Vec::new();

        /* Check how long have peers been in this state and update their reputations if needed */
        let mut pdb = self.network_globals.peers.write();

        for (id, info) in pdb._peers_mut() {
            // Update reputations
            match info.connection_status {
                Connected { .. } => {
                    // Connected peers gain reputation by sending useful messages
                }
                Disconnected { since } | Banned { since } => {
                    // For disconnected peers, lower their reputation by 1 for every hour they
                    // stay disconnected. This helps us slowly forget disconnected peers.
                    // In the same way, slowly allow banned peers back again.
                    let dc_hours = now
                        .checked_duration_since(since)
                        .unwrap_or_else(|| Duration::from_secs(0))
                        .as_secs()
                        / 3600;
                    let last_dc_hours = self
                        ._last_updated
                        .checked_duration_since(since)
                        .unwrap_or_else(|| Duration::from_secs(0))
                        .as_secs()
                        / 3600;
                    if dc_hours > last_dc_hours {
                        // this should be 1 most of the time
                        let rep_dif = (dc_hours - last_dc_hours)
                            .try_into()
                            .unwrap_or(Rep::max_value());

                        info.reputation = if info.connection_status.is_banned() {
                            info.reputation.saturating_add(rep_dif)
                        } else {
                            info.reputation.saturating_sub(rep_dif)
                        };
                    }
                }
                Dialing { since } => {
                    // A peer shouldn't be dialing for more than 2 minutes
                    if since.elapsed().as_secs() > 120 {
                        warn!(self.log,"Peer has been dialing for too long"; "peer_id" => id.to_string());
                        // TODO: decide how to handle this
                    }
                }
                Unknown => {} //TODO: Handle this case
            }
            // Check if the peer gets banned or unbanned and if it should be disconnected
            if info.reputation < _MIN_REP_BEFORE_BAN && !info.connection_status.is_banned() {
                // This peer gets banned. Check if we should request disconnection
                ban_queue.push(id.clone());
            } else if info.reputation >= _MIN_REP_BEFORE_BAN && info.connection_status.is_banned() {
                // This peer gets unbanned
                unban_queue.push(id.clone());
            }
        }

        for id in ban_queue {
            pdb.ban(&id);

            self.events
                .push(PeerManagerEvent::DisconnectPeer(id.clone()));
        }

        for id in unban_queue {
            pdb.disconnect(&id);
        }

        self._last_updated = Instant::now();
        */
    }

    /// The Peer manager's heartbeat maintains the peer count and maintains peer reputations.
    ///
    /// It will request discovery queries if the peer count has not reached the desired number of
    /// peers.
    ///
    /// NOTE: Discovery will only add a new query if one isn't already queued.
    fn heartbeat(&mut self) {
        // TODO: Provide a back-off time for discovery queries. I.e Queue many initially, then only
        // perform discoveries over a larger fixed interval. Perhaps one every 6 heartbeats
        let peer_count = self.network_globals.connected_or_dialing_peers();
        if peer_count < self.target_peers {
            // If we need more peers, queue a discovery lookup.
            self.discovery.discover_peers();
        }

        // TODO: If we have too many peers, remove peers that are not required for subnet
        // validation.

        // TODO: Perform peer reputation maintenance here
    }
}

impl<TSpec: EthSpec> Stream for PeerManager<TSpec> {
    type Item = PeerManagerEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // perform the heartbeat when necessary
        while let Poll::Ready(Some(_)) = self.heartbeat.poll_next_unpin(cx) {
            self.heartbeat();
        }

        // handle any discovery events
        while let Poll::Ready(event) = self.discovery.poll(cx) {
            match event {
                DiscoveryEvent::SocketUpdated(socket_addr) => self.socket_updated(socket_addr),
                DiscoveryEvent::QueryResult(min_ttl, peers) => {
                    self.peers_discovered(*peers, min_ttl)
                }
            }
        }

        // poll the timeouts for pings and status'
        loop {
            match self.ping_peers.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(peer_id))) => {
                    self.ping_peers.insert(peer_id.clone());
                    self.events.push(PeerManagerEvent::Ping(peer_id));
                }
                Poll::Ready(Some(Err(e))) => {
                    error!(self.log, "Failed to check for peers to ping"; "error" => format!("{}",e))
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        loop {
            match self.status_peers.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(peer_id))) => {
                    self.status_peers.insert(peer_id.clone());
                    self.events.push(PeerManagerEvent::Status(peer_id))
                }
                Poll::Ready(Some(Err(e))) => {
                    error!(self.log, "Failed to check for peers to ping"; "error" => format!("{}",e))
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        if !self.events.is_empty() {
            return Poll::Ready(Some(self.events.remove(0)));
        } else {
            self.events.shrink_to_fit();
        }

        Poll::Pending
    }
}

enum ConnectingType {
    /// We are in the process of dialing this peer.
    Dialing,
    /// A peer has dialed us.
    IngoingConnected,
    /// We have successfully dialed a peer.
    OutgoingConnected,
}

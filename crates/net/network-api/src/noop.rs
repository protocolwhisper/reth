//! A network implementation that does nothing.
//!
//! This is useful for wiring components together that don't require network but still need to be
//! generic over it.

use crate::{
    NetworkError, NetworkInfo, PeerKind, Peers, PeersInfo, Reputation, ReputationChangeKind,
};
use async_trait::async_trait;
use reth_eth_wire::{DisconnectReason, ProtocolVersion};
use reth_primitives::{Chain, NodeRecord, PeerId};
use reth_rpc_types::{EthProtocolInfo, NetworkStatus};
use std::net::{IpAddr, SocketAddr};

/// A type that implements all network trait that does nothing.
///
/// Intended for testing purposes where network is not used.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NoopNetwork;

#[async_trait]
impl NetworkInfo for NoopNetwork {
    fn local_addr(&self) -> SocketAddr {
        (IpAddr::from(std::net::Ipv4Addr::UNSPECIFIED), 30303).into()
    }

    async fn network_status(&self) -> Result<NetworkStatus, NetworkError> {
        Ok(NetworkStatus {
            client_version: "reth-test".to_string(),
            protocol_version: ProtocolVersion::V5 as u64,
            eth_protocol_info: EthProtocolInfo {
                difficulty: Default::default(),
                head: Default::default(),
                network: 1,
                genesis: Default::default(),
            },
        })
    }

    fn chain_id(&self) -> u64 {
        Chain::mainnet().into()
    }

    fn is_syncing(&self) -> bool {
        false
    }

    fn is_initially_syncing(&self) -> bool {
        false
    }
}

impl PeersInfo for NoopNetwork {
    fn num_connected_peers(&self) -> usize {
        0
    }

    fn local_node_record(&self) -> NodeRecord {
        NodeRecord::new(self.local_addr(), PeerId::random())
    }
}

#[async_trait]
impl Peers for NoopNetwork {
    fn add_peer_kind(&self, _peer: PeerId, _kind: PeerKind, _addr: SocketAddr) {}

    fn remove_peer(&self, _peer: PeerId, _kind: PeerKind) {}

    fn disconnect_peer(&self, _peer: PeerId) {}

    fn disconnect_peer_with_reason(&self, _peer: PeerId, _reason: DisconnectReason) {}

    fn reputation_change(&self, _peer_id: PeerId, _kind: ReputationChangeKind) {}

    async fn reputation_by_id(&self, _peer_id: PeerId) -> Result<Option<Reputation>, NetworkError> {
        Ok(None)
    }
}

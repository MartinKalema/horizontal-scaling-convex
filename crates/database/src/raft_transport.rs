//! Raft message transport between nodes via gRPC.
//!
//! Each node runs a `RaftTransportServer` that accepts Raft messages
//! (AppendEntries, Vote, Heartbeat, Snapshot) from peer nodes. The
//! `RaftTransportClient` sends messages to peers.
//!
//! Messages are batched per RPC call — the same optimization TiKV uses
//! to reduce network overhead. Each `RaftMessageBatch` can carry multiple
//! Raft protocol messages.
//!
//! The transport serializes raft-rs `Message` structs using prost (since
//! we use the prost-codec feature of raft-rs). On the receiving side,
//! messages are deserialized and fed into the Raft node's `step()` function
//! via the mailbox channel.
//!
//! Reference: [TiKV Raft Transport](https://www.pingcap.com/blog/raft-in-tikv/)

use std::{
    collections::HashMap,
    sync::Arc,
};

use anyhow::Context;
use raft::prelude::Message;
use tokio::sync::mpsc;

use crate::raft_node::RaftMessage;

/// Encode a raft-rs Message to bytes for transport.
///
/// With protobuf-codec, raft-rs types implement protobuf::Message directly.
/// This is the same serialization TiKV uses — no prost version mismatch.
pub fn encode_raft_message(msg: &Message) -> Vec<u8> {
    use protobuf::Message as ProtoMsg;
    msg.write_to_bytes().expect("Raft message encoding failed")
}

/// Decode bytes back to a raft-rs Message.
pub fn decode_raft_message(data: &[u8]) -> anyhow::Result<Message> {
    use protobuf::Message as ProtoMsg;
    Message::parse_from_bytes(data).context("Failed to decode Raft message")
}

/// Client for sending Raft messages to a remote node.
///
/// Maintains a gRPC connection to one peer. Multiple messages are batched
/// into a single RPC call for efficiency.
pub struct RaftTransportClient {
    /// The peer node's gRPC address.
    address: String,
    /// Receiver for outgoing messages to this peer.
    outgoing_rx: mpsc::UnboundedReceiver<Message>,
}

impl RaftTransportClient {
    pub fn new(address: String, outgoing_rx: mpsc::UnboundedReceiver<Message>) -> Self {
        Self {
            address,
            outgoing_rx,
        }
    }

    /// Run the transport client loop. Collects messages and sends them
    /// in batches via gRPC.
    /// Run the transport client loop.
    ///
    /// Follows TiKV's RaftClient pattern:
    /// - Exponential backoff on connection failure (1s → 2s → 4s, capped at
    ///   30s)
    /// - Never exits permanently — retries indefinitely
    /// - Messages queue in the channel while disconnected
    /// - Reports reconnection status via tracing
    ///
    /// etcd uses a similar pattern with rate-limited retries and dual
    /// stream/pipeline fallback. CockroachDB uses exponential backoff
    /// with connection pooling.
    pub async fn run(mut self) {
        use pb::replication::{
            raft_transport_service_client::RaftTransportServiceClient,
            RaftMessageBatch,
        };

        let mut backoff = std::time::Duration::from_secs(1);
        let max_backoff = std::time::Duration::from_secs(30);

        // Connect with exponential backoff retry (TiKV pattern).
        // Nodes start at different times — the transport must wait for
        // peers to become available without exiting.
        let mut client;
        loop {
            match RaftTransportServiceClient::connect(self.address.clone()).await {
                Ok(c) => {
                    tracing::info!("Raft transport: connected to {}", self.address);
                    client = c;
                    backoff = std::time::Duration::from_secs(1);
                    break;
                },
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                },
            }
        }

        loop {
            // Wait for at least one message.
            let msg = match self.outgoing_rx.recv().await {
                Some(msg) => msg,
                None => {
                    tracing::info!("Raft transport: channel closed for {}", self.address);
                    return;
                },
            };

            // Collect all available messages into a batch (TiKV batches
            // messages per RPC to reduce network overhead).
            let mut batch = vec![encode_raft_message(&msg)];
            while let Ok(msg) = self.outgoing_rx.try_recv() {
                batch.push(encode_raft_message(&msg));
            }

            let request = tonic::Request::new(RaftMessageBatch { messages: batch });

            if let Err(e) = client.send_messages(request).await {
                tracing::warn!(
                    "Raft transport: send to {} failed: {e}, reconnecting...",
                    self.address
                );
                // Reconnect with exponential backoff (TiKV/CockroachDB pattern).
                backoff = std::time::Duration::from_secs(1);
                loop {
                    match RaftTransportServiceClient::connect(self.address.clone()).await {
                        Ok(c) => {
                            client = c;
                            tracing::info!("Raft transport: reconnected to {}", self.address);
                            backoff = std::time::Duration::from_secs(1);
                            break;
                        },
                        Err(_) => {
                            tokio::time::sleep(backoff).await;
                            backoff = std::cmp::min(backoff * 2, max_backoff);
                        },
                    }
                }
            }
        }
    }
}

/// Server-side handler for receiving Raft messages from peers.
///
/// Deserializes incoming messages and forwards them to the local Raft
/// node's mailbox.
pub struct RaftTransportServer {
    /// Mailbox to forward received messages to the local Raft node.
    mailbox_tx: mpsc::UnboundedSender<RaftMessage>,
}

impl RaftTransportServer {
    pub fn new(mailbox_tx: mpsc::UnboundedSender<RaftMessage>) -> Self {
        Self { mailbox_tx }
    }

    pub fn into_service(
        self,
    ) -> pb::replication::raft_transport_service_server::RaftTransportServiceServer<Self> {
        pb::replication::raft_transport_service_server::RaftTransportServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl pb::replication::raft_transport_service_server::RaftTransportService for RaftTransportServer {
    async fn send_messages(
        &self,
        request: tonic::Request<pb::replication::RaftMessageBatch>,
    ) -> Result<tonic::Response<pb::replication::RaftMessageResponse>, tonic::Status> {
        let batch = request.into_inner();

        for data in batch.messages {
            match decode_raft_message(&data) {
                Ok(msg) => {
                    if self.mailbox_tx.send(RaftMessage::Raft(msg)).is_err() {
                        return Err(tonic::Status::unavailable("Raft node not running"));
                    }
                },
                Err(e) => {
                    tracing::warn!("Raft transport: failed to decode message: {e}");
                },
            }
        }

        Ok(tonic::Response::new(
            pb::replication::RaftMessageResponse {},
        ))
    }
}

/// Create transport channels for a set of peers.
///
/// Returns:
/// - `peer_senders`: Map of peer_id → sender (for the Raft node to send
///   messages)
/// - `transport_clients`: Vec of (address, RaftTransportClient) to spawn as
///   background tasks
pub fn create_transport(
    peer_addresses: &HashMap<u64, String>,
    local_node_id: u64,
) -> (
    HashMap<u64, mpsc::UnboundedSender<Message>>,
    Vec<RaftTransportClient>,
) {
    let mut peer_senders = HashMap::new();
    let mut clients = Vec::new();

    for (&peer_id, address) in peer_addresses {
        if peer_id == local_node_id {
            continue;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        peer_senders.insert(peer_id, tx);
        clients.push(RaftTransportClient::new(address.clone(), rx));
    }

    (peer_senders, clients)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut msg = Message::default();
        msg.set_msg_type(raft::prelude::MessageType::MsgAppend);
        msg.set_to(2);
        msg.set_from(1);
        msg.set_term(5);
        msg.set_log_term(4);
        msg.set_index(10);

        let encoded = encode_raft_message(&msg);
        let decoded = decode_raft_message(&encoded).unwrap();

        assert_eq!(decoded.get_to(), 2);
        assert_eq!(decoded.get_from(), 1);
        assert_eq!(decoded.get_term(), 5);
        assert_eq!(decoded.get_log_term(), 4);
        assert_eq!(decoded.get_index(), 10);
    }

    #[test]
    fn test_create_transport_excludes_self() {
        let mut addresses = HashMap::new();
        addresses.insert(1, "http://node-a:50051".to_string());
        addresses.insert(2, "http://node-b:50051".to_string());
        addresses.insert(3, "http://node-c:50051".to_string());

        let (senders, clients) = create_transport(&addresses, 1);

        // Should have senders for nodes 2 and 3, but not 1 (self).
        assert_eq!(senders.len(), 2);
        assert!(senders.contains_key(&2));
        assert!(senders.contains_key(&3));
        assert!(!senders.contains_key(&1));
        assert_eq!(clients.len(), 2);
    }
}

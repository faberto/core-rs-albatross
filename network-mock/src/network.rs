use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use futures::{
    channel::{mpsc, oneshot},
    future::{BoxFuture, FutureExt},
    stream::{BoxStream, StreamExt},
    SinkExt,
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::broadcast::Sender;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

use beserial::{Deserialize, Serialize};
use nimiq_network_interface::{
    message::{Message, RequestError, ResponseError, ResponseMessage},
    network::Network,
    network::{MsgAcceptance, NetworkEvent, PubsubId, Topic},
    peer::Peer,
    peer_map::ObservablePeerMap,
};

use crate::hub::{MockHubInner, RequestKey, ResponseSender};
use crate::peer::MockPeer;
use crate::{MockAddress, MockPeerId};

#[derive(Debug, Error, PartialEq)]
pub enum MockNetworkError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] beserial::SerializingError),

    #[error("Can't connect to peer: {0}")]
    CantConnect(MockAddress),

    #[error("Network is not connected")]
    NotConnected,

    #[error("Peer is already subscribed to topic: {0}")]
    AlreadySubscribed(&'static str),

    #[error("Peer is already unsubscribed to topic: {0}")]
    AlreadyUnsubscribed(&'static str),

    #[error("Can't respond to request: {0}")]
    CantRespond(MockRequestId),
}

pub type MockRequestId = u64;

#[derive(Clone, Debug)]
pub struct MockId<P> {
    propagation_source: P,
}

impl MockId<MockPeerId> {
    pub fn new(propagation_source: MockPeerId) -> Self {
        Self { propagation_source }
    }
}

impl PubsubId<MockPeerId> for MockId<MockPeerId> {
    fn propagation_source(&self) -> MockPeerId {
        self.propagation_source
    }
}

#[derive(Debug)]
pub struct MockNetwork {
    address: MockAddress,
    peers: ObservablePeerMap<MockPeer>,
    hub: Arc<Mutex<MockHubInner>>,
    is_connected: Arc<AtomicBool>,
}

impl MockNetwork {
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    pub(crate) fn new(address: MockAddress, hub: Arc<Mutex<MockHubInner>>) -> Self {
        let peers = ObservablePeerMap::default();

        let is_connected = {
            let mut hub = hub.lock();

            // Insert out peer map into global peer maps table
            if hub.peer_maps.insert(address, peers.clone()).is_some() {
                panic!(
                    "address/peer_id of MockNetwork must be unique: address={}",
                    address
                );
            }

            // Insert our is_connected bool into the hub
            let is_connected = Arc::new(AtomicBool::new(false));
            hub.is_connected.insert(address, Arc::clone(&is_connected));

            is_connected
        };

        Self {
            address,
            peers,
            hub,
            is_connected,
        }
    }

    pub fn address(&self) -> MockAddress {
        self.address
    }

    pub fn peer_id(&self) -> MockPeerId {
        self.address.into()
    }

    fn dial_mock_address(&self, address: MockAddress) -> Result<(), MockNetworkError> {
        let hub = self.hub.lock();

        log::debug!("Peer {} dialing peer {}", self.address, address);

        // Insert ourselves into peer's peer list.
        // This also makes sure the other peer actually exists.
        let is_new = hub
            .peer_maps
            .get(&address)
            .ok_or(MockNetworkError::CantConnect(address))?
            .insert(MockPeer {
                network_address: address,
                peer_id: self.address.into(),
                hub: Arc::clone(&self.hub),
            });

        if is_new {
            // Insert peer into out peer list
            self.peers.insert(MockPeer {
                network_address: self.address,
                peer_id: address.into(),
                hub: Arc::clone(&self.hub),
            });

            // Set is_connected flag for this network
            self.is_connected.store(true, Ordering::SeqCst);

            // Set is_connected flag for other network
            let is_connected = hub.is_connected.get(&address).unwrap();
            is_connected.store(true, Ordering::SeqCst);
        } else {
            log::trace!("Peers are already connected.");
        }

        Ok(())
    }

    /// Dials another mock network. Might panic if the peers are not in the same hub (i.e. if the address of the
    /// other network doesn't exist in our hub).
    pub fn dial_mock(&self, other: &Self) {
        self.dial_mock_address(other.address).unwrap();
    }

    /// Disconnect from all peers
    pub fn disconnect(&self) {
        let hub = self.hub.lock();

        for peer in self.peers.remove_all() {
            let peer_map = hub.peer_maps.get(&peer.id().into()).unwrap_or_else(|| {
                panic!(
                    "We're connected to a peer that doesn't have a connection to us: our_peer_id={}, their_peer_id={}",
                    self.address,
                    peer.id()
                )
            });
            peer_map.remove(&self.address.into());
        }

        self.is_connected.store(false, Ordering::SeqCst);
    }

    /// Disconnects from all peers and deletes this peer from the hub to prevent future connections
    /// to or from it.
    pub fn shutdown(&self) {
        self.disconnect();
        self.hub.lock().peer_maps.remove(&self.address);
    }
}

#[async_trait]
impl Network for MockNetwork {
    type PeerType = MockPeer;
    type AddressType = MockAddress;
    type Error = MockNetworkError;
    type PubsubId = MockId<MockPeerId>;
    type RequestId = MockRequestId;

    fn get_peer_updates(&self) -> (Vec<Arc<MockPeer>>, BroadcastStream<NetworkEvent<MockPeer>>) {
        self.peers.subscribe()
    }

    fn get_peers(&self) -> Vec<Arc<MockPeer>> {
        self.peers.get_peers()
    }

    fn get_peer(&self, peer_id: MockPeerId) -> Option<Arc<MockPeer>> {
        self.peers.get_peer(&peer_id)
    }

    fn subscribe_events(&self) -> BroadcastStream<NetworkEvent<MockPeer>> {
        self.get_peer_updates().1
    }

    async fn subscribe<'a, T>(
        &self,
    ) -> Result<BoxStream<'a, (T::Item, Self::PubsubId)>, Self::Error>
    where
        T: Topic + Sync,
    {
        let mut hub = self.hub.lock();
        let is_connected = Arc::clone(&self.is_connected);

        let topic_name = T::NAME;

        log::debug!(
            "Peer {} subscribing to topic '{}'",
            self.address,
            topic_name
        );

        // Add this peer to the topic list
        let sender: &Sender<(Arc<Vec<u8>>, MockPeerId)> =
            if let Some(topic) = hub.subscribe(topic_name, self.address) {
                &topic.sender
            } else {
                return Err(MockNetworkError::AlreadySubscribed(topic_name));
            };

        let stream = BroadcastStream::new(sender.subscribe()).filter_map(move |r| {
            let is_connected = Arc::clone(&is_connected);

            async move {
                if is_connected.load(Ordering::SeqCst) {
                    match r {
                        Ok((data, peer_id)) => match T::Item::deserialize_from_vec(&data) {
                            Ok(item) => return Some((item, peer_id)),
                            Err(e) => {
                                log::warn!("Dropped item because deserialization failed: {}", e)
                            }
                        },
                        Err(BroadcastStreamRecvError::Lagged(_)) => {
                            log::warn!("Mock gossipsub channel is lagging")
                        }
                    }
                } else {
                    log::debug!("Network not connected: Dropping gossipsub message.");
                }

                None
            }
        });

        Ok(Box::pin(stream.map(|(topic, peer_id)| {
            let id = MockId {
                propagation_source: peer_id,
            };
            (topic, id)
        })))
    }

    async fn unsubscribe<'a, T>(&self) -> Result<(), Self::Error>
    where
        T: Topic + Sync,
    {
        let mut hub = self.hub.lock();

        let topic_name = T::NAME;

        log::debug!(
            "Peer {} unsubscribing from topic '{}'",
            self.address,
            topic_name
        );

        if self.is_connected.load(Ordering::SeqCst) {
            if hub.unsubscribe(topic_name, &self.address) {
                Ok(())
            } else {
                Err(MockNetworkError::AlreadyUnsubscribed(topic_name))
            }
        } else {
            Err(MockNetworkError::NotConnected)
        }
    }

    async fn publish<T: Topic>(&self, item: T::Item) -> Result<(), Self::Error>
    where
        T: Topic + Sync,
    {
        let mut hub = self.hub.lock();

        let topic_name = T::NAME;
        let data = item.serialize_to_vec();

        log::debug!(
            "Peer {} publishing on topic '{}': {:?}",
            self.address,
            topic_name,
            item
        );

        if self.is_connected.load(Ordering::SeqCst) {
            if let Some(topic) = hub.get_topic(topic_name) {
                topic
                    .sender
                    .send((Arc::new(data), self.address.into()))
                    .unwrap();
                Ok(())
            } else {
                log::debug!("No peer is subscribed to topic: '{}'", topic_name);
                Ok(())
            }
        } else {
            Err(MockNetworkError::NotConnected)
        }
    }

    fn validate_message<TTopic>(&self, _id: Self::PubsubId, _acceptance: MsgAcceptance)
    where
        TTopic: Topic + Sync,
    {
        // TODO implement
    }

    async fn dht_get<K, V>(&self, k: &K) -> Result<Option<V>, Self::Error>
    where
        K: AsRef<[u8]> + Send + Sync,
        V: Deserialize + Send + Sync,
    {
        if self.is_connected.load(Ordering::SeqCst) {
            let hub = self.hub.lock();

            if let Some(data) = hub.dht.get(k.as_ref()) {
                Ok(Some(V::deserialize_from_vec(data)?))
            } else {
                Ok(None)
            }
        } else {
            Err(MockNetworkError::NotConnected)
        }
    }

    async fn dht_put<K, V>(&self, k: &K, v: &V) -> Result<(), Self::Error>
    where
        K: AsRef<[u8]> + Send + Sync,
        V: Serialize + Send + Sync,
    {
        if self.is_connected.load(Ordering::SeqCst) {
            let mut hub = self.hub.lock();

            let data = v.serialize_to_vec();
            hub.dht.insert(k.as_ref().to_owned(), data);
            Ok(())
        } else {
            Err(MockNetworkError::NotConnected)
        }
    }

    async fn dial_peer(&self, peer_id: MockPeerId) -> Result<(), Self::Error> {
        self.dial_mock_address(peer_id.into())
    }

    async fn dial_address(&self, address: MockAddress) -> Result<(), Self::Error> {
        self.dial_mock_address(address)
    }

    fn get_local_peer_id(&self) -> MockPeerId {
        self.address.into()
    }

    async fn request<'a, Req: Message, Res: Message>(
        &self,
        request: Req,
        peer_id: MockPeerId,
    ) -> Result<
        BoxFuture<
            'a,
            (
                ResponseMessage<Res>,
                Self::RequestId,
                <Self::PeerType as Peer>::Id,
            ),
        >,
        RequestError,
    > {
        if self.peers.get_peer(&peer_id).is_none() {
            log::warn!(
                "Cannot send request {} from {} to {} - peers not connected",
                std::any::type_name::<Req>(),
                self.address,
                peer_id,
            );
            return Err(RequestError::SendError);
        }

        let sender_id = MockPeerId::from(self.address.clone());
        let (tx, rx) = oneshot::channel::<Vec<u8>>();

        let (mut sender, request_id) = {
            let mut hub = self.hub.lock();

            let key = RequestKey {
                recipient: peer_id.clone().into(),
                message_type: Req::TYPE_ID,
            };
            let sender = if let Some(sender) = hub.request_senders.get(&key) {
                sender.clone()
            } else {
                log::warn!("No request sender: {:?}", key);
                return Err(RequestError::SendError);
            };

            let request_id = hub.next_request_id;
            hub.response_senders.insert(
                request_id,
                ResponseSender {
                    peer: self.address.into(),
                    sender: tx,
                },
            );
            hub.next_request_id += 1;

            (sender, request_id)
        };

        let mut data = Vec::with_capacity(request.serialized_message_size());
        request.serialize_message(&mut data).unwrap();

        let request = (data, request_id, sender_id);
        if let Err(e) = sender.send(request).await {
            log::warn!(
                "Cannot send request {} from {} to {} - {:?}",
                std::any::type_name::<Req>(),
                self.address,
                peer_id,
                e
            );
            self.hub.lock().response_senders.remove(&request_id);
            return Err(RequestError::SendError);
        }

        let hub = Arc::clone(&self.hub);
        let future = tokio::time::timeout(MockNetwork::REQUEST_TIMEOUT, rx)
            .map(move |result| {
                let response = match result {
                    Ok(Ok(data)) => match Res::deserialize_message(&mut &data[..]) {
                        Ok(message) => ResponseMessage::Response(message),
                        Err(_) => ResponseMessage::Error(ResponseError::DeSerializationError),
                    },
                    Ok(Err(_)) => ResponseMessage::Error(ResponseError::SenderFutureDropped),
                    Err(_) => {
                        hub.lock().response_senders.remove(&request_id);
                        ResponseMessage::Error(ResponseError::Timeout)
                    }
                };
                (response, request_id, peer_id)
            })
            .boxed();
        Ok(future)
    }

    fn receive_requests<'a, M: Message>(
        &self,
    ) -> BoxStream<'a, (M, Self::RequestId, <Self::PeerType as Peer>::Id)> {
        let mut hub = self.hub.lock();
        let (tx, rx) = mpsc::channel(16);

        let key = RequestKey {
            recipient: self.address,
            message_type: M::TYPE_ID,
        };
        if hub.request_senders.insert(key, tx).is_some() {
            log::warn!(
                "Replacing existing request sender for {}",
                std::any::type_name::<M>()
            );
        }

        rx.filter_map(|(data, request_id, sender)| async move {
            match M::deserialize_message(&mut &data[..]) {
                Ok(message) => Some((message, request_id, sender)),
                Err(e) => {
                    log::warn!("Failed to deserialize request: {}", e);
                    None
                }
            }
        })
        .boxed()
    }

    async fn respond<'a, M: Message>(
        &self,
        request_id: Self::RequestId,
        response: M,
    ) -> Result<(), Self::Error> {
        let mut hub = self.hub.lock();
        if let Some(responder) = hub.response_senders.remove(&request_id) {
            if self.peers.get_peer(&responder.peer).is_none() {
                return Err(MockNetworkError::NotConnected);
            }

            let mut data = Vec::with_capacity(response.serialized_message_size());
            response.serialize_message(&mut data).unwrap();

            responder
                .sender
                .send(data)
                .map_err(|_| MockNetworkError::CantRespond(request_id))
        } else {
            Err(MockNetworkError::CantRespond(request_id))
        }
    }
}

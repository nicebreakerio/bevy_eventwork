use std::{marker::PhantomData, sync::Arc};

use async_channel::{unbounded, Receiver, Sender};
use bevy::prelude::*;
use dashmap::DashMap;

use async_trait::async_trait;

use crate::{
    error::NetworkError,
    network_message::{ClientMessage, ServerMessage},
    runtime::JoinHandle,
    AsyncChannel, ClientNetworkEvent, Connection, ConnectionId, NetworkData, NetworkPacket,
    Runtime,
};

/// A trait used by [`NetworkClient`] to drive a client, this is responsible
/// for generating the futures that carryout the underlying client logic.
#[async_trait]
pub trait NetworkClientProvider: 'static + Send + Sync {
    /// This is to configure particular protocols
    type NetworkSettings: Send + Sync + Clone;

    /// The type that acts as a combined sender and reciever for a client.
    /// This type needs to be able to be split.
    type Socket: Send;

    /// The read half of the given socket type.
    type ReadHalf: Send;

    /// The write half of the given socket type.
    type WriteHalf: Send;

    /// Connect to the server, this will technically live as a long running task, but it can complete.
    async fn connect_task(
        network_settings: Self::NetworkSettings,
        new_connections: Sender<Self::Socket>,
        errors: Sender<ClientNetworkEvent>,
    );

    /// Recieves messages from the server.
    async fn recv_loop(
        read_half: Self::ReadHalf,
        messages: Sender<NetworkPacket>,
        settings: Self::NetworkSettings,
    );

    /// Writes messages to the server.
    async fn send_loop(
        write_half: Self::WriteHalf,
        messages: Receiver<NetworkPacket>,
        settings: Self::NetworkSettings,
    );

    /// Split the socket into a read and write half, so that the two actions
    /// can be handled concurrently.
    fn split(combined: Self::Socket) -> (Self::ReadHalf, Self::WriteHalf);
}

/// An instance of a [`NetworkClient`] is used to connect to a remote server
/// using [`NetworkClient::connect`]
pub struct NetworkClient<NCP: NetworkClientProvider> {
    server_connection: Option<Connection>,
    recv_message_map: Arc<DashMap<&'static str, Vec<String>>>,
    network_events: AsyncChannel<ClientNetworkEvent>,
    connection_events: AsyncChannel<NCP::Socket>,
    connection_task: Option<Box<dyn JoinHandle>>,
    provider: PhantomData<NCP>,
}

impl<NCP: NetworkClientProvider> std::fmt::Debug for NetworkClient<NCP> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(_conn) = self.server_connection.as_ref() {
            write!(f, "NetworkClient [Connected to server]")?;
        } else {
            write!(f, "NetworkClient [Not Connected]")?;
        }

        Ok(())
    }
}

impl<NCP: NetworkClientProvider> NetworkClient<NCP> {
    pub(crate) fn new(_provider: NCP) -> Self {
        Self {
            server_connection: None,
            recv_message_map: Arc::new(DashMap::new()),
            network_events: AsyncChannel::new(),
            connection_events: AsyncChannel::new(),
            connection_task: None,
            provider: PhantomData,
        }
    }

    /// Start async connecting to a remote server.
    ///
    /// ## Note
    /// This will disconnect you first from any existing server connections
    pub fn connect<'a, RT: Runtime>(&mut self, runtime: &RT, connect_info: &NCP::NetworkSettings) {
        debug!("Starting connection");

        self.disconnect();

        let network_error_sender = self.network_events.sender.clone();
        let connection_event_sender = self.connection_events.sender.clone();

        self.connection_task = Some(Box::new(runtime.spawn(NCP::connect_task(
            connect_info.clone(),
            connection_event_sender,
            network_error_sender,
        ))));
    }

    /// a server
    ///
    /// This operation is idempotent and simply does nothing when you are
    /// not connected to anything
    pub fn disconnect(&mut self) {
        if let Some(conn) = self.server_connection.take() {
            conn.stop();

            let _ = self
                .network_events
                .sender
                .send(ClientNetworkEvent::Disconnected);
        }
    }

    /// Send a message to the connected server, returns `Err(NetworkError::NotConnected)` if
    /// the connection hasn't been established yet
    pub fn send_message<T: ServerMessage>(&self, message: T) -> Result<(), NetworkError> {
        debug!("Sending message to server");
        let server_connection = match self.server_connection.as_ref() {
            Some(server) => server,
            None => return Err(NetworkError::NotConnected),
        };

        let packet = NetworkPacket {
            kind: String::from(T::NAME),
            data: serde_json::to_string(&message).unwrap(),
        };

        match server_connection.send_message.try_send(packet) {
            Ok(_) => (),
            Err(err) => {
                error!("Server disconnected: {}", err);
                return Err(NetworkError::NotConnected);
            }
        }

        Ok(())
    }

    /// Returns true if the client has an established connection
    ///
    /// # Note
    /// This may return true even if the connection has already been broken on the server side.
    pub fn is_connected(&self) -> bool {
        self.server_connection.is_some()
    }
}

/// A utility trait on [`App`] to easily register [`ClientMessage`]s
pub trait AppNetworkClientMessage {
    /// Register a client message type
    ///
    /// ## Details
    /// This will:
    /// - Add a new event type of [`NetworkData<T>`]
    /// - Register the type for transformation over the wire
    /// - Internal bookkeeping
    fn listen_for_client_message<T: ClientMessage, NCP: NetworkClientProvider>(
        &mut self,
    ) -> &mut Self;
}

impl AppNetworkClientMessage for App {
    fn listen_for_client_message<T: ClientMessage, NCP: NetworkClientProvider>(
        &mut self,
    ) -> &mut Self {
        let client = self.world.get_resource::<NetworkClient<NCP>>().expect("Could not find `NetworkClient`. Be sure to include the `ClientPlugin` before listening for client messages.");

        debug!("Registered a new ClientMessage: {}", T::NAME);

        assert!(
            !client.recv_message_map.contains_key(T::NAME),
            "Duplicate registration of ClientMessage: {}",
            T::NAME
        );
        client.recv_message_map.insert(T::NAME, Vec::new());

        self.add_event::<NetworkData<T>>();
        self.add_system_to_stage(CoreStage::PreUpdate, register_client_message::<T, NCP>)
    }
}

fn register_client_message<T, NCP: NetworkClientProvider>(
    net_res: ResMut<NetworkClient<NCP>>,
    mut events: EventWriter<NetworkData<T>>,
) where
    T: ClientMessage,
{
    let mut messages = match net_res.recv_message_map.get_mut(T::NAME) {
        Some(messages) => messages,
        None => return,
    };

    events.send_batch(
        messages
            .drain(..)
            .filter_map(|msg| serde_json::from_str(&msg).ok())
            .map(|msg| NetworkData::<T>::new(ConnectionId::server(), msg)),
    );
}

/// Pushes messages into the network event queue.
pub fn handle_connection_event<NCP: NetworkClientProvider, RT: Runtime>(
    mut net_res: ResMut<NetworkClient<NCP>>,
    mut events: EventWriter<ClientNetworkEvent>,
    runtime: Res<RT>,
    network_settings: Res<NCP::NetworkSettings>,
) {
    let connection = match net_res.connection_events.receiver.try_recv() {
        Ok(event) => event,
        Err(_err) => {
            return;
        }
    };

    let (read_half, write_half) = NCP::split(connection);
    let recv_message_map = net_res.recv_message_map.clone();
    let (outgoing_tx, outgoing_rx) = unbounded();
    let (incoming_tx, incoming_rx) = unbounded();
    let network_event_sender = net_res.network_events.sender.clone();
    let read_network_settings = network_settings.clone();
    let write_network_settings = network_settings.clone();

    net_res.server_connection = Some(Connection {
        send_task: Box::new(runtime.spawn(async move {
            trace!("Starting send task");
            NCP::send_loop(write_half, outgoing_rx, write_network_settings).await;
        })),
        receive_task: Box::new(runtime.spawn(async move {
            trace!("Starting listen task");
            NCP::recv_loop(read_half, incoming_tx, read_network_settings).await;

            match network_event_sender
                .send(ClientNetworkEvent::Disconnected)
                .await
            {
                Ok(_) => (),
                Err(_) => {
                    error!("Could not send disconnected event, because channel is disconnected");
                }
            }
        })),
        map_receive_task: Box::new(runtime.spawn(async move {
            while let Ok(packet) = incoming_rx.recv().await {
                match recv_message_map.get_mut(&packet.kind[..]) {
                    Some(mut packets) => packets.push(packet.data),
                    None => {
                        error!(
                            "Could not find existing entries for message kinds: {:?}",
                            packet
                        );
                    }
                }
            }
        })),
        send_message: outgoing_tx,
    });

    events.send(ClientNetworkEvent::Connected);
}

/// Takes events and forwards them to the server.
pub fn send_client_network_events<NCP: NetworkClientProvider, RT: Runtime>(
    client_server: ResMut<NetworkClient<NCP>>,
    mut client_network_events: EventWriter<ClientNetworkEvent>,
) {
    client_network_events.send_batch(
        std::iter::repeat_with(|| client_server.network_events.receiver.try_recv().ok())
            .map_while(|val| val),
    );
}

use std::net::SocketAddr;

use crate::{
    async_channel::{Receiver, Sender},
    async_trait,
    client::NetworkClientProvider,
    error::NetworkError,
    server::NetworkServerProvider,
    ClientNetworkEvent, NetworkPacket,
};
use async_net::{TcpListener, TcpStream};
use bevy::log::{debug, error, info, trace};
use futures_lite::{AsyncReadExt, AsyncWriteExt};

#[derive(Default, Debug)]
/// Provides a tcp stream and listener for eventwork.
pub struct TcpServerProvider;

#[async_trait]
impl NetworkServerProvider for TcpServerProvider {
    type NetworkSettings = NetworkSettings;

    type Socket = TcpStream;

    type ReadHalf = TcpStream;

    type WriteHalf = TcpStream;

    async fn accept_loop(
        network_settings: Self::NetworkSettings,
        new_connections: Sender<Self::Socket>,
        errors: Sender<NetworkError>,
    ) {
        let listener = match TcpListener::bind(network_settings.addr).await {
            Ok(listener) => listener,
            Err(err) => {
                if let Err(err) = errors.send(NetworkError::Listen(err)).await {
                    error!("Could not send listen error: {}", err);
                }
                return;
            }
        };

        let new_connections = new_connections;
        loop {
            let resp = match listener.accept().await {
                Ok((socket, _addr)) => socket,
                Err(error) => {
                    if let Err(err) = errors.send(NetworkError::Accept(error)).await {
                        error!("Could not send listen error: {}", err);
                        return;
                    };
                    continue;
                }
            };

            if let Err(err) = new_connections.send(resp).await {
                error!("Could not send listen error: {}", err);
                return;
            }
            info!("New Connection Made!");
        }
    }

    async fn recv_loop(
        mut read_half: Self::ReadHalf,
        messages: Sender<NetworkPacket>,
        settings: Self::NetworkSettings,
    ) {
        let mut buffer = vec![0; settings.max_packet_length];
        loop {
            trace!("Reading message length");
            let length = match read_half.read(&mut buffer[..8]).await {
                Ok(0) => {
                    // EOF, meaning the TCP stream has closed.
                    info!("Client disconnected");
                    // TODO: probably want to do more than just quit the receive task.
                    //       to let eventwork know that the peer disconnected.
                    break;
                }
                Ok(8) => {
                    let bytes = &buffer[..8];
                    u64::from_le_bytes(bytes.try_into().unwrap()) as usize
                }
                Ok(n) => {
                    error!(
                        "Could not read enough bytes for header. Expected 8, got {}",
                        n
                    );
                    break;
                }
                Err(err) => {
                    error!("Encountered error while fetching length: {}", err);
                    break;
                }
            };
            debug!("Receiving new message of size: {}", length);

            if length > settings.max_packet_length {
                error!(
                    "Received too large packet: {} > {}",
                    length, settings.max_packet_length
                );
                break;
            }

            trace!("Reading message into buffer");
            match read_half.read_exact(&mut buffer[..length]).await {
                Ok(()) => (),
                Err(err) => {
                    error!(
                        "Encountered error while fetching stream of length {}: {}",
                        length, err
                    );
                    break;
                }
            }
            trace!("Message read");

            let packet: NetworkPacket = match serde_json::from_slice(&buffer[..length]) {
                Ok(packet) => packet,
                Err(err) => {
                    error!("Failed to decode network packet from: {}", err);
                    break;
                }
            };

            if let Err(_) = messages.send(packet).await {
                error!("Failed to send decoded message to eventwork");
                break;
            }
            trace!("Message deserialized and sent to eventwork");
        }
    }

    async fn send_loop(
        mut write_half: Self::WriteHalf,
        messages: Receiver<NetworkPacket>,
        _settings: Self::NetworkSettings,
    ) {
        while let Ok(message) = messages.recv().await {
            let encoded = match serde_json::to_string(&message) {
                Ok(encoded) => encoded,
                Err(err) => {
                    error!("Could not encode packet {:?}: {}", message, err);
                    continue;
                }
            };

            let len = encoded.len() as u64;
            debug!("Sending a new message of size: {}", len);

            match write_half.write(&len.to_le_bytes()).await {
                Ok(_) => (),
                Err(err) => {
                    error!("Could not send packet length: {:?}: {}", len, err);
                    break;
                }
            }

            trace!("Sending the content of the message!");

            match write_half.write_all(&encoded.as_bytes()).await {
                Ok(_) => (),
                Err(err) => {
                    error!("Could not send packet: {:?}: {}", message, err);
                    break;
                }
            }

            trace!("Succesfully written all!");
        }
    }

    fn split(combined: Self::Socket) -> (Self::ReadHalf, Self::WriteHalf) {
        (combined.clone(), combined)
    }
}

#[derive(Default, Debug)]
/// Provides a tcp stream for Eventwork
pub struct TcpClientProvider;

#[async_trait]
impl NetworkClientProvider for TcpClientProvider {
    type NetworkSettings = NetworkSettings;

    type Socket = TcpStream;

    type ReadHalf = TcpStream;

    type WriteHalf = TcpStream;

    async fn connect_task(
        network_settings: Self::NetworkSettings,
        new_connections: Sender<Self::Socket>,
        errors: Sender<ClientNetworkEvent>,
    ) {
        info!("Beginning connection");
        let stream = match TcpStream::connect(network_settings.addr).await {
            Ok(stream) => stream,
            Err(error) => {
                match errors
                    .send(ClientNetworkEvent::Error(NetworkError::Connection(error)))
                    .await
                {
                    Ok(_) => (),
                    Err(err) => {
                        error!("Could not send error event: {}", err);
                    }
                }

                return;
            }
        };

        info!("Connected!");

        let addr = stream
            .peer_addr()
            .expect("Could not fetch peer_addr of existing stream");

        match new_connections.send(stream).await {
            Ok(_) => (),
            Err(err) => {
                error!("Could not initiate connection: {}", err);
            }
        }

        debug!("Connected to: {:?}", addr);
    }

    async fn recv_loop(
        mut read_half: Self::ReadHalf,
        messages: Sender<NetworkPacket>,
        settings: Self::NetworkSettings,
    ) {
        let mut buffer = vec![0; settings.max_packet_length];
        loop {
            trace!("Reading message length");
            let length = match read_half.read(&mut buffer[..8]).await {
                Ok(0) => {
                    // EOF, meaning the TCP stream has closed.
                    info!("Client disconnected");
                    // TODO: probably want to do more than just quit the receive task.
                    //       to let eventwork know that the peer disconnected.
                    break;
                }
                Ok(8) => {
                    let bytes = &buffer[..8];
                    u64::from_le_bytes(bytes.try_into().unwrap()) as usize
                }
                Ok(n) => {
                    error!(
                        "Could not read enough bytes for header. Expected 8, got {}",
                        n
                    );
                    break;
                }
                Err(err) => {
                    error!("Encountered error while fetching length: {}", err);
                    break;
                }
            };
            trace!("Info read");

            if length > settings.max_packet_length {
                error!(
                    "Received too large packet: {} > {}",
                    length, settings.max_packet_length
                );
                break;
            }

            trace!("Reading message into buffer");
            match read_half.read_exact(&mut buffer[..length]).await {
                Ok(_) => (),
                Err(err) => {
                    error!(
                        "Encountered error while fetching stream of length {}: {}",
                        length, err
                    );
                    break;
                }
            }
            trace!("Message read");

            let packet: NetworkPacket = match serde_json::from_slice(&buffer[..length]) {
                Ok(packet) => packet,
                Err(err) => {
                    error!("Failed to decode network packet from: {}", err);
                    break;
                }
            };

            if let Err(_) = messages.send(packet).await {
                error!("Failed to send decoded message to Spicy");
                break;
            }
        }
    }

    async fn send_loop(
        mut write_half: Self::WriteHalf,
        messages: Receiver<NetworkPacket>,
        _settings: Self::NetworkSettings,
    ) {
        while let Ok(message) = messages.recv().await {
            info!("Sending message!");

            let encoded = match serde_json::to_string(&message) {
                Ok(encoded) => encoded,
                Err(err) => {
                    error!("Could not encode packet {:?}: {}", message, err);
                    continue;
                }
            };

            let len = encoded.len() as u64;
            debug!("Sending a new message of size: {}", len);

            match write_half.write(&len.to_le_bytes()).await {
                Ok(_) => (),
                Err(err) => {
                    error!("Could not send packet length: {:?}: {}", len, err);
                    break;
                }
            }

            trace!("Sending the content of the message!");

            match write_half.write_all(&encoded.as_bytes()).await {
                Ok(_) => (),
                Err(err) => {
                    error!("Could not send packet: {:?}: {}", message, err);
                    break;
                }
            }

            info!("Message sent!");
        }
    }

    fn split(combined: Self::Socket) -> (Self::ReadHalf, Self::WriteHalf) {
        (combined.clone(), combined)
    }
}

#[derive(Clone, Debug)]
#[allow(missing_copy_implementations)]
/// Settings to configure the network, both client and server
pub struct NetworkSettings {
    /// Maximum packet size in bytes. If a client ever exceeds this size, they will be disconnected
    ///
    /// ## Default
    /// The default is set to 10MiB
    pub max_packet_length: usize,

    /// Address to connect to or port to open
    pub addr: SocketAddr,
}

impl NetworkSettings {
    /// Create a new instance of [`NetworkSettings`]
    pub fn new(addr: impl Into<SocketAddr>) -> Self {
        Self {
            max_packet_length: 10 * 1024 * 1024,
            addr: addr.into(),
        }
    }
}

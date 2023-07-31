use std::{net::{UdpSocket, SocketAddr}, time::Instant, collections::{HashMap, hash_map::Entry}};



/// describes the static behavior of a client
///
///
pub struct ClientConfig {
    /// max message size in bytes, maximum is 65443 allowed by udp over IPv4
    pub max_message_size: u16,

    /// interval to send heartbeats at to prevent timeout
    pub heartbeat_interval: u128,
    /// timeout length for when to close a connection for not responding
    pub timeout: u128,

    /// set to true to accept incoming connections
    ///
    /// if false no connections will be accepted and will be replied to with a disconnect packet
    ///
    /// generally true for servers and false for clients
    pub listen: bool,

    /// list of channel configurations
    ///
    /// each channel should correspond to it's opposite receive/send on any other client
    pub channels: Vec<ChannelConfig>,
}

pub enum ChannelConfig {
    SendUnreliable,
    ReceiveUnreliable,
}


pub(crate) struct Socket {
    socket: UdpSocket,

    in_buffer: Vec<u8>,
    out_buffer: Vec<u8>,

    max_message_size: usize,
}

impl Socket {
    fn new(max_message_size: u16, bind_addr: SocketAddr) -> Result<Self, Error> {
        let socket = UdpSocket::bind(bind_addr)?;
        socket.set_nonblocking(true)?;

        let max_message_size = max_message_size as usize;

        Ok(Socket {
            socket,

            in_buffer: vec![0; max_message_size],
            out_buffer: Vec::with_capacity(max_message_size),

            max_message_size,
        })
    }

    fn clear_buffer(&mut self) {
        self.out_buffer.clear();
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.max_message_size - self.out_buffer.len() < bytes.len() {
            Err(Error::MessageTooLong)
        } else {
            self.out_buffer.extend_from_slice(bytes);

            Ok(())
        }

    }

    fn send(&mut self, addr: SocketAddr) -> Result<usize, Error> {
        Ok(self.socket.send_to(&self.out_buffer, addr)?)
    }

    fn receive(&mut self) -> Option<(&[u8], SocketAddr)> {
        self.socket.recv_from(&mut self.in_buffer).ok().and_then(|(received_bytes, origin)| Some((&self.in_buffer[..received_bytes], origin)))
    }

    fn heartbeat(&mut self, addr: SocketAddr) -> Result<(), Error> {
        self.clear_buffer();
        self.write(&[0])?;
        self.send(addr)?;
        Ok(())
    }

    fn close(&mut self, addr: SocketAddr) -> Result<(), Error> {
        self.clear_buffer();
        self.write(&[1])?;
        self.send(addr)?;
        Ok(())
    }

    fn channel_prefix(&mut self, channel_id: u8) -> Result<(), Error> {
        self.clear_buffer();
        self.write(&[channel_id + 2])?;
        Ok(())
    }
}


pub struct Client {
    socket: Socket,

    connections: HashMap<SocketAddr, Connection>,

    config: ClientConfig,

    events: Vec<Event>,
}

impl Client {
    pub fn bind(config: ClientConfig, bind_addr: SocketAddr) -> Result<Self, Error> {
        if config.channels.len() > (u8::MAX - 2) as usize {
            return Err(Error::TooManyChannels);
        }

        let socket = Socket::new(config.max_message_size, bind_addr)?;

        Ok(Client {
            socket,

            connections: HashMap::new(),

            config,

            events: Vec::new(),
        })
    }

    pub fn bind_any(config: ClientConfig) -> Result<Self, Error> {
        Client::bind(config, "0.0.0.0:0".parse().unwrap())
    }

    pub fn connect(&mut self, addr: SocketAddr) -> Result<(), Error> {
        self.connections.insert(addr, Connection::new(&self.config, addr, &mut self.socket)?);

        self.events.push(Event::Connection(addr));

        Ok(())
    }

    pub fn disconnect(&mut self, addr: SocketAddr) -> Result<bool, Error> {
        Ok(if self.connections.remove(&addr).is_some() {
            self.socket.close(addr)?;
            self.events.push(Event::Disconnection(addr, DisconnectReason::Kicked));

            true
        } else {
            false
        })
    }

    pub fn update(&mut self) -> Result<Vec<Event>, Error> {

        // receive messages
        while let Some((message, origin)) = self.socket.receive() {

            let mut channel_message = None;

            let valid_message = match message.get(0) {
                None => false,
                Some(0) => true,
                Some(1) => {
                    if self.connections.remove(&origin).is_some() {
                        self.events.push(Event::Disconnection(origin, DisconnectReason::Other));
                    }

                    false
                },
                Some(channel_id) => {
                    let channel_id = *channel_id - 2;
                    if ((channel_id) as usize) < self.config.channels.len() {
                        channel_message = Some((channel_id, Vec::from(&message[1..])));
                        true
                    } else {
                        false
                    }
                },
            };

            if valid_message {
                let connection = match self.connections.entry(origin) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        if self.config.listen {
                            let connection = entry.insert(Connection::new(&self.config, origin, &mut self.socket)?);
                            self.events.push(Event::Connection(origin));
                            connection
                        } else {
                            self.socket.close(origin)?;
                            continue;
                        }
                    },
                };

                connection.last_received_keep_alive = Instant::now();

                if let Some((channel_id, message)) = channel_message {
                    if let Some(channel) = connection.channels.get_mut(channel_id as usize) {
                        for message in channel.receive(message, &mut self.socket)? {
                            self.events.push(Event::Message(origin, channel_id, message));
                        }
                    }
                }
            }
        }

        // timeout clients
        let mut to_remove = Vec::new();

        for (&origin, connection) in self.connections.iter_mut() {
            if connection.last_received_keep_alive.elapsed().as_millis() > self.config.timeout {
                to_remove.push(origin);
            }
        }

        for addr in to_remove {
            self.connections.remove(&addr);

            self.events.push(Event::Disconnection(addr, DisconnectReason::Timeout));
        }


        // update clients
        for connection in self.connections.values_mut() {
            connection.update(&mut self.socket)?;
        }


        Ok(std::mem::replace(&mut self.events, Vec::new()))
    }

    pub fn send(&mut self, addr: SocketAddr, channel_id: u8, message: &[u8]) -> Result<(), Error> {
        let Some(connection) = self.connections.get_mut(&addr) else {return Err(Error::AddressNotConnected);};

        let Some(channel) = connection.channels.get_mut(channel_id as usize) else {return Err(Error::InvalidChannelId);};

        channel.send(message, &mut self.socket)?;

        Ok(())
    }

    pub fn send_single(&mut self, channel_id: u8, message: &[u8]) -> Result<(), Error> {
        let mut addresses = self.connections.keys();
        match (addresses.next(), addresses.next()) {
            (None, None) => Err(Error::SendSingleInvalid),
            (Some(_), Some(_)) => Err(Error::SendSingleInvalid),
            (Some(&addr), None) => self.send(addr, channel_id, message),
            _ => unreachable!(),
        }
    }
}

pub struct Connection {
    addr: SocketAddr,

    heartbeat_interval: u128,

    last_received_keep_alive: Instant,
    last_sent_keep_alive: Instant,

    channels: Vec<Channel>,
}

impl Connection {
    fn new(config: &ClientConfig, addr: SocketAddr, socket: &mut Socket) -> Result<Self, Error> {
        socket.heartbeat(addr)?;

        Ok(Connection {
            addr,

            heartbeat_interval: config.heartbeat_interval,

            last_received_keep_alive: Instant::now(),
            last_sent_keep_alive: Instant::now(),

            channels: config.channels.iter().enumerate().map(|(id, c)| Channel::new(c, id as u8, addr)).collect(),
        })
    }

    fn update(&mut self, socket: &mut Socket) -> Result<(), Error> {
        if self.last_sent_keep_alive.elapsed().as_millis() > self.heartbeat_interval {
            socket.heartbeat(self.addr)?;
            self.last_sent_keep_alive = Instant::now();
        }

        Ok(())
    }
}


pub enum Event {
    Connection(SocketAddr),
    Disconnection(SocketAddr, DisconnectReason),
    Message(SocketAddr, u8, Vec<u8>),
}

#[derive(Debug)]
pub enum DisconnectReason {
    Kicked,
    Other,
    Timeout,
}


struct Channel {
    addr: SocketAddr,
    channel_id: u8,

    channel_type: ChannelType,
}

enum ChannelType {
    SendUnreliable,
    ReceiveUnreliable,
}

impl Channel {
    fn new(config: &ChannelConfig, channel_id: u8, addr: SocketAddr) -> Self {
        Channel {
            addr,
            channel_id,

            channel_type: match config {
                ChannelConfig::SendUnreliable => ChannelType::SendUnreliable,
                ChannelConfig::ReceiveUnreliable => ChannelType::ReceiveUnreliable,
            }
        }
    }

    fn send(&mut self, message: &[u8], socket: &mut Socket) -> Result<(), Error> {
        socket.channel_prefix(self.channel_id)?;

        match self.channel_type {
            ChannelType::ReceiveUnreliable => return Err(Error::SendOnReceiveChannel),

            ChannelType::SendUnreliable => {
                socket.write(message)?;
                socket.send(self.addr)?;
            }
        }

        Ok(())
    }

    fn receive(&mut self, message: Vec<u8>, socket: &mut Socket) -> Result<Vec<Vec<u8>>, Error> {
        let _ = socket;

        Ok(match self.channel_type {
            ChannelType::SendUnreliable => vec![],

            ChannelType::ReceiveUnreliable => vec![message],
        })
    }
}

#[derive(Debug)]
pub enum Error {
    /// returned when trying to create a client with more than 253 channels
    TooManyChannels,
    /// returned when trying to send a message that is too long
    MessageTooLong,
    /// returned when trying to send a message on a channel meant for receiving
    SendOnReceiveChannel,
    /// returned when trying to send to an address that doesn't exist
    AddressNotConnected,
    /// returned when trying to send on a channel id that doesn't exist
    InvalidChannelId,
    /// returned when either 0 or more than one connection is present when trying to use Client::send_single
    SendSingleInvalid,
    /// returned when an io error is encountered
    IoError(std::io::Error)
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::IoError(value)
    }
}




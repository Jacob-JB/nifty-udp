use std::{net::{UdpSocket, SocketAddr}, time::{Instant, UNIX_EPOCH, SystemTime}, collections::{HashMap, hash_map::Entry, VecDeque}};
use rand::{thread_rng, rngs::ThreadRng, Rng};


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

    /// how many ping time samples to keep
    pub ping_memory_length: u8,

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
    SendReliable {
        /// at what multiple after the connections average ping time should a message be resent
        resend_threshhold: f32,
    },
    ReceiveReliable,
}


const CHANNEL_OFFSET: u8 = 3;


pub(crate) struct Socket {
    socket: UdpSocket,

    in_buffer: Vec<u8>,
    out_buffer: Vec<u8>,

    max_message_size: usize,

    rng: ThreadRng,
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

            rng: thread_rng(),
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
        if self.rng.gen_bool(0.2) {return Ok(0);}

        Ok(self.socket.send_to(&self.out_buffer, addr)?)
    }

    fn receive(&mut self) -> Option<(&[u8], SocketAddr)> {
        self.socket.recv_from(&mut self.in_buffer).ok().and_then(|(received_bytes, origin)| Some((&self.in_buffer[..received_bytes], origin)))
    }

    fn heartbeat(&mut self, addr: SocketAddr, instance: &[u8; 16], time: u128) -> Result<(), Error> {
        self.clear_buffer();
        self.write(&[0])?;
        self.write(instance)?;
        self.write(&time.to_be_bytes())?;
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
        self.write(&[channel_id + CHANNEL_OFFSET])?;
        Ok(())
    }
}


pub struct Client {
    socket: Socket,

    instance: [u8; 16],

    connections: HashMap<SocketAddr, Connection>,

    config: ClientConfig,

    events: Vec<Event>,
}

impl Client {
    pub fn bind(config: ClientConfig, bind_addr: SocketAddr) -> Result<Self, Error> {
        if config.channels.len() > (u8::MAX - CHANNEL_OFFSET) as usize {
            return Err(Error::TooManyChannels);
        }

        let socket = Socket::new(config.max_message_size, bind_addr)?;

        Ok(Client {
            socket,

            instance: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_be_bytes(),

            connections: HashMap::new(),

            config,

            events: Vec::new(),
        })
    }

    pub fn bind_any(config: ClientConfig) -> Result<Self, Error> {
        Client::bind(config, "0.0.0.0:0".parse().unwrap())
    }

    pub fn connect(&mut self, addr: SocketAddr) -> Result<(), Error> {
        self.connections.insert(addr, Connection::new(&self.config, addr, &self.instance, &mut self.socket)?);

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
            let mut heartbeat_data: Option<([u8; 16], [u8; 16])> = None;
            let mut time_response = None;

            let valid_message = match message.get(0) {
                None => false,
                Some(0) => {

                    if let (Some(instance_bytes), Some(time_bytes)) = (message.get(1..17), message.get(17..33)) {
                        heartbeat_data = Some((instance_bytes.try_into().unwrap(), time_bytes.try_into().unwrap()));
                        true
                    } else {
                        false
                    }
                },
                Some(1) => {
                    if self.connections.remove(&origin).is_some() {
                        self.events.push(Event::Disconnection(origin, DisconnectReason::Other));
                    }

                    false
                },
                Some(2) => {

                    if let Some(bytes) = message.get(1..17) {
                        time_response = Some(u128::from_be_bytes(bytes.try_into().unwrap()));
                        true
                    } else {
                        false
                    }
                },
                Some(channel_id) => {
                    let channel_id = *channel_id - CHANNEL_OFFSET;
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
                            let connection = entry.insert(Connection::new(&self.config, origin, &self.instance, &mut self.socket)?);
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

                if let Some(time) = time_response {
                    let diff = connection.creation_time.elapsed().as_millis() - time;

                    if connection.ping_memory.len() >= self.config.ping_memory_length as usize {
                        connection.ping_memory.pop_front();
                    }
                    connection.ping_memory.push_back(diff);

                    connection.average_ping = connection.ping_memory.iter().fold(0, |p, &e| p + e) / connection.ping_memory.len() as u128;

                    println!("current ping is {}", connection.average_ping);
                }

                if let Some((instance, time)) = heartbeat_data {
                    match connection.other_instance {
                        None => connection.other_instance = Some(instance),
                        Some(other_instance) => if instance != other_instance {
                            self.connections.remove(&origin);
                            self.events.push(Event::Disconnection(origin, DisconnectReason::OriginChangedInstance));
                        }
                    }

                    self.socket.clear_buffer();
                    self.socket.write(&[2])?;
                    self.socket.write(&time)?;
                    self.socket.send(origin)?;
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
            connection.update(&self.instance, &mut self.socket)?;
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

    other_instance: Option<[u8; 16]>,

    creation_time: Instant,
    ping_memory: VecDeque<u128>,
    average_ping: u128,

    heartbeat_interval: u128,

    last_received_keep_alive: Instant,
    last_sent_keep_alive: Instant,

    channels: Vec<Channel>,
}

impl Connection {
    fn new(config: &ClientConfig, addr: SocketAddr, instance: &[u8; 16], socket: &mut Socket) -> Result<Self, Error> {
        let creation_time = Instant::now();

        socket.heartbeat(addr, instance, creation_time.elapsed().as_millis())?;

        Ok(Connection {
            addr,

            other_instance: None,

            creation_time,
            ping_memory: VecDeque::new(),
            average_ping: 1000,

            heartbeat_interval: config.heartbeat_interval,

            last_received_keep_alive: Instant::now(),
            last_sent_keep_alive: Instant::now(),

            channels: config.channels.iter().enumerate().map(|(id, c)| Channel::new(c, id as u8, addr)).collect(),
        })
    }

    fn update(&mut self, instance: &[u8; 16], socket: &mut Socket) -> Result<(), Error> {
        if self.last_sent_keep_alive.elapsed().as_millis() > self.heartbeat_interval {
            socket.heartbeat(self.addr, instance, self.creation_time.elapsed().as_millis())?;
            self.last_sent_keep_alive = Instant::now();
        }

        for channel in self.channels.iter_mut() {
            channel.update(self.average_ping, socket)?;
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
    OriginChangedInstance,
}


struct Channel {
    addr: SocketAddr,
    channel_id: u8,

    channel_type: ChannelType,
}

enum ChannelType {
    SendUnreliable,
    ReceiveUnreliable,

    SendReliable {
        resend_threshhold: f32,

        seq_counter: u64,

        messages_start_seq: u64,
        messages: VecDeque<Option<(Instant, Vec<u8>)>>
    },
    ReceiveReliable {
        acks_to_send: Vec<u64>,

        received_start_seq: u64,
        received: VecDeque<bool>,
    }
}

impl Channel {
    fn new(config: &ChannelConfig, channel_id: u8, addr: SocketAddr) -> Self {
        Channel {
            addr,
            channel_id,

            channel_type: match config {
                ChannelConfig::SendUnreliable => ChannelType::SendUnreliable,
                ChannelConfig::ReceiveUnreliable => ChannelType::ReceiveUnreliable,
                ChannelConfig::SendReliable { resend_threshhold } => ChannelType::SendReliable {
                    resend_threshhold: *resend_threshhold,

                    seq_counter: 0,

                    messages_start_seq: 0,
                    messages: VecDeque::new(),
                },
                ChannelConfig::ReceiveReliable => ChannelType::ReceiveReliable {
                    acks_to_send: Vec::new(),

                    received_start_seq: 0,
                    received: VecDeque::new(),
                }
            }
        }
    }

    fn send(&mut self, message: &[u8], socket: &mut Socket) -> Result<(), Error> {
        socket.channel_prefix(self.channel_id)?;

        match &mut self.channel_type {
            ChannelType::ReceiveUnreliable => return Err(Error::SendOnReceiveChannel),
            ChannelType::ReceiveReliable { .. } => return Err(Error::SendOnReceiveChannel),


            ChannelType::SendUnreliable => {
                socket.write(message)?;
                socket.send(self.addr)?;
            },


            ChannelType::SendReliable { seq_counter, messages, .. } => {
                socket.write(&seq_counter.to_be_bytes())?;
                socket.write(message)?;
                socket.send(self.addr)?;

                println!("sent seq {}", seq_counter);

                messages.push_back(Some((Instant::now(), Vec::from(message))));
                *seq_counter += 1;

            },
        }

        Ok(())
    }

    fn receive(&mut self, message: Vec<u8>, socket: &mut Socket) -> Result<Vec<Vec<u8>>, Error> {
        let _ = socket;

        Ok(match &mut self.channel_type {
            ChannelType::SendUnreliable => vec![],

            ChannelType::ReceiveUnreliable => vec![message],

            ChannelType::SendReliable { messages_start_seq, messages, .. } => 'b: {

                let Some(bytes) = message.get(..8) else {break 'b vec![];};
                let seq = u64::from_be_bytes(bytes.try_into().unwrap());

                if seq < *messages_start_seq {break 'b vec![];}

                // will fail if seq hasn't been sent
                let Some(entry) = messages.get_mut((seq - *messages_start_seq) as usize) else {break 'b vec![];};

                println!("got ack {}", seq);
                if entry.is_some() {
                    println!("marked");
                }

                // mark entry as received
                *entry = None;

                while let Some(None) = messages.front() {
                    messages.pop_front();
                    *messages_start_seq += 1;
                }

                vec![]
            },

            ChannelType::ReceiveReliable { acks_to_send, received_start_seq, received } => 'b: {
                // only return messages with sequence numbers that haven't been seen

                let Some(bytes) = message.get(..8) else {break 'b vec![];};
                let seq = u64::from_be_bytes(bytes.try_into().unwrap());

                println!("got seq {}", seq);

                acks_to_send.push(seq);

                if seq < *received_start_seq {break 'b vec![];}

                let i = (seq - *received_start_seq) as usize;
                let seen = loop {
                    match received.get_mut(i) {
                        None => received.push_back(false),
                        Some(e) => break e,
                    }
                };

                if *seen {
                    println!("seen");
                    break 'b vec![];
                }

                println!("returned");
                *seen = true;

                while let Some(false) = received.front() {
                    received.pop_front();
                    *received_start_seq += 1;
                }

                vec![Vec::from(&message[8..])]
            }
        })
    }

    fn update(&mut self, ping: u128, socket: &mut Socket) -> Result<(), Error> {
        match &mut self.channel_type {
            ChannelType::SendUnreliable => (),
            ChannelType::ReceiveUnreliable => (),

            ChannelType::SendReliable { messages, messages_start_seq, resend_threshhold, .. } => {
                let mut seq = *messages_start_seq;
                for message in messages.iter_mut() {
                    if let Some((last_sent, message)) = message {


                        if last_sent.elapsed().as_millis() as f32 > ping as f32 * *resend_threshhold {
                            socket.channel_prefix(self.channel_id)?;
                            socket.write(&seq.to_be_bytes())?;
                            socket.write(&*message)?;
                            socket.send(self.addr)?;

                            *last_sent = Instant::now();

                            println!("resent seq {}", seq);
                        }
                    }

                    seq += 1;
                }
            },

            ChannelType::ReceiveReliable { acks_to_send, .. } => {
                for seq in acks_to_send.drain(..) {
                    socket.channel_prefix(self.channel_id)?;
                    socket.write(&seq.to_be_bytes())?;
                    socket.send(self.addr)?;

                    println!("sent ack {}", seq);
                }
            }
        }

        Ok(())
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




use std::{net::{UdpSocket, SocketAddr}, time::{Instant, UNIX_EPOCH, SystemTime}, collections::{HashMap, hash_map::Entry, VecDeque}, io::Write};


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

    SendFecReliable {
        resend_threshhold: f32,
        max_data_symbols: usize,
        max_repair_symbols: usize,
    },
    ReceiveFecReliable,
}


const CHANNEL_OFFSET: u8 = 3;


pub(crate) struct Socket {
    socket: UdpSocket,

    in_buffer: Vec<u8>,
    out_buffer: Vec<u8>,

    max_message_size: usize,
}

impl Socket {
    fn new(max_message_size: u16, bind_addr: SocketAddr) -> Result<Self, Error> {
        let socket = UdpSocket::bind(bind_addr)?;

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

    fn receive(&mut self) -> Result<Option<(&[u8], SocketAddr)>, Error> {
        loop {
            match {
                self.socket.set_nonblocking(true)?;
                let result = self.socket.recv_from(&mut self.in_buffer);
                self.socket.set_nonblocking(false)?;
                result
             } {
                Err(err) => {
                    match err.kind() {
                        std::io::ErrorKind::WouldBlock => break Ok(None),
                        std::io::ErrorKind::ConnectionReset => continue,
                        _ => break Err(err.into()),
                    }
                },
                Ok((received_bytes, origin)) => {
                    break Ok(Some((&self.in_buffer[..received_bytes], origin)))
                }
            }
        }
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

    pub fn disconnect_all(&mut self) -> Result<(), Error> {
        for (addr, _) in std::mem::replace(&mut self.connections, HashMap::new()) {
            self.socket.close(addr)?;
            self.events.push(Event::Disconnection(addr, DisconnectReason::Kicked));
        }

        Ok(())
    }

    pub fn update(&mut self) -> Result<Vec<Event>, Error> {

        // receive messages
        while let Some((message, origin)) = self.socket.receive()? {

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

                    connection.average_ping = Some(connection.ping_memory.iter().fold(0, |p, &e| p + e) / connection.ping_memory.len() as u128);
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

    pub fn get_ping(&self, connection: SocketAddr) -> Result<Option<u128>, Error> {
        self.connections.get(&connection).ok_or(Error::AddressNotConnected).and_then(|connection| Ok(connection.average_ping))
    }

    pub fn connections(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.connections.keys().cloned()
    }

    pub fn bound_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.socket.socket.local_addr()?)
    }
}

pub struct Connection {
    addr: SocketAddr,

    other_instance: Option<[u8; 16]>,

    creation_time: Instant,
    ping_memory: VecDeque<u128>,
    average_ping: Option<u128>,

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
            average_ping: None,

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
    },

    SendFecReliable {
        resend_threshhold: f32,

        max_data_symbols: usize,
        max_repair_symbols: usize,

        seq_counter: u64,

        messages_start_seq: u64,
        messages: VecDeque<Option<(Instant, Vec<Option<Vec<u8>>>)>>,
    },
    ReceiveFecReliable {
        messages_start_seq: u64,
        messages: VecDeque<ReceiveFecMessage>,
    },
}

enum ReceiveFecMessage {
    NotSeen,
    Receiving {
        decoder: raptor_code::SourceBlockDecoder,
    },
    Received,
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
                },

                ChannelConfig::SendFecReliable { resend_threshhold, max_data_symbols, max_repair_symbols } => ChannelType::SendFecReliable {
                    resend_threshhold: *resend_threshhold,

                    max_data_symbols: *max_data_symbols,
                    max_repair_symbols: *max_repair_symbols,

                    seq_counter: 0,

                    messages_start_seq: 0,
                    messages: VecDeque::new(),
                },
                ChannelConfig::ReceiveFecReliable => ChannelType::ReceiveFecReliable {
                    messages_start_seq: 0,
                    messages: VecDeque::new(),
                },
            }
        }
    }

    fn send(&mut self, message: &[u8], socket: &mut Socket) -> Result<(), Error> {
        socket.channel_prefix(self.channel_id)?;

        match &mut self.channel_type {
            ChannelType::ReceiveUnreliable => return Err(Error::SendOnReceiveChannel),
            ChannelType::ReceiveReliable { .. } => return Err(Error::SendOnReceiveChannel),
            ChannelType::ReceiveFecReliable { .. } => return Err(Error::SendOnReceiveChannel),


            ChannelType::SendUnreliable => {
                socket.write(message)?;
                socket.send(self.addr)?;
            },


            ChannelType::SendReliable { seq_counter, messages, .. } => {
                socket.write(&seq_counter.to_be_bytes())?;
                socket.write(message)?;
                socket.send(self.addr)?;

                messages.push_back(Some((Instant::now(), Vec::from(message))));
                *seq_counter += 1;

            },


            ChannelType::SendFecReliable { max_data_symbols, max_repair_symbols, seq_counter, messages, .. } => {

                let (encoded_symbols, num_source_symbols) = raptor_code::encode_source_block(
                    message,
                    *max_data_symbols,
                    *max_repair_symbols,
                );

                // println!("new fec message {} with {} symbols {:?}", seq_counter, num_source_symbols as usize + *max_repair_symbols, encoded_symbols);

                let sequence = seq_counter.to_be_bytes();
                let num_source_symbols = num_source_symbols.to_be_bytes();


                let mut packets = Vec::new();

                for (encoded_symbol_index, encoded_symbol) in encoded_symbols.iter().enumerate() {
                    let mut packet = Vec::new();

                    // 8 bytes
                    packet.write(&sequence)?;
                    // 4 bytes
                    packet.write(&num_source_symbols)?;
                    // 1 byte
                    packet.write(&[encoded_symbol_index as u8])?;
                    // 2 bytes
                    packet.write(&(message.len() as u16).to_be_bytes())?;

                    packet.write(&encoded_symbol)?;

                    socket.channel_prefix(self.channel_id)?;
                    socket.write(&packet)?;
                    socket.send(self.addr)?;

                    packets.push(Some(packet));
                }

                messages.push_back(Some((
                    Instant::now(),
                    packets,
                )));
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

                acks_to_send.push(seq);

                if seq < *received_start_seq {break 'b vec![];}

                let i = (seq - *received_start_seq) as usize;
                let seen = loop {
                    match received.get_mut(i) {
                        None => received.push_back(false),
                        Some(e) => break e,
                    }
                };

                if *seen {break 'b vec![];}

                *seen = true;

                while let Some(false) = received.front() {
                    received.pop_front();
                    *received_start_seq += 1;
                }

                vec![Vec::from(&message[8..])]
            },

            ChannelType::SendFecReliable { messages_start_seq, messages, .. } => {
                match message.get(0) {
                    // whole message received acknowledgement
                    Some(0) => 'b: {
                        let Some(seq_id) = message.get(1..9) else {break 'b;};
                        let seq_id = u64::from_be_bytes(seq_id.try_into().unwrap());

                        // println!("got ack for full fec message {}", seq_id);

                        if seq_id < *messages_start_seq {break 'b;}

                        if let Some(message) = messages.get_mut((seq_id - *messages_start_seq) as usize) {
                            // mark message as received
                            *message = None;

                            // clear front of message ring buffer
                            while let Some(None) = messages.front() {
                                messages.pop_front();
                                *messages_start_seq += 1;
                            }
                        }
                    },
                    Some(1) => 'b: {
                        // single symbol/packet received acknowledgement
                        let (
                            Some(seq_id),
                            Some(symbol_index),
                         ) = (
                            message.get(1..9),
                            message.get(9)
                        ) else {break 'b;};
                        let seq_id = u64::from_be_bytes(seq_id.try_into().unwrap());

                        // println!("got ack for fec symbol {} {}", seq_id, symbol_index);

                        if seq_id < *messages_start_seq {break 'b;}

                        if let Some(message) = messages.get_mut((seq_id - *messages_start_seq) as usize) {
                            if let Some((_, symbols)) = message {
                                if let Some(symbol) = symbols.get_mut(*symbol_index as usize) {
                                    // mark packet/symbol as received
                                    *symbol = None;

                                    // mark as sent if every packet gets acknowledged
                                    if !symbols.iter().any(|e| e.is_some()) {
                                        *message = None;

                                        // clear front of message ring buffer
                                        while let Some(Some(_)) = messages.front() {
                                            messages.pop_front();
                                            *messages_start_seq += 1;
                                        }
                                    }
                                }
                            }
                        }
                    },
                    _ => (),
                }

                vec![]
            },

            ChannelType::ReceiveFecReliable { messages, messages_start_seq, .. } => 'b: {

                // get header values
                let (
                    Some(seq_id),
                    Some(num_source_symbols),
                    Some(symbol_index),
                    Some(message_length),
                ) = (
                    message.get(0..8),
                    message.get(8..12),
                    message.get(12..13),
                    message.get(13..15),
                ) else {break 'b vec![];};

                let seq_id = u64::from_be_bytes(seq_id.try_into().unwrap());
                let num_source_symbols = u32::from_be_bytes(num_source_symbols.try_into().unwrap());
                let symbol_index = u8::from_be_bytes(symbol_index.try_into().unwrap());
                let source_block_length = u16::from_be_bytes(message_length.try_into().unwrap());

                // println!("got fec symbol for sequence {} index {}", seq_id, symbol_index);

                // get the entry for the given seq_id in the receiving messages ring buffer
                if seq_id < *messages_start_seq {
                    // send ack for full message received
                    socket.channel_prefix(self.channel_id)?;
                    socket.write(&[0])?;
                    socket.write(&seq_id.to_be_bytes())?;
                    socket.send(self.addr)?;

                    break 'b vec![];
                } else {
                    // send ack for single symbol received
                    socket.channel_prefix(self.channel_id)?;
                    socket.write(&[1])?;
                    socket.write(&seq_id.to_be_bytes())?;
                    socket.write(&[symbol_index])?;
                    socket.send(self.addr)?;
                }

                let index = (seq_id - *messages_start_seq) as usize;

                let receiving_message = loop {
                    match messages.get_mut(index) {
                        None => messages.push_back(ReceiveFecMessage::NotSeen),
                        Some(message) => break message,
                    }
                };

                // if not seen yet initialize the decoder
                if let ReceiveFecMessage::NotSeen = receiving_message {
                    *receiving_message = ReceiveFecMessage::Receiving {
                        decoder: raptor_code::SourceBlockDecoder::new(num_source_symbols as usize,),
                    };
                }

                // get the decoder
                let decoder = match receiving_message {
                    ReceiveFecMessage::NotSeen => unreachable!(),
                    ReceiveFecMessage::Received => {
                        // send ack for full message received
                        socket.channel_prefix(self.channel_id)?;
                        socket.write(&[0])?;
                        socket.write(&seq_id.to_be_bytes())?;
                        socket.send(self.addr)?;

                        break 'b vec![];
                    },
                    ReceiveFecMessage::Receiving { decoder } => decoder,
                };

                // push the symbol to the decoder
                decoder.push_encoding_symbol(&message[15..], symbol_index as u32);

                // check if decoding is possible
                if decoder.fully_specified() {
                    let message = decoder.decode(source_block_length as usize).unwrap();

                    *receiving_message = ReceiveFecMessage::Received;

                    // send ack for full message received
                    socket.channel_prefix(self.channel_id)?;
                    socket.write(&[0])?;
                    socket.write(&seq_id.to_be_bytes())?;
                    socket.send(self.addr)?;

                    // clear the front of the receiving ring buffer
                    while let Some(ReceiveFecMessage::Received) = messages.front() {
                        messages.pop_front();
                        *messages_start_seq += 1;
                    }

                    vec![message]
                } else {
                    vec![]
                }
            }
        })
    }

    fn update(&mut self, ping: Option<u128>, socket: &mut Socket) -> Result<(), Error> {
        match &mut self.channel_type {
            ChannelType::SendUnreliable => (),
            ChannelType::ReceiveUnreliable => (),

            ChannelType::SendReliable { messages, messages_start_seq, resend_threshhold, .. } => {
                // only resend if ping has been calculated
                if let Some(ping) = ping {

                    let mut seq = *messages_start_seq;
                    for message in messages.iter_mut() {
                        if let Some((last_sent, message)) = message {

                            if last_sent.elapsed().as_millis() as f32 > ping as f32 * *resend_threshhold {
                                socket.channel_prefix(self.channel_id)?;
                                socket.write(&seq.to_be_bytes())?;
                                socket.write(&*message)?;
                                socket.send(self.addr)?;

                                *last_sent = Instant::now();
                            }
                        }

                        seq += 1;
                    }
                }
            },

            ChannelType::ReceiveReliable { acks_to_send, .. } => {
                for seq in acks_to_send.drain(..) {
                    socket.channel_prefix(self.channel_id)?;
                    socket.write(&seq.to_be_bytes())?;
                    socket.send(self.addr)?;
                }
            },

            ChannelType::SendFecReliable { messages, resend_threshhold, messages_start_seq, .. } => {
                // retransmit packets that have not gotten acks

                // only resend if ping has been calculated
                if let Some(ping) = ping {

                    for message in messages.iter_mut() {
                        if let Some((last_sent, symbols)) = message {

                            if last_sent.elapsed().as_millis() as f32 > ping as f32 * *resend_threshhold {
                                for symbol in symbols.iter() {
                                    if let Some(packet) = symbol {
                                        // println!("retransmitting an fec symbol");
                                        socket.channel_prefix(self.channel_id)?;
                                        socket.write(&packet)?;
                                        socket.send(self.addr)?;
                                    }
                                }

                                *last_sent = Instant::now();
                            }
                        }
                    }
                }

                // let mut n = *messages_start_seq;
                // for message in messages.iter() {
                //     if n != *messages_start_seq {
                //         print!(", ")
                //     }

                //     if message.is_some() {
                //         print!("{}", n);
                //     } else {
                //         print!("{}", "_".repeat(n.to_string().len()));
                //     }

                //     n += 1;
                // }

                // println!();

            },

            ChannelType::ReceiveFecReliable { .. } => (),
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




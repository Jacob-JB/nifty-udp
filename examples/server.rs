use udp_4::*;

fn main() {
    let mut server = Client::bind(
        ClientConfig {
            max_message_size: 65443,

            heartbeat_interval: 100,
            timeout: 10000,

            listen: true,

            channels: vec![
                ChannelConfig::ReceiveUnreliable,
                ChannelConfig::SendUnreliable,
            ],
        },
        "0.0.0.0:3000".parse().unwrap()
    ).unwrap();

    loop {
        for event in server.update().unwrap() {
            match event {
                Event::Connection(addr) => println!("connection {}", addr),
                Event::Disconnection(addr, reason) => println!("disconnected {} {:?}", addr, reason),
                Event::Message(addr, channel_id, message) => println!("message from {} on channel {} {:?}", addr, channel_id, std::str::from_utf8(&message).unwrap()),
            }
        }
    }
}

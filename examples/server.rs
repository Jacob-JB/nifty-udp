use udp_4::*;

fn main() {
    let mut server = Client::bind(
        ClientConfig {
            max_message_size: 65443,

            heartbeat_interval: 100,
            timeout: 10000,
            ping_memory_length: 16,

            listen: true,

            channels: vec![
                ChannelConfig::ReceiveUnreliable,
                ChannelConfig::SendUnreliable,

                ChannelConfig::ReceiveReliable,
                ChannelConfig::SendReliable {
                    resend_threshhold: 1.25
                },
            ],
        },
        "0.0.0.0:3000".parse().unwrap()
    ).unwrap();

    loop {
        for event in server.update().unwrap() {
            match event {
                Event::Connection(addr) => println!("connection {}", addr),
                Event::Disconnection(addr, reason) => println!("disconnected {} {:?}", addr, reason),
                Event::Message(addr, channel_id, message) => {
                    println!("message from {} on channel {} {:?}", addr, channel_id, std::str::from_utf8(&message).unwrap());

                    server.send(addr, 3, "Pong".as_bytes()).unwrap();
                },
            }
        }
    }
}

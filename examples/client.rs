use std::time::Instant;

use udp_4::*;


fn main() {
    let mut client = Client::bind_any(
        ClientConfig {
            max_message_size: 65443,

            heartbeat_interval: 100,
            timeout: 10000,

            listen: false,

            channels: vec![
                ChannelConfig::SendUnreliable,
                ChannelConfig::ReceiveUnreliable,
            ],
        }
    ).unwrap();

    client.connect("10.176.80.83:3000".parse().unwrap()).unwrap();

    let mut last_ping = Instant::now();

    loop {
        for event in client.update().unwrap() {
            match event {
                Event::Connection(addr) => println!("connection {}", addr),
                Event::Disconnection(addr, reason) => println!("disconnected {} {:?}", addr, reason),
                Event::Message(addr, channel_id, message) => println!("message from {} on channel {} {:?}", addr, channel_id, message),
            }
        }

        if last_ping.elapsed().as_millis() > 2000 {
            client.send_single(0, "Ping".as_bytes()).unwrap();
            last_ping = Instant::now();
        }
    }
}
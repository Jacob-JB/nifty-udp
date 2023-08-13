use std::time::Instant;

use nifty_udp::*;


fn main() {
    let mut client = Client::bind_any(
        ClientConfig {
            max_message_size: 65443,

            heartbeat_interval: 100,
            timeout: 10000,
            ping_memory_length: 16,

            listen: false,

            channels: vec![
                ChannelConfig::SendUnreliable,
                ChannelConfig::ReceiveUnreliable,

                ChannelConfig::SendReliable {
                    resend_threshhold: 1.25
                },
                ChannelConfig::ReceiveReliable,
            ],
        }
    ).unwrap();

    client.connect("10.0.20.248:3000".parse().unwrap()).unwrap();

    let mut last_ping = Instant::now();

    loop {
        for event in client.update().unwrap() {
            match event {
                Event::Connection(addr) => println!("connection {}", addr),
                Event::Disconnection(addr, reason) => println!("disconnected {} {:?}", addr, reason),
                Event::Message(addr, channel_id, message) => {
                    println!("message from {} on channel {} {:?}", addr, channel_id, message);

                    client.disconnect_all();
                },
            }
        }

        if last_ping.elapsed().as_millis() > 3000 {
            client.send_single(2, "Ping".as_bytes()).unwrap();
            last_ping = Instant::now();
            for i in client.connections() {
                println!("current ping for {} is {:?}", i, client.get_ping(i).unwrap());
            }
        }
    }
}
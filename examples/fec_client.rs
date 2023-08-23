
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
                ChannelConfig::SendFecReliable {
                    resend_threshhold: 1.25,
                    max_data_symbols: 4,
                    max_repair_symbols: 3,
                },
            ],
        }
    ).unwrap();

    client.connect("10.176.82.194:3001".parse().unwrap()).unwrap();

    let mut last_ping = Instant::now();

    loop {
        for event in client.update().unwrap() {
            match event {
                Event::Connection(addr) => println!("connection {}", addr),
                Event::Disconnection(addr, reason) => println!("disconnected {} {:?}", addr, reason),
                Event::Message(_, _, _) => (),
            }
        }


        if last_ping.elapsed().as_millis() > 4000 {
            client.send_single(0, "this is an fec ping".as_bytes()).unwrap();
            println!("pinged");

            last_ping = Instant::now();
        }
    }
}
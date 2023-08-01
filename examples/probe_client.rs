
use udp_4::*;

fn main() {
    let mut client = Client::bind_any(ClientConfig {
            max_message_size: 65443,

            heartbeat_interval: 100,
            timeout: 10000,
            ping_memory_length: 16,

            listen: true,

            channels: vec![],
    }).unwrap();

    println!("bound to {}", client.bound_addr().unwrap());

    let mut addresses = std::env::args();
    addresses.next();

    for addr in addresses.map(
        |text| {
            if let Ok(addr) = text.parse() {
                addr
            } else {
                panic!("{} is not a valid address", text);
            }
        }
    ) {
        client.connect(addr).unwrap();
    }

    loop {
        for event in client.update().unwrap() {
            match event {
                Event::Connection(addr) => println!("connection {}", addr),
                Event::Disconnection(addr, reason) => println!("disconnected {} {:?}", addr, reason),
                _ => (),
            }
        }
    }
}

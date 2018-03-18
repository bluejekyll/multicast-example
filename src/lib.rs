// Copyright 2018 Benjamin Fry <benjaminfry@me.com>
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

#[macro_use]
extern crate lazy_static;
extern crate socket2;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Barrier};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

pub const PORT: u16 = 7645;
lazy_static! {
    pub static ref IPV4: IpAddr = Ipv4Addr::new(224, 0, 0, 123).into();
    pub static ref IPV6: IpAddr = Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 0x0123).into();
}

// this will be common for all our sockets
fn new_socket(addr: &SocketAddr) -> io::Result<Socket> {
    let domain = if addr.is_ipv4() {
        Domain::ipv4()
    } else {
        Domain::ipv6()
    };

    let socket = Socket::new(domain, Type::dgram(), Some(Protocol::udp()))?;

    // we're going to use read timeouts so that we don't hang waiting for packets
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;

    Ok(socket)
}

/// On Windows, unlike all Unix variants, it is improper to bind to the multicast address
///
/// see https://msdn.microsoft.com/en-us/library/windows/desktop/ms737550(v=vs.85).aspx
#[cfg(windows)]
fn bind_multicast(socket: &Socket, addr: &SocketAddr) -> io::Result<()> {
    let addr = match *addr {
        SocketAddr::V4(addr) => SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), addr.port()),
        SocketAddr::V6(addr) => {
            SocketAddr::new(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0).into(), addr.port())
        }
    };
    socket.bind(&socket2::SockAddr::from(addr))
}

/// On unixes we bind to the multicast address, which causes multicast packets to be filtered
#[cfg(unix)]
fn bind_multicast(socket: &Socket, addr: &SocketAddr) -> io::Result<()> {
    socket.bind(&socket2::SockAddr::from(*addr))
}

fn join_multicast(addr: SocketAddr) -> io::Result<UdpSocket> {
    let ip_addr = addr.ip();

    let socket = new_socket(&addr)?;

    // depending on the IP protocol we have slightly different work
    match ip_addr {
        IpAddr::V4(ref mdns_v4) => {
            // join to the multicast address, with all interfaces
            socket.join_multicast_v4(mdns_v4, &Ipv4Addr::new(0, 0, 0, 0))?;
        }
        IpAddr::V6(ref mdns_v6) => {
            // join to the multicast address, with all interfaces (ipv6 uses indexes not addresses)
            socket.join_multicast_v6(mdns_v6, 0)?;
            socket.set_only_v6(true)?;
        }
    };

    // bind us to the socket address.
    bind_multicast(&socket, &addr)?;

    // convert to standard sockets
    Ok(socket.into_udp_socket())
}

fn multicast_listener(
    response: &'static str,
    client_done: Arc<AtomicBool>,
    addr: SocketAddr,
) -> JoinHandle<()> {
    // A barrier to not start the client test code until after the server is running
    let server_barrier = Arc::new(Barrier::new(2));
    let client_barrier = Arc::clone(&server_barrier);

    let join_handle = std::thread::Builder::new()
        .name(format!("{}:server", response))
        .spawn(move || {
            // socket creation will go here...
            let listener = join_multicast(addr).expect("failed to create listener");
            println!("{}:server: joined: {}", response, addr);

            server_barrier.wait();
            println!("{}:server: is ready", response);

            // We'll be looping until the client indicates it is done.
            while !client_done.load(std::sync::atomic::Ordering::Relaxed) {
                // test receive and response code will go here...
                let mut buf = [0u8; 64]; // receive buffer

                // we're assuming failures were timeouts, the client_done loop will stop us
                match listener.recv_from(&mut buf) {
                    Ok((len, remote_addr)) => {
                        let data = &buf[..len];

                        println!(
                            "{}:server: got data: {} from: {}",
                            response,
                            String::from_utf8_lossy(data),
                            remote_addr
                        );

                        // create a socket to send the response
                        let responder = new_socket(&remote_addr)
                            .expect("failed to create responder")
                            .into_udp_socket();

                        // we send the response that was set at the method beginning
                        responder
                            .send_to(response.as_bytes(), &remote_addr)
                            .expect("failed to respond");

                        println!("{}:server: sent response to: {}", response, remote_addr);
                    }
                    Err(err) => {
                        println!("{}:server: got an error: {}", response, err);
                    }
                }
            }

            println!("{}:server: client is done", response);
        })
        .unwrap();

    client_barrier.wait();
    join_handle
}

fn new_sender(addr: &SocketAddr) -> io::Result<UdpSocket> {
    let socket = new_socket(addr)?;

    if addr.is_ipv4() {
        socket.set_multicast_if_v4(&Ipv4Addr::new(0, 0, 0, 0))?;

        socket.bind(&SockAddr::from(SocketAddr::new(
            Ipv4Addr::new(0, 0, 0, 0).into(),
            0,
        )))?;
    } else {
        // *WARNING* THIS IS SPECIFIC TO THE AUTHORS COMPUTER
        //   find the index of your IPv6 interface you'd like to test with.
        socket.set_multicast_if_v6(5)?;

        socket.bind(&SockAddr::from(SocketAddr::new(
            Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0).into(),
            0,
        )))?;
    }

    // convert to standard sockets...
    Ok(socket.into_udp_socket())
}

/// This will guarantee we always tell the server to stop
struct NotifyServer(Arc<AtomicBool>);
impl Drop for NotifyServer {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Our generic test over different IPs
fn test_multicast(test: &'static str, addr: IpAddr) {
    assert!(addr.is_multicast());
    let addr = SocketAddr::new(addr, PORT);

    let client_done = Arc::new(AtomicBool::new(false));
    let notify = NotifyServer(Arc::clone(&client_done));

    multicast_listener(test, client_done, addr);

    // client test code send and receive code after here
    println!("{}:client: running", test);

    let message = b"Hello from client!";

    // create the sending socket
    let socket = new_sender(&addr).expect("could not create sender!");
    socket.send_to(message, &addr).expect("could not send_to!");

    let mut buf = [0u8; 64]; // receive buffer

    match socket.recv_from(&mut buf) {
        Ok((len, remote_addr)) => {
            let data = &buf[..len];
            let response = String::from_utf8_lossy(data);

            println!("{}:client: got data: {}", test, response);

            // verify it's what we expected
            assert_eq!(test, response);
        }
        Err(err) => {
            println!("{}:client: had a problem: {}", test, err);
            assert!(false);
        }
    }

    // make sure we don't notify the server until the end of the client test
    drop(notify);
}

#[test]
fn test_ipv4_multicast() {
    test_multicast("ipv4", *IPV4);
}

#[test]
fn test_ipv6_multicast() {
    test_multicast("ipv6", *IPV6);
}

//! The DAP client (design §5.5) arrives with S5. What is here now is the one network rule it
//! needs, from decision 12: the core may connect, never listen, to a debug adapter's port on
//! 127.0.0.1 or ::1, and to nothing else. The check is in code, before any socket exists.

// The DAP client of S5 calls it; the tests do now.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Connect to `host:port` if `host` is the literal 127.0.0.1 or ::1. Names are refused too: a name
/// could resolve anywhere, and resolving one is a socket of its own.
pub fn connect_loopback(host: &str, port: u16, wait: Duration) -> Result<TcpStream> {
    let ip = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok();
    let Some(ip) = ip.filter(|ip| *ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || *ip == IpAddr::V6(Ipv6Addr::LOCALHOST)) else {
        bail!("{host} is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only");
    };
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), wait).with_context(|| format!("connecting to {host} port {port}"))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn a_loopback_adapter_port_is_connected() {
        // The test listens, standing in for an adapter; the core never does.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let mut buf = [0u8; 4];
            listener.accept().unwrap().0.read_exact(&mut buf).unwrap();
            buf
        });
        connect_loopback("127.0.0.1", port, Duration::from_secs(2)).unwrap().write_all(b"ping").unwrap();
        assert_eq!(&server.join().unwrap(), b"ping");
        if let Ok(v6) = TcpListener::bind("[::1]:0") {
            let port = v6.local_addr().unwrap().port();
            assert!(connect_loopback("[::1]", port, Duration::from_secs(2)).is_ok());
            assert!(connect_loopback("::1", port, Duration::from_secs(2)).is_ok());
        }
    }

    #[test]
    fn a_non_loopback_address_is_refused() {
        for host in ["10.0.0.1", "127.0.0.2", "0.0.0.0", "::", "::ffff:127.0.0.1", "localhost", "example.com", "", "192.168.1.1"] {
            let e = connect_loopback(host, 5005, Duration::from_millis(100)).unwrap_err().to_string();
            assert_eq!(e, format!("{host} is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only"));
        }
    }

    #[test]
    fn a_closed_loopback_port_is_an_error_that_names_it() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = connect_loopback("127.0.0.1", port, Duration::from_secs(1)).unwrap_err().to_string();
        assert_eq!(e, format!("connecting to 127.0.0.1 port {port}"));
    }
}

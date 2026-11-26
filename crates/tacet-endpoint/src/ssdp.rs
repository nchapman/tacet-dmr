//! Unified dual-device SSDP announcer & `M-SEARCH` responder backed by
//! `openhome_core::ssdp`.
//!
//! Advertises both the OpenHome `Source:1` device and (when enabled) the
//! minimal UPnP AV `MediaRenderer:1` device over a single UDP multicast socket
//! so each virtual device keeps its own UDN and `LOCATION` URL cleanly on all
//! platforms (macOS, Linux, Windows).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use openhome_core::ssdp::{
    SSDP_ADDR_V4, SsdpMessage, decode_ssdp_packet, encode_notify_alive, encode_notify_byebye,
    encode_search_response, format_usn, search_target_matches,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;

const MAX_AGE_SECS: u32 = 1800;

/// One virtual UPnP root device advertised via SSDP.
#[derive(Debug, Clone)]
pub struct SsdpDevice {
    pub udn: String,
    pub location: String,
    pub targets: Vec<String>,
}

/// Run the SSDP advertiser & `M-SEARCH` responder for `devices` until `shutdown_rx` becomes true.
pub async fn run_ssdp(
    devices: Vec<SsdpDevice>,
    advertise_ip: Ipv4Addr,
    server_header: String,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    if devices.is_empty() {
        return Ok(());
    }
    let sock = build_multicast_socket(advertise_ip)?;
    let sock = UdpSocket::from_std(sock).map_err(|e| format!("tokio UdpSocket: {e}"))?;
    let mcast_dst = SocketAddr::V4(SSDP_ADDR_V4);
    let host_header = mcast_dst.to_string();

    send_alive_all(&sock, mcast_dst, &host_header, &devices, &server_header).await;

    let mut tick = tokio::time::interval(Duration::from_secs(u64::from(MAX_AGE_SECS) / 3));
    tick.tick().await; // consume immediate tick

    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = tick.tick() => {
                send_alive_all(&sock, mcast_dst, &host_header, &devices, &server_header).await;
            }
            recv = sock.recv_from(&mut buf) => {
                if let Ok((len, peer)) = recv
                    && let Ok(SsdpMessage::MSearch(search)) = decode_ssdp_packet(&buf[..len])
                    && search.man.as_deref().is_some_and(|m| {
                        m.split(';')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .trim_matches('"')
                            .eq_ignore_ascii_case("ssdp:discover")
                    })
                {
                    for dev in &devices {
                        for target in &dev.targets {
                            if search_target_matches(&search.st, target) {
                                let usn = format_usn(&dev.udn, target);
                                let resp = encode_search_response(
                                    &dev.location,
                                    target,
                                    &usn,
                                    &server_header,
                                    MAX_AGE_SECS,
                                );
                                let _ = sock.send_to(resp.as_bytes(), peer).await;
                            }
                        }
                    }
                }
            }
        }
    }

    send_byebye_all(&sock, mcast_dst, &host_header, &devices).await;
    Ok(())
}

async fn send_alive_all(
    sock: &UdpSocket,
    mcast_dst: SocketAddr,
    host_header: &str,
    devices: &[SsdpDevice],
    server_header: &str,
) {
    for dev in devices {
        for target in &dev.targets {
            let usn = format_usn(&dev.udn, target);
            let msg = encode_notify_alive(
                host_header,
                &dev.location,
                target,
                &usn,
                server_header,
                MAX_AGE_SECS,
            );
            let _ = sock.send_to(msg.as_bytes(), mcast_dst).await;
        }
    }
}

async fn send_byebye_all(
    sock: &UdpSocket,
    mcast_dst: SocketAddr,
    host_header: &str,
    devices: &[SsdpDevice],
) {
    for dev in devices {
        for target in &dev.targets {
            let usn = format_usn(&dev.udn, target);
            let msg = encode_notify_byebye(host_header, target, &usn);
            let _ = sock.send_to(msg.as_bytes(), mcast_dst).await;
        }
    }
}

fn build_multicast_socket(advertise_ip: Ipv4Addr) -> Result<std::net::UdpSocket, String> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("create SSDP socket: {e}"))?;
    sock.set_reuse_address(true)
        .map_err(|e| format!("SO_REUSEADDR: {e}"))?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    {
        sock.set_reuse_port(true)
            .map_err(|e| format!("SO_REUSEPORT: {e}"))?;
    }

    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_ADDR_V4.port());
    sock.bind(&SocketAddr::V4(bind_addr).into())
        .map_err(|e| format!("bind SSDP :1900: {e}"))?;
    sock.join_multicast_v4(SSDP_ADDR_V4.ip(), &advertise_ip)
        .map_err(|e| format!("join SSDP multicast on {advertise_ip}: {e}"))?;
    let _ = sock.set_multicast_loop_v4(true);
    let _ = sock.set_multicast_ttl_v4(2);
    if !advertise_ip.is_unspecified() {
        let _ = sock.set_multicast_if_v4(&advertise_ip);
    }
    sock.set_nonblocking(true)
        .map_err(|e| format!("SSDP nonblocking: {e}"))?;
    Ok(sock.into())
}

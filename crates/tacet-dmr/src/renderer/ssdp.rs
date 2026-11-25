//! Native UDP multicast SSDP advertiser and `M-SEARCH` responder (UPnP UDA 1.0).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;

use crate::renderer::schema::{DEVICE_TYPE, SERVICES, service_type};

const SSDP_MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const MAX_AGE_SECS: u32 = 1800;

/// Spawn SSDP advertisement + `M-SEARCH` responder until `shutdown_rx` becomes true.
pub async fn run_ssdp(
    udn: String,
    location: String,
    advertise_ip: Ipv4Addr,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let targets = discovery_targets(&udn);
    let server_hdr = format!(
        "{}/1.0 UPnP/1.0 tacet-dmr/{}",
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION")
    );

    let sock = build_multicast_socket(advertise_ip)?;
    let sock = UdpSocket::from_std(sock).map_err(|e| format!("tokio UdpSocket: {e}"))?;
    let mcast_dst = SocketAddr::V4(SocketAddrV4::new(SSDP_MULTICAST_V4, SSDP_PORT));

    // Send initial ssdp:alive burst.
    send_notify_all(
        &sock,
        mcast_dst,
        &targets,
        &udn,
        &location,
        &server_hdr,
        "ssdp:alive",
    )
    .await;

    let mut announce_tick = tokio::time::interval(Duration::from_secs(MAX_AGE_SECS as u64 / 3));
    announce_tick.tick().await; // consume immediate tick

    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = announce_tick.tick() => {
                send_notify_all(&sock, mcast_dst, &targets, &udn, &location, &server_hdr, "ssdp:alive").await;
            }
            res = sock.recv_from(&mut buf) => {
                if let Ok((n, peer)) = res
                    && let Ok(text) = std::str::from_utf8(&buf[..n])
                    && let Some(st) = parse_msearch_st(text)
                {
                    for (nt, usn) in &targets {
                        if st == "ssdp:all" || st.eq_ignore_ascii_case(nt) {
                            let resp = format_msearch_response(nt, usn, &location, &server_hdr);
                            let _ = sock.send_to(resp.as_bytes(), peer).await;
                        }
                    }
                }
            }
        }
    }

    // Send clean ssdp:byebye burst on shutdown.
    send_notify_all(
        &sock,
        mcast_dst,
        &targets,
        &udn,
        &location,
        &server_hdr,
        "ssdp:byebye",
    )
    .await;
    Ok(())
}

fn discovery_targets(udn: &str) -> Vec<(String, String)> {
    let mut out = vec![
        ("upnp:rootdevice".into(), format!("{udn}::upnp:rootdevice")),
        (udn.to_string(), udn.to_string()),
        (DEVICE_TYPE.into(), format!("{udn}::{DEVICE_TYPE}")),
    ];
    for &s in SERVICES {
        let st = service_type(s);
        let usn = format!("{udn}::{st}");
        out.push((st, usn));
    }
    out
}

fn build_multicast_socket(iface: Ipv4Addr) -> Result<std::net::UdpSocket, String> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("create SSDP socket: {e}"))?;
    sock.set_reuse_address(true)
        .map_err(|e| format!("set SO_REUSEADDR: {e}"))?;
    #[cfg(unix)]
    {
        let _ = sock.set_reuse_port(true);
    }
    sock.set_nonblocking(true)
        .map_err(|e| format!("set nonblocking: {e}"))?;

    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT);
    sock.bind(&bind_addr.into())
        .map_err(|e| format!("bind SSDP :1900: {e}"))?;

    let _ = sock.set_multicast_loop_v4(true);
    let _ = sock.set_multicast_ttl_v4(4);
    let _ = sock.set_multicast_if_v4(&iface);
    if sock.join_multicast_v4(&SSDP_MULTICAST_V4, &iface).is_err() && iface != Ipv4Addr::UNSPECIFIED
    {
        let _ = sock.join_multicast_v4(&SSDP_MULTICAST_V4, &Ipv4Addr::UNSPECIFIED);
    }

    Ok(sock.into())
}

async fn send_notify_all(
    sock: &UdpSocket,
    dst: SocketAddr,
    targets: &[(String, String)],
    _udn: &str,
    location: &str,
    server: &str,
    nts: &str,
) {
    for (nt, usn) in targets {
        let msg = if nts == "ssdp:byebye" {
            format!(
                "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: {nt}\r\nNTS: ssdp:byebye\r\nUSN: {usn}\r\n\r\n"
            )
        } else {
            format!(
                "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age={MAX_AGE_SECS}\r\nLOCATION: {location}\r\nNT: {nt}\r\nNTS: ssdp:alive\r\nSERVER: {server}\r\nUSN: {usn}\r\n\r\n"
            )
        };
        let _ = sock.send_to(msg.as_bytes(), dst).await;
    }
}

fn format_msearch_response(st: &str, usn: &str, location: &str, server: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE_SECS}\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: {server}\r\nST: {st}\r\nUSN: {usn}\r\n\r\n"
    )
}

fn parse_msearch_st(msg: &str) -> Option<&str> {
    let mut lines = msg.lines();
    let first = lines.next()?.trim();
    if !first.starts_with("M-SEARCH ") {
        return None;
    }
    let mut has_discover = false;
    let mut st = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("MAN") && v.eq_ignore_ascii_case("\"ssdp:discover\"") {
                has_discover = true;
            } else if k.eq_ignore_ascii_case("ST") {
                st = Some(v);
            }
        }
    }
    if has_discover { st } else { None }
}

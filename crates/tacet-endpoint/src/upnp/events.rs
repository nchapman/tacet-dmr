//! UPnP AV GENA eventing (`SUBSCRIBE`, `UNSUBSCRIBE`, `NOTIFY`) and `<LastChange>` XML.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::Method;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::upnp::schema::{AV, CM, RC, escape, sink_protocols, tag};

const EVENT_NS: &str = "urn:schemas-upnp-org:event-1-0";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventState {
    pub state: String,
    pub status: String,
    pub uri: String,
    pub mode: String,
    pub next: String,
    pub duration_sec: u64,
    pub track: usize,
    pub volume: u8,
    pub mute: bool,
}

impl EventState {
    pub fn body(&self, service: &str) -> String {
        let props = match service {
            CM => {
                let protos = sink_protocols();
                format!(
                    "<e:property>{}</e:property><e:property>{}</e:property><e:property>{}</e:property>",
                    tag("SourceProtocolInfo", ""),
                    tag("SinkProtocolInfo", &protos),
                    tag("CurrentConnectionIDs", "0")
                )
            }
            RC => {
                let mute = if self.mute { "1" } else { "0" };
                let inner = format!(
                    r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0"><Volume channel="Master" val="{}"/><Mute channel="Master" val="{mute}"/></InstanceID></Event>"#,
                    self.volume
                );
                format!("<e:property>{}</e:property>", tag("LastChange", &inner))
            }
            _ => {
                let dur = format_timestamp(self.duration_sec);
                let inner = format!(
                    r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="{}"/><TransportStatus val="{}"/><CurrentTrackURI val="{}"/><CurrentTrackDuration val="{dur}"/><CurrentTrack val="{}"/><CurrentPlayMode val="{}"/><NextAVTransportURI val="{}"/></InstanceID></Event>"#,
                    self.state,
                    self.status,
                    escape(&self.uri),
                    self.track,
                    self.mode,
                    escape(&self.next)
                );
                format!("<e:property>{}</e:property>", tag("LastChange", &inner))
            }
        };
        format!(
            r#"<?xml version="1.0"?><e:propertyset xmlns:e="{EVENT_NS}">{props}</e:propertyset>"#
        )
    }

    pub fn changed_for(&self, service: &str, old: &EventState) -> bool {
        match service {
            CM => false,
            RC => self.volume != old.volume || self.mute != old.mute,
            AV => {
                self.state != old.state
                    || self.uri != old.uri
                    || self.duration_sec != old.duration_sec
                    || self.status != old.status
                    || self.track != old.track
                    || self.mode != old.mode
                    || self.next != old.next
            }
            _ => false,
        }
    }
}

pub fn format_timestamp(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h}:{m:02}:{s:02}")
}

pub fn parse_timestamp(raw: &str) -> Option<f64> {
    let parts: Vec<&str> = raw.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let (sec_str, frac) = match parts[2].split_once('.') {
        Some((s, f)) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let frac: f64 = format!("0.{f}").parse().ok()?;
            (s, frac)
        }
        None => (parts[2], 0.0),
    };
    let s: u64 = sec_str.parse().ok()?;
    if m >= 60 || s >= 60 {
        return None;
    }
    Some((h * 3600 + m * 60 + s) as f64 + frac)
}

pub fn parse_timeout_seconds(header: Option<&str>) -> u64 {
    if let Some(raw) = header {
        let lower = raw.trim().to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("second-")
            && let Ok(v) = rest.trim().parse::<u64>()
            && v > 0
            && v < 300
        {
            return v;
        }
    }
    300
}

/// Parse UPnP GENA `CALLBACK` header (`<http://host:port/path>...`) and verify
/// the callback host matches the requesting peer's IP address.
pub fn parse_callback_url(raw: &str, peer: IpAddr) -> Option<String> {
    let mut s = raw.trim();
    if !s.starts_with('<') || !s.ends_with('>') {
        return None;
    }
    while !s.is_empty() {
        s = s.trim();
        if !s.starts_with('<') {
            return None;
        }
        let end = s.find('>')?;
        if end <= 1 {
            return None;
        }
        let token = &s[1..end];
        if token.contains('<') {
            return None;
        }
        s = s[end + 1..].trim();

        let uri: hyper::Uri = match token.parse() {
            Ok(u) => u,
            Err(_) => continue,
        };
        if uri.scheme_str() != Some("http") {
            continue;
        }
        let Some(host) = uri.host() else {
            continue;
        };
        let host_clean = host.trim_start_matches('[').trim_end_matches(']');
        let Ok(ip) = host_clean.parse::<IpAddr>() else {
            continue;
        };
        if normalize_ip(ip) != normalize_ip(peer) {
            continue;
        }
        return Some(token.to_string());
    }
    None
}

pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

pub struct Subscription {
    pub sid: String,
    pub service: String,
    pub callback: String,
    pub peer: IpAddr,
    pub expires: Instant,
    pub tx: watch::Sender<EventState>,
    pub stop_tx: watch::Sender<bool>,
}

pub struct SubRegistry {
    pub subs: HashMap<String, Subscription>,
    pub last: EventState,
}

impl SubRegistry {
    pub fn new(initial: EventState) -> Self {
        Self {
            subs: HashMap::new(),
            last: initial,
        }
    }

    pub fn has_rc_subs(&self) -> bool {
        self.subs.values().any(|s| s.service == RC)
    }

    pub fn expire_stale(&mut self, now: Instant) {
        self.subs.retain(|_, s| {
            if now >= s.expires {
                let _ = s.stop_tx.send(true);
                false
            } else {
                true
            }
        });
    }

    pub fn publish(&mut self, v: EventState) {
        if v == self.last {
            return;
        }
        let old = std::mem::replace(&mut self.last, v.clone());
        for s in self.subs.values() {
            if v.changed_for(&s.service, &old) {
                let _ = s.tx.send(v.clone());
            }
        }
    }

    pub fn close_all(&mut self) {
        for (_, s) in self.subs.drain() {
            let _ = s.stop_tx.send(true);
        }
    }
}

/// Spawn a background task delivering GENA `NOTIFY` events (starting at SEQ=0
/// for `initial`, then latest snapshots from `rx` until stopped or delivery fails).
pub fn spawn_notifier(
    sid: String,
    service: String,
    callback: String,
    initial: EventState,
    mut rx: watch::Receiver<EventState>,
    mut stop_rx: watch::Receiver<bool>,
    on_drop: impl FnOnce(String) + Send + 'static,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        let mut seq: u32 = 0;

        let delivered = tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    return;
                }
                true
            }
            ok = send_notify(&callback, &sid, &service, seq, &initial) => ok,
        };
        if !delivered {
            on_drop(sid);
            return;
        }
        seq = next_seq(seq);

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                }
                res = rx.changed() => {
                    if res.is_err() {
                        break;
                    }
                    let snap = rx.borrow_and_update().clone();
                    let ok = tokio::select! {
                        _ = stop_rx.changed() => {
                            if *stop_rx.borrow() {
                                break;
                            }
                            true
                        }
                        ok = send_notify(&callback, &sid, &service, seq, &snap) => ok,
                    };
                    if !ok {
                        on_drop(sid);
                        return;
                    }
                    seq = next_seq(seq);
                }
            }
        }
    });
}

fn next_seq(seq: u32) -> u32 {
    if seq == u32::MAX { 1 } else { seq + 1 }
}

async fn send_notify(
    callback: &str,
    sid: &str,
    service: &str,
    seq: u32,
    state: &EventState,
) -> bool {
    let Ok(uri) = callback.parse::<hyper::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let port = uri.port_u16().unwrap_or(80);
    let addr = format!("{host}:{port}");
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let host_header = if uri.port_u16().is_some() {
        format!("{host}:{port}")
    } else {
        host.to_string()
    };

    let body = Bytes::from(state.body(service));
    let Ok(req) = hyper::Request::builder()
        .method(Method::from_bytes(b"NOTIFY").unwrap())
        .uri(path)
        .header("HOST", host_header)
        .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
        .header("CONTENT-LENGTH", body.len().to_string())
        .header("NT", "upnp:event")
        .header("NTS", "upnp:propchange")
        .header("SID", sid)
        .header("SEQ", seq.to_string())
        .body(Full::new(body))
    else {
        return false;
    };

    let io_fut = async {
        let stream = TcpStream::connect(&addr).await.ok()?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let resp = sender.send_request(req).await.ok()?;
        Some(resp.status().is_success())
    };

    matches!(
        tokio::time::timeout(Duration::from_secs(3), io_fut).await,
        Ok(Some(true))
    )
}

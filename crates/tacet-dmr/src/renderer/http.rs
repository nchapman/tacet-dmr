//! HTTP/1.1 UPnP control, description, and GENA eventing server (`hyper`).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use quick_xml::Reader;
use quick_xml::events::Event;
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::renderer::browse::format_browse_soap;
use crate::renderer::events::{
    SubRegistry, Subscription, normalize_ip, parse_callback_url, parse_timeout_seconds,
    spawn_notifier,
};
use crate::renderer::schema::{
    Fault, PQ, SERVICES, device_xml, find_action, scpd_xml, service_type, soap_response_xml,
};
use crate::renderer::transport::{Fields, Transport};

const MAX_SOAP_BODY: usize = 8 << 20;

/// IP / CIDR allowlist for restricting which control points may access the renderer.
#[derive(Debug, Clone, Default)]
pub struct ControllerFilter {
    rules: Vec<IpRule>,
}

#[derive(Debug, Clone, Copy)]
enum IpRule {
    Exact(IpAddr),
    V4Prefix { net: u32, mask: u32 },
}

impl ControllerFilter {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Self::default());
        }
        let mut rules = Vec::new();
        for part in raw.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            if let Some((ip_s, bits_s)) = p.split_once('/') {
                let ip: Ipv4Addr = ip_s
                    .parse()
                    .map_err(|_| format!("invalid IPv4 CIDR {p:?}"))?;
                let bits: u32 = bits_s
                    .parse()
                    .map_err(|_| format!("invalid CIDR prefix {p:?}"))?;
                if bits > 32 {
                    return Err(format!("invalid CIDR prefix {p:?}"));
                }
                let mask = if bits == 0 { 0 } else { !0u32 << (32 - bits) };
                rules.push(IpRule::V4Prefix {
                    net: u32::from(ip) & mask,
                    mask,
                });
            } else {
                let ip: IpAddr = p
                    .parse()
                    .map_err(|_| format!("invalid controller IP {p:?}"))?;
                rules.push(IpRule::Exact(normalize_ip(ip)));
            }
        }
        Ok(Self { rules })
    }

    pub fn allows(&self, peer: IpAddr) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        let peer = normalize_ip(peer);
        for rule in &self.rules {
            match (*rule, peer) {
                (IpRule::Exact(ip), p) if ip == p => return true,
                (IpRule::V4Prefix { net, mask }, IpAddr::V4(v4))
                    if (u32::from(v4) & mask) == net =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

pub struct DeviceState {
    pub transport: Transport,
    pub subs: SubRegistry,
}

pub struct Device {
    pub name: String,
    pub udn: String,
    pub device_doc: String,
    pub scpd_docs: HashMap<&'static str, String>,
    pub filter: ControllerFilter,
    pub state: Mutex<DeviceState>,
    pub wake: Notify,
}

impl Device {
    pub fn new(
        name: String,
        udn: String,
        filter: ControllerFilter,
        mut transport: Transport,
    ) -> Arc<Self> {
        let initial = transport.event_state();
        let device_doc = device_xml(&name, &udn);
        let mut scpd_docs = HashMap::new();
        for &s in SERVICES {
            scpd_docs.insert(s, scpd_xml(s));
        }
        Arc::new(Self {
            name,
            udn,
            device_doc,
            scpd_docs,
            filter,
            state: Mutex::new(DeviceState {
                transport,
                subs: SubRegistry::new(initial),
            }),
            wake: Notify::new(),
        })
    }

    pub fn nudge(&self) {
        self.wake.notify_one();
    }

    pub fn close(&self) {
        let mut st = self.state.lock().unwrap();
        st.subs.close_all();
        st.transport.close();
        self.nudge();
    }
}

/// Adaptive background monitor:
/// - 0 wakeups/sec when idle with no active playback and no subscribers.
/// - 50 ms playback event polling while active.
/// - 500 ms hardware volume sync when RenderingControl subscribers exist.
pub async fn run_monitor(dev: Arc<Device>, mut shutdown_rx: watch::Receiver<bool>) {
    let mut last_vol_sync = Instant::now() - Duration::from_secs(10);

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        let next_wait = {
            let mut st = dev.state.lock().unwrap();
            let now = Instant::now();
            st.subs.expire_stale(now);

            let mut changed = st.transport.poll_events();
            let active = st.transport.is_active();
            let has_rc = st.subs.has_rc_subs();

            if has_rc && now.duration_since(last_vol_sync) >= Duration::from_millis(500) {
                st.transport.sync_volume();
                last_vol_sync = now;
                changed = true;
            }

            if changed {
                let ev = st.transport.event_state();
                st.subs.publish(ev);
            }

            let mut wait: Option<Duration> = None;
            let mut consider = |d: Duration| {
                wait = Some(match wait {
                    Some(cur) => cur.min(d),
                    None => d,
                });
            };
            if active {
                consider(Duration::from_millis(50));
            }
            if has_rc {
                let since = now.duration_since(last_vol_sync);
                consider(
                    Duration::from_millis(500)
                        .saturating_sub(since)
                        .max(Duration::from_millis(50)),
                );
            }
            if let Some(exp) = st.subs.next_expiry() {
                consider(
                    exp.saturating_duration_since(now)
                        .max(Duration::from_millis(50)),
                );
            }
            wait
        };

        match next_wait {
            Some(dur) => {
                tokio::select! {
                    _ = shutdown_rx.changed() => {}
                    _ = dev.wake.notified() => {}
                    _ = tokio::time::sleep(dur) => {}
                }
            }
            None => {
                tokio::select! {
                    _ = shutdown_rx.changed() => {}
                    _ = dev.wake.notified() => {}
                }
            }
        }
    }
}

/// Serve UPnP HTTP/1.1 requests on `listener` until `shutdown_rx` signals stop.
pub async fn serve_http(
    listener: TcpListener,
    dev: Arc<Device>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            res = listener.accept() => {
                let Ok((stream, peer)) = res else {
                    continue;
                };
                let dev = Arc::clone(&dev);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let dev = Arc::clone(&dev);
                        async move { Ok::<_, std::convert::Infallible>(handle_request(dev, peer, req).await) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        }
    }
}

fn resp_with(
    status: StatusCode,
    content_type: Option<&str>,
    body: String,
) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header("Content-Type", ct);
    }
    builder.body(Full::new(Bytes::from(body))).unwrap()
}

async fn handle_request(
    dev: Arc<Device>,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    if !dev.filter.allows(peer.ip()) {
        return resp_with(StatusCode::FORBIDDEN, None, String::new());
    }

    let path = req.uri().path().to_string();
    let method = req.method().clone();

    if method == Method::GET && (path == "/description.xml" || path == "/device.xml") {
        return resp_with(
            StatusCode::OK,
            Some(r#"text/xml; charset="utf-8""#),
            dev.device_doc.clone(),
        );
    }

    // Parse /<Service>/<endpoint>
    let mut segs = path.trim_start_matches('/').split('/');
    let Some(service) = segs.next() else {
        return resp_with(StatusCode::NOT_FOUND, None, String::new());
    };
    let Some(endpoint) = segs.next() else {
        return resp_with(StatusCode::NOT_FOUND, None, String::new());
    };
    if segs.next().is_some() || !SERVICES.contains(&service) {
        return resp_with(StatusCode::NOT_FOUND, None, String::new());
    }

    match endpoint {
        "scpd.xml" if method == Method::GET => {
            if let Some(doc) = dev.scpd_docs.get(service) {
                resp_with(
                    StatusCode::OK,
                    Some(r#"text/xml; charset="utf-8""#),
                    doc.clone(),
                )
            } else {
                resp_with(StatusCode::NOT_FOUND, None, String::new())
            }
        }
        "control" if method == Method::POST => handle_control(dev, service, req).await,
        "events" if service != PQ => handle_events(dev, service, peer.ip(), req),
        _ => resp_with(StatusCode::NOT_FOUND, None, String::new()),
    }
}

async fn handle_control(
    dev: Arc<Device>,
    service: &str,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let soap_action = req
        .headers()
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_default();

    let Some(action_name) = parse_soap_action(&soap_action, service) else {
        return soap_fault(Fault::INVALID_ACTION);
    };
    let Some(spec) = find_action(service, &action_name) else {
        return soap_fault(Fault::INVALID_ACTION);
    };

    let body_bytes = match collect_limited(req.into_body(), MAX_SOAP_BODY).await {
        Ok(b) => b,
        Err(_) => return resp_with(StatusCode::PAYLOAD_TOO_LARGE, None, String::new()),
    };

    let args = match parse_soap_args(&body_bytes, &action_name) {
        Ok(a) => a,
        Err(f) => return soap_fault(f),
    };

    // Validate required input arguments exist in SOAP body.
    for f in spec.inputs.split_whitespace() {
        let name = f.split_once(':').map(|(n, _)| n).unwrap_or(f);
        if !args.contains_key(name) {
            return soap_fault(Fault::INVALID_ARGS);
        }
    }

    // Fast streaming path for BrowseQueue: snapshot under lock, serialize unlocked.
    if service == PQ && action_name == "BrowseQueue" {
        let qname = args.get("QueueName").map(String::as_str).unwrap_or("");
        let snap = {
            let st = dev.state.lock().unwrap();
            st.transport.browse_snapshot(qname)
        };
        return match snap {
            Ok(s) => resp_with(
                StatusCode::OK,
                Some(r#"text/xml; charset="utf-8""#),
                format_browse_soap(&s),
            ),
            Err(f) => soap_fault(f),
        };
    }

    let result = {
        let mut st = dev.state.lock().unwrap();
        let res = st.transport.call(service, &action_name, &args);
        let ev = st.transport.event_state();
        st.subs.publish(ev);
        res
    };
    dev.nudge();

    match result {
        Ok(out_fields) => {
            let xml = soap_response_xml(service, &action_name, spec.outputs, &out_fields);
            resp_with(StatusCode::OK, Some(r#"text/xml; charset="utf-8""#), xml)
        }
        Err(f) => soap_fault(f),
    }
}

fn soap_fault(f: Fault) -> Response<Full<Bytes>> {
    resp_with(
        StatusCode::INTERNAL_SERVER_ERROR,
        Some(r#"text/xml; charset="utf-8""#),
        f.to_soap_xml(),
    )
}

fn handle_events(
    dev: Arc<Device>,
    service: &str,
    peer: IpAddr,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let method = req.method().as_str();
    let sid_hdr = req
        .headers()
        .get("sid")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let cb_hdr = req
        .headers()
        .get("callback")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let nt_hdr = req
        .headers()
        .get("nt")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let timeout_sec =
        parse_timeout_seconds(req.headers().get("timeout").and_then(|v| v.to_str().ok()));
    let peer_norm = normalize_ip(peer);

    if method == "UNSUBSCRIBE" {
        if sid_hdr.is_empty() || !cb_hdr.is_empty() || !nt_hdr.is_empty() {
            return resp_with(StatusCode::BAD_REQUEST, None, String::new());
        }
        let ok = {
            let mut st = dev.state.lock().unwrap();
            let valid = st
                .subs
                .subs
                .get(&sid_hdr)
                .is_some_and(|s| s.service == service && s.peer == peer_norm);
            if valid {
                if let Some(sub) = st.subs.subs.remove(&sid_hdr) {
                    let _ = sub.stop_tx.send(true);
                }
                true
            } else {
                false
            }
        };
        dev.nudge();
        return if ok {
            resp_with(StatusCode::OK, None, String::new())
        } else {
            resp_with(StatusCode::PRECONDITION_FAILED, None, String::new())
        };
    }

    if method != "SUBSCRIBE" {
        return resp_with(StatusCode::METHOD_NOT_ALLOWED, None, String::new());
    }

    // Renewal request: has SID, must NOT have Callback or NT.
    if !sid_hdr.is_empty() {
        if !cb_hdr.is_empty() || !nt_hdr.is_empty() {
            return resp_with(StatusCode::BAD_REQUEST, None, String::new());
        }
        let ok = {
            let mut st = dev.state.lock().unwrap();
            if let Some(sub) = st.subs.subs.get_mut(&sid_hdr)
                && sub.service == service
                && sub.peer == peer_norm
            {
                sub.expires = Instant::now() + Duration::from_secs(timeout_sec);
                true
            } else {
                false
            }
        };
        dev.nudge();
        return if ok {
            Response::builder()
                .status(StatusCode::OK)
                .header("SID", sid_hdr)
                .header("TIMEOUT", format!("Second-{timeout_sec}"))
                .body(Full::new(Bytes::new()))
                .unwrap()
        } else {
            resp_with(StatusCode::PRECONDITION_FAILED, None, String::new())
        };
    }

    // Fresh subscription: requires NT: upnp:event and valid peer-bound CALLBACK.
    if nt_hdr.trim() != "upnp:event" {
        return resp_with(StatusCode::PRECONDITION_FAILED, None, String::new());
    }
    let Some(callback) = parse_callback_url(&cb_hdr, peer_norm) else {
        return resp_with(StatusCode::PRECONDITION_FAILED, None, String::new());
    };

    let sid = format!("uuid:{}", Uuid::new_v4());
    let (initial, rx, stop_rx) = {
        let mut st = dev.state.lock().unwrap();
        if service == "RenderingControl" {
            st.transport.sync_volume();
        }
        let initial = st.transport.event_state();
        let (tx, rx) = watch::channel(initial.clone());
        let (stop_tx, stop_rx) = watch::channel(false);
        st.subs.subs.insert(
            sid.clone(),
            Subscription {
                sid: sid.clone(),
                service: service.to_string(),
                callback: callback.clone(),
                peer: peer_norm,
                expires: Instant::now() + Duration::from_secs(timeout_sec),
                tx,
                stop_tx,
            },
        );
        (initial, rx, stop_rx)
    };
    dev.nudge();

    let dev_drop = Arc::clone(&dev);
    spawn_notifier(
        sid.clone(),
        service.to_string(),
        callback,
        initial,
        rx,
        stop_rx,
        move |dead_sid| {
            let mut st = dev_drop.state.lock().unwrap();
            st.subs.subs.remove(&dead_sid);
            dev_drop.nudge();
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("SID", sid)
        .header("TIMEOUT", format!("Second-{timeout_sec}"))
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn parse_soap_action(header: &str, service: &str) -> Option<String> {
    let trimmed = header.trim().trim_matches('"');
    let (svc, act) = trimmed.split_once('#')?;
    if svc != service_type(service) || act.is_empty() {
        return None;
    }
    Some(act.to_string())
}

async fn collect_limited(mut body: Incoming, max_bytes: usize) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return Err(()) };
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > max_bytes {
                return Err(());
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
}

fn parse_soap_args(body: &[u8], expected_action: &str) -> Result<Fields, Fault> {
    let text = std::str::from_utf8(body).map_err(|_| Fault::INVALID_ARGS)?;
    let mut reader = Reader::from_str(text);
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let mut action_depth: Option<usize> = None;
    let mut cur_arg: Option<String> = None;
    let mut fields = Fields::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if action_depth.is_none() {
                    if local == expected_action {
                        action_depth = Some(depth);
                    }
                } else if Some(depth) == action_depth.map(|d| d + 1) {
                    fields.entry(local.clone()).or_default();
                    cur_arg = Some(local);
                }
            }
            Ok(Event::Empty(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if action_depth.is_none() && local == expected_action {
                    action_depth = Some(depth + 1);
                } else if let Some(ad) = action_depth
                    && depth == ad
                {
                    fields.entry(local).or_default();
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(arg) = &cur_arg {
                    let s = e.unescape().map_err(|_| Fault::INVALID_ARGS)?;
                    if let Some(val) = fields.get_mut(arg) {
                        val.push_str(&s);
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if let Some(arg) = &cur_arg {
                    let s = String::from_utf8_lossy(e.as_ref());
                    if let Some(val) = fields.get_mut(arg) {
                        val.push_str(&s);
                    }
                }
            }
            Ok(Event::End(_)) => {
                if let Some(ad) = action_depth {
                    if depth == ad + 1 {
                        cur_arg = None;
                    } else if depth == ad {
                        break;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(Fault::INVALID_ARGS),
            _ => {}
        }
        buf.clear();
    }

    if action_depth.is_none() {
        return Err(Fault::INVALID_ACTION);
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_filter_handles_exact_cidr_and_mapped_ipv6() {
        let f = ControllerFilter::parse("192.168.1.0/24, 127.0.0.1").unwrap();
        assert!(f.allows("192.168.1.42".parse().unwrap()));
        assert!(f.allows("::ffff:192.168.1.9".parse().unwrap()));
        assert!(f.allows("127.0.0.1".parse().unwrap()));
        assert!(!f.allows("192.168.2.1".parse().unwrap()));
        assert!(!f.allows("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn parse_callback_validates_peer_ip_and_multiple_urls() {
        let peer: IpAddr = "192.168.1.10".parse().unwrap();
        assert_eq!(
            parse_callback_url("<http://192.168.1.10:8080/notify>", peer).as_deref(),
            Some("http://192.168.1.10:8080/notify")
        );
        // Second entry matches when first is another host.
        assert_eq!(
            parse_callback_url(
                "<http://10.0.0.1:9999/nope><http://192.168.1.10:8080/ok>",
                peer
            )
            .as_deref(),
            Some("http://192.168.1.10:8080/ok")
        );
        // Rejects mismatched host.
        assert_eq!(
            parse_callback_url("<http://192.168.1.11:8080/evil>", peer),
            None
        );
    }
}

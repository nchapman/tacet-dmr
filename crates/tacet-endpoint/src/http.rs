//! Unified single-port HTTP/1.1 server hosting both the primary OpenHome
//! `Source:1` device (`/description.xml`, `/scpd/*`, `/ctl/*`, `/evt/*`) and
//! the optional minimal UPnP AV `MediaRenderer:1` compatibility device
//! (`/upnp/description.xml`, `/upnp/{Service}/{scpd.xml,control,events}`).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, SERVER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use openhome_core::gena::{format_timeout_header, parse_callback_header, parse_timeout_header};
use openhome_core::soap::encode_fault as encode_oh_fault;
use openhome_server::SubscribeError;
use openhome_server::dispatch::ServiceRegistry;
use quick_xml::Reader;
use quick_xml::events::Event;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use uuid::Uuid;

use crate::coordinator::EndpointCoordinator;
use crate::upnp::events::{
    Subscription, normalize_ip, parse_callback_url, parse_timeout_seconds, spawn_notifier,
};
use crate::upnp::schema::{
    Fault as UpnpFault, SERVICES as UPNP_SERVICES, find_action, scpd_xml as upnp_scpd_xml,
    service_type as upnp_service_type, soap_response_xml,
};

const MAX_SOAP_BODY_BYTES: usize = 4 * 1024 * 1024;
const HTTP_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const SOAP_BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_CONNS: usize = 128;
const MAX_UPNP_SUBSCRIPTIONS: usize = 64;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Optional IP / CIDR allowlist gating HTTP control and eventing across both
/// OpenHome and UPnP AV devices.
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
        if rules.is_empty() {
            return Err("controller allowlist must contain at least one IP or CIDR".into());
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

pub struct UpnpHttpConfig {
    pub description_xml: Bytes,
    pub scpd_docs: HashMap<&'static str, Bytes>,
}

impl UpnpHttpConfig {
    pub fn new(description_xml: String) -> Self {
        let mut scpd_docs = HashMap::new();
        for &svc in UPNP_SERVICES {
            scpd_docs.insert(svc, Bytes::from(upnp_scpd_xml(svc)));
        }
        Self {
            description_xml: Bytes::from(description_xml),
            scpd_docs,
        }
    }
}

#[derive(Clone)]
pub struct HttpState {
    pub oh_description_xml: Bytes,
    pub oh_registry: ServiceRegistry,
    pub upnp: Option<Arc<UpnpHttpConfig>>,
    pub coordinator: Arc<EndpointCoordinator>,
    pub filter: Option<Arc<ControllerFilter>>,
    pub server_header: Arc<String>,
}

pub async fn serve_http(
    listener: TcpListener,
    state: HttpState,
    mut shutdown: watch::Receiver<bool>,
) {
    let conn_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));

    loop {
        let permit = tokio::select! {
            res = conn_limit.clone().acquire_owned() => match res {
                Ok(p) => p,
                Err(_) => break,
            },
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
        };

        let accept = tokio::select! {
            res = listener.accept() => res,
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
        };

        let Ok((stream, peer)) = accept else {
            drop(permit);
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };

        let peer_ip = peer.ip();
        let state = state.clone();
        let mut conn_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle_request(req, peer_ip, state).await) }
            });

            let conn = http1::Builder::new()
                .keep_alive(true)
                .timer(TokioTimer::new())
                .header_read_timeout(HTTP_HEADER_READ_TIMEOUT)
                .serve_connection(io, svc);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {}
                res = conn_shutdown.changed() => {
                    if res.is_err() || *conn_shutdown.borrow() {
                        conn.as_mut().graceful_shutdown();
                        let _ = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, conn).await;
                    }
                }
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    peer_ip: IpAddr,
    state: HttpState,
) -> Response<Full<Bytes>> {
    if let Some(filter) = &state.filter
        && !filter.allows(peer_ip)
    {
        return empty_response(StatusCode::FORBIDDEN, &state.server_header);
    }

    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // --- OpenHome Source:1 routes ---
    if path == "/description.xml" {
        if method == Method::GET || method == Method::HEAD {
            let body = if method == Method::HEAD {
                Bytes::new()
            } else {
                state.oh_description_xml.clone()
            };
            return xml_response(
                StatusCode::OK,
                body,
                state.oh_description_xml.len(),
                &state.server_header,
            );
        }
        return empty_response(StatusCode::METHOD_NOT_ALLOWED, &state.server_header);
    }

    if let Some(rest) = path.strip_prefix("/scpd/") {
        if method != Method::GET && method != Method::HEAD {
            return empty_response(StatusCode::METHOD_NOT_ALLOWED, &state.server_header);
        }
        let slug = rest.strip_suffix(".xml").unwrap_or(rest);
        if let Some(scpd) = state.oh_registry.scpd_xml(slug) {
            let body = if method == Method::HEAD {
                Bytes::new()
            } else {
                Bytes::from_static(scpd.as_bytes())
            };
            return xml_response(StatusCode::OK, body, scpd.len(), &state.server_header);
        }
        return empty_response(StatusCode::NOT_FOUND, &state.server_header);
    }

    if let Some(slug) = path.strip_prefix("/ctl/") {
        if method != Method::POST {
            return empty_response(StatusCode::METHOD_NOT_ALLOWED, &state.server_header);
        }
        if !state.oh_registry.has_service(slug) {
            return empty_response(StatusCode::NOT_FOUND, &state.server_header);
        }
        return handle_oh_soap(req, slug, state).await;
    }

    if let Some(slug) = path.strip_prefix("/evt/") {
        if !state.oh_registry.has_service(slug) {
            return empty_response(StatusCode::NOT_FOUND, &state.server_header);
        }
        return match method.as_str() {
            "SUBSCRIBE" => handle_oh_subscribe(&req, peer_ip, slug, &state),
            "UNSUBSCRIBE" => handle_oh_unsubscribe(&req, slug, &state),
            _ => empty_response(StatusCode::METHOD_NOT_ALLOWED, &state.server_header),
        };
    }

    // --- Minimal UPnP AV MediaRenderer:1 routes ---
    if let Some(upnp) = state.upnp.clone()
        && let Some(subpath) = path.strip_prefix("/upnp")
    {
        if subpath == "/description.xml" {
            if method == Method::GET || method == Method::HEAD {
                let body = if method == Method::HEAD {
                    Bytes::new()
                } else {
                    upnp.description_xml.clone()
                };
                return xml_response(
                    StatusCode::OK,
                    body,
                    upnp.description_xml.len(),
                    &state.server_header,
                );
            }
            return empty_response(StatusCode::METHOD_NOT_ALLOWED, &state.server_header);
        }

        let trimmed = subpath.trim_start_matches('/');
        if let Some((svc, kind)) = trimmed.split_once('/')
            && let Some(&canonical_svc) = UPNP_SERVICES.iter().find(|&&s| s == svc)
        {
            return match kind {
                "scpd.xml" => {
                    if method != Method::GET && method != Method::HEAD {
                        return empty_response(
                            StatusCode::METHOD_NOT_ALLOWED,
                            &state.server_header,
                        );
                    }
                    let doc = upnp.scpd_docs.get(canonical_svc).unwrap();
                    let body = if method == Method::HEAD {
                        Bytes::new()
                    } else {
                        doc.clone()
                    };
                    xml_response(StatusCode::OK, body, doc.len(), &state.server_header)
                }
                "control" => {
                    if method != Method::POST {
                        return empty_response(
                            StatusCode::METHOD_NOT_ALLOWED,
                            &state.server_header,
                        );
                    }
                    handle_upnp_soap(req, canonical_svc, &state).await
                }
                "events" => handle_upnp_events(&req, peer_ip, canonical_svc, &state),
                _ => empty_response(StatusCode::NOT_FOUND, &state.server_header),
            };
        }
    }

    empty_response(StatusCode::NOT_FOUND, &state.server_header)
}

// ---------------------------------------------------------------------------
// OpenHome SOAP + GENA handlers (delegating directly to openhome-server)
// ---------------------------------------------------------------------------

async fn read_soap_body(
    req: Request<Incoming>,
    server_header: &str,
) -> Result<(Option<String>, String), Box<Response<Full<Bytes>>>> {
    if let Some(cl) = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        && cl > MAX_SOAP_BODY_BYTES as u64
    {
        return Err(Box::new(empty_close_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            server_header,
        )));
    }

    let soap_action = req
        .headers()
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let limited = Limited::new(req.into_body(), MAX_SOAP_BODY_BYTES);
    let body_bytes = match tokio::time::timeout(SOAP_BODY_READ_TIMEOUT, limited.collect()).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(err)) => {
            let status = if err.is::<LengthLimitError>() {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return Err(Box::new(empty_close_response(status, server_header)));
        }
        Err(_) => {
            return Err(Box::new(empty_close_response(
                StatusCode::REQUEST_TIMEOUT,
                server_header,
            )));
        }
    };
    let Ok(body_str) = std::str::from_utf8(&body_bytes) else {
        return Err(Box::new(empty_response(
            StatusCode::BAD_REQUEST,
            server_header,
        )));
    };
    Ok((soap_action, body_str.to_string()))
}

async fn handle_oh_soap(
    req: Request<Incoming>,
    slug: &str,
    state: HttpState,
) -> Response<Full<Bytes>> {
    let (soap_action, body_str) = match read_soap_body(req, &state.server_header).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    match state
        .oh_registry
        .dispatch_soap(slug, soap_action.as_deref(), &body_str)
    {
        Ok(output) => {
            if !output.mutated_services.is_empty() {
                state.coordinator.notify_openhome(output.mutated_services);
            }
            soap_http_response(StatusCode::OK, output.response_xml, &state.server_header)
        }
        Err(fault) => soap_http_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            encode_oh_fault(&fault),
            &state.server_header,
        ),
    }
}

fn handle_oh_subscribe(
    req: &Request<Incoming>,
    peer_ip: IpAddr,
    slug: &str,
    state: &HttpState,
) -> Response<Full<Bytes>> {
    let sid = header_str(req, "sid");
    let callback = header_str(req, "callback");
    let nt = header_str(req, "nt");
    let timeout = header_str(req, "timeout").and_then(parse_timeout_header);

    if sid.is_some() && (callback.is_some() || nt.is_some()) {
        return empty_response(StatusCode::BAD_REQUEST, &state.server_header);
    }

    if let Some(sid) = sid {
        let Some(granted) = state.coordinator.oh_events().renew(slug, sid, timeout) else {
            return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
        };
        return subscribe_ok_response(sid, granted, &state.server_header);
    }

    if !nt.is_some_and(|v| v.eq_ignore_ascii_case("upnp:event")) {
        return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
    }
    let callbacks = callback.map(parse_callback_header).unwrap_or_default();
    if callbacks.is_empty() {
        return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
    }

    let Some(initial_state) = state.oh_registry.state_snapshot(slug) else {
        return empty_response(StatusCode::NOT_FOUND, &state.server_header);
    };

    match state.coordinator.oh_events().subscribe(
        slug,
        callbacks,
        Some(peer_ip),
        timeout,
        initial_state,
    ) {
        Ok((new_sid, granted)) => subscribe_ok_response(&new_sid, granted, &state.server_header),
        Err(SubscribeError::InvalidCallbacks) => {
            empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header)
        }
        Err(SubscribeError::TooManySubscriptions) => {
            empty_response(StatusCode::SERVICE_UNAVAILABLE, &state.server_header)
        }
    }
}

fn handle_oh_unsubscribe(
    req: &Request<Incoming>,
    slug: &str,
    state: &HttpState,
) -> Response<Full<Bytes>> {
    let sid = header_str(req, "sid");
    let callback = header_str(req, "callback");
    let nt = header_str(req, "nt");

    if callback.is_some() || nt.is_some() {
        return empty_response(StatusCode::BAD_REQUEST, &state.server_header);
    }
    let Some(sid) = sid else {
        return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
    };
    if state.coordinator.oh_events().unsubscribe(slug, sid) {
        empty_response(StatusCode::OK, &state.server_header)
    } else {
        empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header)
    }
}

// ---------------------------------------------------------------------------
// Minimal UPnP AV SOAP + GENA handlers
// ---------------------------------------------------------------------------

async fn handle_upnp_soap(
    req: Request<Incoming>,
    service: &'static str,
    state: &HttpState,
) -> Response<Full<Bytes>> {
    let (soap_action_hdr, body_str) = match read_soap_body(req, &state.server_header).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    let expected_type = upnp_service_type(service);
    let action_from_hdr = soap_action_hdr.as_deref().and_then(|raw| {
        let trimmed = raw.trim().trim_matches('"');
        let (urn, act) = trimmed.rsplit_once('#')?;
        if urn == expected_type {
            Some(act.to_string())
        } else {
            None
        }
    });

    let Ok((body_action, args)) = parse_upnp_soap_body(&body_str) else {
        return soap_http_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            UpnpFault::INVALID_ARGS.to_soap_xml(),
            &state.server_header,
        );
    };

    let action = match (action_from_hdr, body_action) {
        (Some(h), Some(b)) if h != b => {
            return soap_http_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                UpnpFault::INVALID_ACTION.to_soap_xml(),
                &state.server_header,
            );
        }
        (Some(h), _) => h,
        (None, Some(b)) => b,
        (None, None) => {
            return soap_http_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                UpnpFault::INVALID_ACTION.to_soap_xml(),
                &state.server_header,
            );
        }
    };

    let Some(spec) = find_action(service, &action) else {
        return soap_http_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            UpnpFault::INVALID_ACTION.to_soap_xml(),
            &state.server_header,
        );
    };

    match state.coordinator.call_upnp(service, &action, &args) {
        Ok(fields) => {
            let xml = soap_response_xml(service, &action, spec.outputs, &fields);
            soap_http_response(StatusCode::OK, xml, &state.server_header)
        }
        Err(fault) => soap_http_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            fault.to_soap_xml(),
            &state.server_header,
        ),
    }
}

fn handle_upnp_events(
    req: &Request<Incoming>,
    peer_ip: IpAddr,
    service: &'static str,
    state: &HttpState,
) -> Response<Full<Bytes>> {
    let method = req.method().as_str();
    let sid = header_str(req, "sid");
    let callback = header_str(req, "callback");
    let nt = header_str(req, "nt");

    match method {
        "SUBSCRIBE" => {
            if sid.is_some() && (callback.is_some() || nt.is_some()) {
                return empty_response(StatusCode::BAD_REQUEST, &state.server_header);
            }
            let secs = parse_timeout_seconds(header_str(req, "timeout"));
            let now = Instant::now();

            if let Some(sid) = sid {
                let mut subs = state.coordinator.upnp_subs().lock().unwrap();
                subs.expire_stale(now);
                let Some(sub) = subs.subs.get_mut(sid) else {
                    return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
                };
                if sub.service != service || normalize_ip(sub.peer) != normalize_ip(peer_ip) {
                    return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
                }
                sub.expires = now + Duration::from_secs(secs);
                return subscribe_ok_response(sid, secs as u32, &state.server_header);
            }

            if !nt.is_some_and(|v| v.eq_ignore_ascii_case("upnp:event")) {
                return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
            }
            let Some(cb_url) = callback.and_then(|c| parse_callback_url(c, peer_ip)) else {
                return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
            };

            let initial = state.coordinator.upnp_event_state();
            let new_sid = format!("uuid:{}", Uuid::new_v4());
            let (tx, rx) = watch::channel(initial.clone());
            let (stop_tx, stop_rx) = watch::channel(false);

            {
                let mut subs = state.coordinator.upnp_subs().lock().unwrap();
                subs.expire_stale(now);
                if subs.subs.len() >= MAX_UPNP_SUBSCRIPTIONS {
                    return empty_response(StatusCode::SERVICE_UNAVAILABLE, &state.server_header);
                }
                subs.subs.insert(
                    new_sid.clone(),
                    Subscription {
                        sid: new_sid.clone(),
                        service: service.to_string(),
                        callback: cb_url.clone(),
                        peer: normalize_ip(peer_ip),
                        expires: now + Duration::from_secs(secs),
                        tx,
                        stop_tx,
                    },
                );
            }

            let coord_drop = Arc::clone(&state.coordinator);
            spawn_notifier(
                new_sid.clone(),
                service.to_string(),
                cb_url,
                initial,
                rx,
                stop_rx,
                move |dropped_sid| {
                    coord_drop
                        .upnp_subs()
                        .lock()
                        .unwrap()
                        .subs
                        .remove(&dropped_sid);
                },
            );

            subscribe_ok_response(&new_sid, secs as u32, &state.server_header)
        }
        "UNSUBSCRIBE" => {
            if callback.is_some() || nt.is_some() {
                return empty_response(StatusCode::BAD_REQUEST, &state.server_header);
            }
            let Some(sid) = sid else {
                return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
            };
            let mut subs = state.coordinator.upnp_subs().lock().unwrap();
            let matches = subs.subs.get(sid).is_some_and(|s| {
                s.service == service && normalize_ip(s.peer) == normalize_ip(peer_ip)
            });
            if !matches {
                return empty_response(StatusCode::PRECONDITION_FAILED, &state.server_header);
            }
            if let Some(sub) = subs.subs.remove(sid) {
                let _ = sub.stop_tx.send(true);
            }
            empty_response(StatusCode::OK, &state.server_header)
        }
        _ => empty_response(StatusCode::METHOD_NOT_ALLOWED, &state.server_header),
    }
}

fn parse_upnp_soap_body(xml: &str) -> Result<(Option<String>, HashMap<String, String>), String> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_body = false;
    let mut action: Option<String> = None;
    let mut cur_arg: Option<String> = None;
    let mut cur_val = String::new();
    let mut args = HashMap::new();
    let mut depth_in_action: usize = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qn = e.name();
                let local = local_name(qn.as_ref());
                if !in_body {
                    if local == "Body" {
                        in_body = true;
                    }
                } else if action.is_none() {
                    action = Some(local.to_string());
                    depth_in_action = 1;
                } else if depth_in_action == 1 {
                    cur_arg = Some(local.to_string());
                    cur_val.clear();
                    depth_in_action = 2;
                } else {
                    depth_in_action += 1;
                }
            }
            Ok(Event::Empty(e)) => {
                let qn = e.name();
                let local = local_name(qn.as_ref());
                if in_body && action.is_none() {
                    action = Some(local.to_string());
                } else if in_body && depth_in_action == 1 {
                    args.insert(local.to_string(), String::new());
                }
            }
            Ok(Event::Text(e)) => {
                if cur_arg.is_some()
                    && let Ok(t) = e.unescape()
                {
                    cur_val.push_str(&t);
                }
            }
            Ok(Event::CData(e)) => {
                if cur_arg.is_some()
                    && let Ok(t) = std::str::from_utf8(e.as_ref())
                {
                    cur_val.push_str(t);
                }
            }
            Ok(Event::End(e)) => {
                let qn = e.name();
                let local = local_name(qn.as_ref());
                if in_body && local == "Body" && depth_in_action == 0 {
                    break;
                } else if depth_in_action == 2 {
                    if let Some(name) = cur_arg.take() {
                        args.insert(name, std::mem::take(&mut cur_val));
                    }
                    depth_in_action = 1;
                } else {
                    depth_in_action = depth_in_action.saturating_sub(1);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("xml parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok((action, args))
}

fn local_name(raw: &[u8]) -> &str {
    let s = std::str::from_utf8(raw).unwrap_or("");
    s.rsplit_once(':').map(|(_, l)| l).unwrap_or(s)
}

fn header_str<'a>(req: &'a Request<Incoming>, name: &str) -> Option<&'a str> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn subscribe_ok_response(
    sid: &str,
    timeout_secs: u32,
    server_header: &str,
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("SID", sid)
        .header("TIMEOUT", format_timeout_header(timeout_secs))
        .header(SERVER, server_header)
        .header(CONTENT_LENGTH, "0")
        .body(Full::new(Bytes::new()))
        .expect("valid SUBSCRIBE response")
}

fn soap_http_response(
    status: StatusCode,
    xml: String,
    server_header: &str,
) -> Response<Full<Bytes>> {
    let bytes = Bytes::from(xml);
    let len = bytes.len();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/xml; charset=\"utf-8\"")
        .header(CONTENT_LENGTH, len.to_string())
        .header("EXT", "")
        .header(SERVER, server_header)
        .body(Full::new(bytes))
        .expect("valid SOAP response")
}

fn xml_response(
    status: StatusCode,
    body: Bytes,
    content_length: usize,
    server_header: &str,
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/xml; charset=\"utf-8\"")
        .header(CONTENT_LENGTH, content_length.to_string())
        .header(SERVER, server_header)
        .body(Full::new(body))
        .expect("valid XML response")
}

fn empty_response(status: StatusCode, server_header: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(SERVER, server_header)
        .header(CONTENT_LENGTH, "0")
        .body(Full::new(Bytes::new()))
        .expect("valid empty response")
}

fn empty_close_response(status: StatusCode, server_header: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(SERVER, server_header)
        .header(CONTENT_LENGTH, "0")
        .header(CONNECTION, "close")
        .body(Full::new(Bytes::new()))
        .expect("valid empty close response")
}

//! End-to-end integration tests exercising `tacet-endpoint` over real loopback
//! HTTP/SOAP/GENA and Songcast UDP sockets using `openhome-client` and raw UPnP AV SOAP.

use std::convert::Infallible;
use std::f64::consts::PI;
use std::fs;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use openhome_client::{EventListener, OpenHomeDevice};
use openhome_core::urn::SERVICE_VOLUME_1;
use openhome_core::{ReceiverState, SenderStatus, SourceType, TransportState};
use tacet_endpoint::http::ControllerFilter;
use tacet_endpoint::upnp::events::{format_timestamp, parse_timestamp};
use tacet_endpoint::{
    EndpointConfig, RunningEndpoint, derive_upnp_udn, resolve_listen, resolve_udn,
};
use tokio::net::{TcpListener, TcpStream};

fn build_wav_bytes(sample_rate: u32, frames: usize, freq_hz: f64) -> Vec<u8> {
    let channels: u16 = 2;
    let bits: u16 = 16;
    let data_bytes = (frames * channels as usize * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..frames {
        let s = ((2.0 * PI * freq_hz * i as f64 / sample_rate as f64).sin() * 14_000.0) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

async fn spawn_wav_server(wav_a: Vec<u8>, wav_b: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let a = Bytes::from(wav_a);
    let b = Bytes::from(wav_b);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let a = a.clone();
            let b = b.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let a = a.clone();
                    let b = b.clone();
                    async move {
                        let data = match req.uri().path() {
                            "/a.wav" => a,
                            "/b.wav" => b,
                            _ => {
                                return Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                );
                            }
                        };
                        let range_hdr = req
                            .headers()
                            .get("range")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        if let Some(r) = range_hdr
                            && let Some(spec) = r.strip_prefix("bytes=")
                            && let Some((s, e)) = spec.split_once('-')
                            && let Ok(start) = s.parse::<usize>()
                        {
                            let end = e
                                .parse::<usize>()
                                .ok()
                                .map(|v| v.min(data.len().saturating_sub(1)))
                                .unwrap_or_else(|| data.len().saturating_sub(1));
                            if start <= end && start < data.len() {
                                let slice = data.slice(start..=end);
                                return Ok(Response::builder()
                                    .status(StatusCode::PARTIAL_CONTENT)
                                    .header("Content-Type", "audio/wav")
                                    .header("Accept-Ranges", "bytes")
                                    .header(
                                        "Content-Range",
                                        format!("bytes {start}-{end}/{}", data.len()),
                                    )
                                    .header("Content-Length", slice.len().to_string())
                                    .body(Full::new(slice))
                                    .unwrap());
                            }
                        }
                        Ok(Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "audio/wav")
                            .header("Accept-Ranges", "bytes")
                            .header("Content-Length", data.len().to_string())
                            .body(Full::new(data))
                            .unwrap())
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    (format!("http://{addr}"), handle)
}

async fn http_request(
    addr: SocketAddr,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Host", addr.to_string())
        .header("Content-Length", body.len().to_string());
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = builder
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn upnp_soap(
    addr: SocketAddr,
    service: &str,
    action: &str,
    args: &[(&str, &str)],
) -> (StatusCode, String) {
    let urn = format!("urn:schemas-upnp-org:service:{service}:1");
    let mut inner = String::new();
    for (k, v) in args {
        inner.push_str(&format!("<{k}>{v}</{k}>"));
    }
    let body = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{urn}">{inner}</u:{action}></s:Body></s:Envelope>"#
    );
    let soap_action = format!("\"{urn}#{action}\"");
    http_request(
        addr,
        Method::POST,
        &format!("/upnp/{service}/control"),
        &[
            ("Content-Type", "text/xml; charset=\"utf-8\""),
            ("SOAPACTION", &soap_action),
        ],
        &body,
    )
    .await
}

fn extract_tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

#[tokio::test]
async fn openhome_playlist_gapless_playback_and_volume() {
    let wav_a = build_wav_bytes(44_100, 11_025, 440.0); // 250 ms
    let wav_b = build_wav_bytes(44_100, 11_025, 880.0); // 250 ms
    let (base_url, srv) = spawn_wav_server(wav_a, wav_b).await;

    let endpoint = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        room: Some("Listening Room".into()),
        name: "Desk DAC".into(),
        no_ssdp: true,
        audio_device: "null".into(),
        initial_volume: Some(60),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();

    // Distinct UDNs for OpenHome and UPnP AV.
    assert_ne!(endpoint.openhome_udn(), endpoint.upnp_udn().unwrap());

    let dev = OpenHomeDevice::connect(endpoint.openhome_url())
        .await
        .unwrap();

    // Product metadata and sources (Playlist, Songcast, UPnP AV).
    let product = dev.product().unwrap();
    let pinfo = product.product().await.unwrap();
    assert_eq!(pinfo.room, "Listening Room");
    assert_eq!(pinfo.name, "Desk DAC");
    let sources = product.source_xml().await.unwrap();
    assert_eq!(sources.len(), 3);
    assert_eq!(sources[0].source_type, SourceType::Playlist);
    assert_eq!(sources[1].source_type, SourceType::Receiver);
    assert_eq!(sources[2].source_type, SourceType::UpnpAv);

    // Volume control works and rejects non-zero balance with SOAP fault.
    let vol = dev.volume().unwrap();
    assert_eq!(vol.volume().await.unwrap(), 60);
    vol.set_volume(75).await.unwrap();
    assert_eq!(vol.volume().await.unwrap(), 75);
    vol.volume_inc().await.unwrap();
    assert_eq!(vol.volume().await.unwrap(), 76);
    vol.volume_dec().await.unwrap();
    assert_eq!(vol.volume().await.unwrap(), 75);
    vol.set_mute(true).await.unwrap();
    assert!(vol.mute().await.unwrap());
    vol.set_mute(false).await.unwrap();
    assert!(vol.set_balance(1).await.is_err());

    // Playlist rejects file:// URIs over the network.
    let playlist = dev.playlist().unwrap();
    assert!(playlist.insert(0, "file:///etc/passwd", "").await.is_err());

    // Insert two tracks and play gaplessly to completion.
    let uri_a = format!("{base_url}/a.wav");
    let uri_b = format!("{base_url}/b.wav");
    let id1 = playlist
        .insert(0, &uri_a, "<DIDL-Lite>A</DIDL-Lite>")
        .await
        .unwrap();
    let id2 = playlist
        .insert(id1, &uri_b, "<DIDL-Lite>B</DIDL-Lite>")
        .await
        .unwrap();
    let (_, ids) = playlist.id_array().await.unwrap();
    assert_eq!(ids, vec![id1, id2]);

    playlist.play().await.unwrap();

    // Wait for track 2 to become active, then cleanly finish Stopped.
    let mut saw_track_2 = false;
    let mut saw_stopped = false;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let cur = playlist.id().await.unwrap();
        let state = playlist.transport_state().await.unwrap();
        if cur == id2 {
            saw_track_2 = true;
        }
        if saw_track_2 && state == TransportState::Stopped {
            saw_stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(saw_track_2, "expected gapless transition to track 2");
    assert!(saw_stopped, "expected clean Stopped at end of playlist");

    let info = dev.info().unwrap();
    let details = info.details().await.unwrap();
    assert_eq!(details.sample_rate, 44_100);
    assert_eq!(details.codec_name, "PCM");
    assert!(details.lossless);

    endpoint.shutdown().await;
    srv.abort();
}

#[tokio::test]
async fn upnp_av_compat_shared_volume_and_source_arbitration() {
    let wav_a = build_wav_bytes(44_100, 44_100, 440.0); // 1.0 s
    let wav_b = build_wav_bytes(44_100, 22_050, 660.0); // 0.5 s
    let (base_url, srv) = spawn_wav_server(wav_a, wav_b).await;

    let endpoint = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        room: Some("Office".into()),
        name: "Studio DAC".into(),
        no_ssdp: true,
        audio_device: "null".into(),
        initial_volume: Some(50),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();
    let addr = endpoint.local_addr();

    // UPnP description XML exposes MediaRenderer:1 with "(UPnP)" suffix and NO PlayQueue.
    let (st, desc) = http_request(addr, Method::GET, "/upnp/description.xml", &[], "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(desc.contains("urn:schemas-upnp-org:device:MediaRenderer:1"));
    assert!(desc.contains("<friendlyName>Studio DAC (UPnP)</friendlyName>"));
    assert!(!desc.contains("PlayQueue"));

    // ConnectionManager GetProtocolInfo advertises FLAC + Opus, omits raw L16.
    let (st, proto) = upnp_soap(addr, "ConnectionManager", "GetProtocolInfo", &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert!(proto.contains("audio/flac"));
    assert!(proto.contains("audio/opus"));
    assert!(!proto.contains("audio/L16"));

    // Shared volume between UPnP RenderingControl and OpenHome Volume:1.
    let oh = OpenHomeDevice::connect(endpoint.openhome_url())
        .await
        .unwrap();
    let oh_vol = oh.volume().unwrap();
    let (st, _) = upnp_soap(
        addr,
        "RenderingControl",
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", "42"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(oh_vol.volume().await.unwrap(), 42);

    oh_vol.set_mute(true).await.unwrap();
    let (st, mute_xml) = upnp_soap(
        addr,
        "RenderingControl",
        "GetMute",
        &[("InstanceID", "0"), ("Channel", "Master")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(extract_tag(&mute_xml, "CurrentMute").as_deref(), Some("1"));
    oh_vol.set_mute(false).await.unwrap();

    // Start playing on OpenHome Playlist.
    let uri_a = format!("{base_url}/a.wav");
    let uri_b = format!("{base_url}/b.wav");
    let oh_pl = oh.playlist().unwrap();
    let oh_prod = oh.product().unwrap();
    let oh_sender = oh.sender().unwrap();
    oh_pl.insert(0, &uri_a, "").await.unwrap();
    oh_pl.play().await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if oh_pl.transport_state().await.unwrap() == TransportState::Playing {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        oh_pl.transport_state().await.unwrap(),
        TransportState::Playing
    );
    assert_eq!(oh_sender.status().await.unwrap(), SenderStatus::Enabled);
    assert!(oh_sender.audio().await.unwrap());

    // Now UPnP AV takes over via SetAVTransportURI + Play -> OpenHome switches to UpnpAv source!
    let (st, _) = upnp_soap(
        addr,
        "AVTransport",
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &uri_a),
            ("CurrentURIMetaData", ""),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = upnp_soap(
        addr,
        "AVTransport",
        "SetNextAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("NextURI", &uri_b),
            ("NextURIMetaData", ""),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = upnp_soap(
        addr,
        "AVTransport",
        "Play",
        &[("InstanceID", "0"), ("Speed", "1")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Product.SourceIndex is now 2 (UpnpAv), OpenHome Playlist is Stopped, Sender is Disabled.
    assert_eq!(oh_prod.source_index().await.unwrap(), 2);
    assert_eq!(
        oh_pl.transport_state().await.unwrap(),
        TransportState::Stopped
    );
    assert_eq!(oh_sender.status().await.unwrap(), SenderStatus::Disabled);

    // Wait for UPnP PLAYING.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut playing = false;
    while Instant::now() < deadline {
        let (_, ti) = upnp_soap(
            addr,
            "AVTransport",
            "GetTransportInfo",
            &[("InstanceID", "0")],
        )
        .await;
        if extract_tag(&ti, "CurrentTransportState").as_deref() == Some("PLAYING") {
            playing = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(playing, "expected UPnP AVTransport PLAYING");

    // OpenHome Playlist.Play reclaims focus and transitions UPnP AV back to STOPPED.
    oh_pl.play().await.unwrap();
    assert_eq!(oh_prod.source_index().await.unwrap(), 0);
    let (_, ti) = upnp_soap(
        addr,
        "AVTransport",
        "GetTransportInfo",
        &[("InstanceID", "0")],
    )
    .await;
    assert_eq!(
        extract_tag(&ti, "CurrentTransportState").as_deref(),
        Some("STOPPED")
    );

    endpoint.shutdown().await;
    srv.abort();
}

#[tokio::test]
async fn songcast_leader_to_follower_delivers_pcm_and_metadata() {
    let wav_a = build_wav_bytes(44_100, 66_150, 440.0); // 1.5 s
    let (base_url, srv) = spawn_wav_server(wav_a, Vec::new()).await;

    let out_dir = std::env::temp_dir().join(format!("tacet-ep-sc-{}", std::process::id()));
    let _ = fs::create_dir_all(&out_dir);
    let out_raw = out_dir.join("follower.raw");
    let _ = fs::remove_file(&out_raw);

    let leader = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        room: Some("Leader".into()),
        name: "Leader DAC".into(),
        no_ssdp: true,
        no_upnp: true,
        audio_device: "null".into(),
        initial_volume: Some(100),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();

    let follower = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        room: Some("Follower".into()),
        name: "Follower DAC".into(),
        no_ssdp: true,
        no_upnp: true,
        no_sender: true,
        audio_device: format!("pcm/{}", out_raw.display()),
        initial_volume: Some(100),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();

    let sender_uri = leader.sender_uri().expect("leader Songcast sender URI");
    let leader_cp = OpenHomeDevice::connect(leader.openhome_url())
        .await
        .unwrap();
    let follower_cp = OpenHomeDevice::connect(follower.openhome_url())
        .await
        .unwrap();

    // Connect follower Receiver to leader Sender.
    let rx_svc = follower_cp.receiver().unwrap();
    rx_svc.set_sender(sender_uri, "").await.unwrap();
    rx_svc.play().await.unwrap();

    // Start playing on leader Playlist.
    let uri_a = format!("{base_url}/a.wav");
    let leader_pl = leader_cp.playlist().unwrap();
    leader_pl
        .insert(0, &uri_a, "<DIDL-Lite>Songcast Test</DIDL-Lite>")
        .await
        .unwrap();
    leader_pl.play().await.unwrap();

    // Wait for follower Receiver to reach Playing and receive non-zero PCM audio.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut follower_playing = false;
    while Instant::now() < deadline {
        if rx_svc.transport_state().await.unwrap() == ReceiverState::Playing
            && fs::metadata(&out_raw).is_ok_and(|m| m.len() >= 1024)
        {
            follower_playing = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        follower_playing,
        "expected Songcast follower to receive PCM audio from leader"
    );

    let follower_info = follower_cp.info().unwrap();
    let details = follower_info.details().await.unwrap();
    assert_eq!(details.sample_rate, 44_100);

    rx_svc.stop().await.unwrap();
    assert_eq!(
        rx_svc.transport_state().await.unwrap(),
        ReceiverState::Stopped
    );

    follower.shutdown().await;
    leader.shutdown().await;
    srv.abort();
    let _ = fs::remove_dir_all(&out_dir);
}

#[tokio::test]
async fn controller_filter_gates_both_openhome_and_upnp() {
    let endpoint = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        controller: Some("198.51.100.1".into()),
        name: "Locked DAC".into(),
        no_ssdp: true,
        no_sender: true,
        audio_device: "null".into(),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();
    let addr = endpoint.local_addr();

    let (st_oh, _) = http_request(addr, Method::GET, "/description.xml", &[], "").await;
    assert_eq!(st_oh, StatusCode::FORBIDDEN);
    let (st_upnp, _) = http_request(addr, Method::GET, "/upnp/description.xml", &[], "").await;
    assert_eq!(st_upnp, StatusCode::FORBIDDEN);

    endpoint.shutdown().await;
}

#[tokio::test]
async fn no_upnp_flag_disables_upnp_device_and_source() {
    let endpoint = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        name: "Pure OpenHome".into(),
        no_ssdp: true,
        no_upnp: true,
        no_sender: true,
        audio_device: "null".into(),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();

    assert!(endpoint.upnp_url().is_none());
    assert!(endpoint.upnp_udn().is_none());
    assert!(endpoint.sender_uri().is_none());

    let (st, _) = http_request(
        endpoint.local_addr(),
        Method::GET,
        "/upnp/description.xml",
        &[],
        "",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let dev = OpenHomeDevice::connect(endpoint.openhome_url())
        .await
        .unwrap();
    let sources = dev.product().unwrap().source_xml().await.unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].source_type, SourceType::Playlist);
    assert_eq!(sources[1].source_type, SourceType::Receiver);

    endpoint.shutdown().await;
}

#[tokio::test]
async fn openhome_gena_volume_events_fire_on_upnp_volume_change() {
    let endpoint = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        name: "Gena Test".into(),
        no_ssdp: true,
        no_sender: true,
        audio_device: "null".into(),
        initial_volume: Some(30),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();

    let dev = OpenHomeDevice::connect(endpoint.openhome_url())
        .await
        .unwrap();
    let listener = EventListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let mut sub = listener
        .subscribe_service(&dev, SERVICE_VOLUME_1, 300)
        .await
        .unwrap();

    // Initial SEQ=0 snapshot carries Volume=30.
    let ev0 = tokio::time::timeout(Duration::from_secs(3), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ev0.seq, 0);
    assert_eq!(ev0.properties.get("Volume").map(String::as_str), Some("30"));

    // Mutating volume via UPnP RenderingControl pushes a GENA update to OpenHome Volume:1!
    let (st, _) = upnp_soap(
        endpoint.local_addr(),
        "RenderingControl",
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", "67"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let ev1 = tokio::time::timeout(Duration::from_secs(3), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(ev1.seq >= 1);
    assert_eq!(ev1.properties.get("Volume").map(String::as_str), Some("67"));

    sub.unsubscribe().await.unwrap();
    listener.shutdown().await;
    endpoint.shutdown().await;
}

#[tokio::test]
async fn upnp_av_faults_and_pause_seek_stop_lifecycle() {
    let wav = build_wav_bytes(44_100, 44_100 * 3, 440.0); // 3.0 s
    let (base_url, srv) = spawn_wav_server(wav, Vec::new()).await;

    let endpoint = RunningEndpoint::start(EndpointConfig {
        listen: "127.0.0.1:0".into(),
        name: "Fault Test".into(),
        no_ssdp: true,
        no_sender: true,
        audio_device: "null".into(),
        quiet: true,
        ..EndpointConfig::default()
    })
    .await
    .unwrap();
    let addr = endpoint.local_addr();

    // Invalid InstanceID -> SOAP 718
    let (st, body) = upnp_soap(
        addr,
        "AVTransport",
        "GetTransportInfo",
        &[("InstanceID", "9")],
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(extract_tag(&body, "errorCode").as_deref(), Some("718"));

    // Unsupported PlayMode -> SOAP 712
    let (st, body) = upnp_soap(
        addr,
        "AVTransport",
        "SetPlayMode",
        &[("InstanceID", "0"), ("NewPlayMode", "SHUFFLE")],
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(extract_tag(&body, "errorCode").as_deref(), Some("712"));

    // Rejects file:// URI -> SOAP 402
    let (st, body) = upnp_soap(
        addr,
        "AVTransport",
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", "file:///etc/passwd"),
            ("CurrentURIMetaData", ""),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(extract_tag(&body, "errorCode").as_deref(), Some("402"));

    // Load, Play, Pause, Seek, Stop
    let uri = format!("{base_url}/a.wav");
    let (st, _) = upnp_soap(
        addr,
        "AVTransport",
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &uri),
            ("CurrentURIMetaData", ""),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = upnp_soap(
        addr,
        "AVTransport",
        "Play",
        &[("InstanceID", "0"), ("Speed", "1")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let (_, ti) = upnp_soap(
            addr,
            "AVTransport",
            "GetTransportInfo",
            &[("InstanceID", "0")],
        )
        .await;
        if extract_tag(&ti, "CurrentTransportState").as_deref() == Some("PLAYING") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Unsupported Seek Unit -> 710, malformed Target -> 711, valid REL_TIME -> 200
    let (st, body) = upnp_soap(
        addr,
        "AVTransport",
        "Seek",
        &[("InstanceID", "0"), ("Unit", "TRACK_NR"), ("Target", "1")],
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(extract_tag(&body, "errorCode").as_deref(), Some("710"));

    let (st, body) = upnp_soap(
        addr,
        "AVTransport",
        "Seek",
        &[
            ("InstanceID", "0"),
            ("Unit", "REL_TIME"),
            ("Target", "not-a-time"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(extract_tag(&body, "errorCode").as_deref(), Some("711"));

    let (st, _) = upnp_soap(
        addr,
        "AVTransport",
        "Seek",
        &[
            ("InstanceID", "0"),
            ("Unit", "REL_TIME"),
            ("Target", "0:00:01"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = upnp_soap(addr, "AVTransport", "Pause", &[("InstanceID", "0")]).await;
    assert_eq!(st, StatusCode::OK);
    let (_, ti) = upnp_soap(
        addr,
        "AVTransport",
        "GetTransportInfo",
        &[("InstanceID", "0")],
    )
    .await;
    assert_eq!(
        extract_tag(&ti, "CurrentTransportState").as_deref(),
        Some("PAUSED_PLAYBACK")
    );

    let (st, _) = upnp_soap(addr, "AVTransport", "Stop", &[("InstanceID", "0")]).await;
    assert_eq!(st, StatusCode::OK);
    let (_, ti) = upnp_soap(
        addr,
        "AVTransport",
        "GetTransportInfo",
        &[("InstanceID", "0")],
    )
    .await;
    assert_eq!(
        extract_tag(&ti, "CurrentTransportState").as_deref(),
        Some("STOPPED")
    );

    endpoint.shutdown().await;
    srv.abort();
}

#[test]
fn config_and_filter_helpers() {
    let f = ControllerFilter::parse("192.168.1.0/24, 10.0.0.5").unwrap();
    assert!(f.allows("192.168.1.42".parse().unwrap()));
    assert!(f.allows("10.0.0.5".parse().unwrap()));
    assert!(!f.allows("192.168.2.1".parse().unwrap()));
    assert!(ControllerFilter::parse("192.168.1.0/33").is_err());

    let b = resolve_listen("127.0.0.1:49152").unwrap();
    assert_eq!(b.bind_addr.port(), 49152);
    assert_eq!(b.advertise_ip.to_string(), "127.0.0.1");

    let u1 = resolve_udn("12345678-1234-5678-1234-567812345678").unwrap();
    assert_eq!(u1, "uuid:12345678-1234-5678-1234-567812345678");
    let u2 = derive_upnp_udn(&u1);
    assert_ne!(u1, u2);
    assert_eq!(u2, derive_upnp_udn(&u1));

    assert_eq!(format_timestamp(3665), "1:01:05");
    assert_eq!(parse_timestamp("1:01:05"), Some(3665.0));
    assert_eq!(parse_timestamp("0:01:02.500"), Some(62.5));
    assert_eq!(parse_timestamp("0:60:00"), None);
}

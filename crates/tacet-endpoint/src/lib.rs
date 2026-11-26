//! `tacet-endpoint` — Self-contained OpenHome Media, Songcast, and minimal
//! UPnP/DLNA audiophile audio endpoint powered by `tacet-audio` and `openhome-rs`.

pub mod coordinator;
pub mod http;
pub mod songcast;
pub mod ssdp;
pub mod upnp;

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use openhome_core::DeviceDescription;
use openhome_core::urn::{DEVICE_SOURCE_1, UPNP_ROOT_DEVICE};
use openhome_core::xml::encode_device_description;
use openhome_songcast::{SenderConfig, SongcastSender, SongcastUri, encode_sender_metadata};
use tacet_audio::{EngineConfig, is_exclusive_target, validate_target};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::coordinator::{CoordinatorInit, EndpointCoordinator};
use crate::http::{ControllerFilter, HttpState, UpnpHttpConfig, serve_http};
use crate::ssdp::{SsdpDevice, run_ssdp};
use crate::upnp::schema::{
    DEVICE_TYPE as UPNP_DEVICE_TYPE, SERVICES as UPNP_SERVICES, device_xml as upnp_device_xml,
    service_type as upnp_service_type,
};

/// Configuration for hosting a `tacet-endpoint` instance.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// LAN IPv4:port or interface:port (e.g. `"en0:49152"`, `"127.0.0.1:0"`).
    pub listen: String,
    /// Optional trusted controller IPv4 or CIDR allowlist (comma-separated).
    /// When `None`, any LAN peer reaching the bound interface is accepted.
    pub controller: Option<String>,
    /// OpenHome room name advertised via `Product:1` (defaults to `name` if `None`/empty).
    pub room: Option<String>,
    /// Device friendly name. The UPnP AV compat device advertises `"<name> (UPnP)"`.
    pub name: String,
    /// Optional explicit UUID (with or without `uuid:` prefix). Derived stably
    /// from the machine hostname when empty.
    pub uuid: String,
    /// Disable UDP multicast SSDP announcements and `M-SEARCH` responses.
    pub no_ssdp: bool,
    /// Disable the minimal UPnP/DLNA `MediaRenderer:1` compatibility device.
    pub no_upnp: bool,
    /// Disable OpenHome `Sender:1` multi-room Songcast leader rebroadcasting.
    pub no_sender: bool,
    /// Whether network control points may load `file://` or local filesystem paths.
    /// Defaults to `false` (HTTP/HTTPS URIs only).
    pub allow_local_files: bool,
    /// Audio output device ID (`"auto"`, `"null"`, `"coreaudio/<uid>"`, etc.).
    pub audio_device: String,
    /// Request exclusive hardware mode.
    pub exclusive: bool,
    /// Optional explicit initial volume (0..=100).
    pub initial_volume: Option<u8>,
    /// Suppress operational stderr logs.
    pub quiet: bool,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            listen: String::new(),
            controller: None,
            room: None,
            name: "Tacet Endpoint".into(),
            uuid: String::new(),
            no_ssdp: false,
            no_upnp: false,
            no_sender: false,
            allow_local_files: false,
            audio_device: "auto".into(),
            exclusive: false,
            initial_volume: None,
            quiet: false,
        }
    }
}

/// Resolved bind address and concrete IPv4 advertise address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenBinding {
    pub bind_addr: SocketAddrV4,
    pub advertise_ip: Ipv4Addr,
}

/// Resolve `"IP:PORT"` or `"IFACE:PORT"` (e.g. `"en0:49152"`, `"0.0.0.0:49152"`,
/// `"127.0.0.1:0"`) into concrete bind and advertise IPv4 addresses.
pub fn resolve_listen(raw: &str) -> Result<ListenBinding, String> {
    let raw = raw.trim();
    let (host, port_s) = raw
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid -serve address {raw:?}: expected IP:PORT or IFACE:PORT"))?;
    let port: u16 = port_s
        .parse()
        .map_err(|_| format!("invalid port in -serve {raw:?}"))?;

    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        let advertise_ip = if ip.is_unspecified() {
            default_outbound_ipv4()
        } else {
            ip
        };
        return Ok(ListenBinding {
            bind_addr: SocketAddrV4::new(ip, port),
            advertise_ip,
        });
    }

    let ifaces = if_addrs::get_if_addrs().map_err(|e| format!("list network interfaces: {e}"))?;
    for iface in ifaces {
        if iface.name == host
            && let IpAddr::V4(v4) = iface.ip()
            && !v4.is_unspecified()
        {
            return Ok(ListenBinding {
                bind_addr: SocketAddrV4::new(v4, port),
                advertise_ip: v4,
            });
        }
    }
    Err(format!("network interface {host:?} has no IPv4 address"))
}

fn default_outbound_ipv4() -> Ipv4Addr {
    if let Ok(sock) = StdUdpSocket::bind("0.0.0.0:0")
        && sock.connect("192.0.2.1:80").is_ok()
        && let Ok(SocketAddr::V4(v4)) = sock.local_addr()
        && !v4.ip().is_unspecified()
    {
        return *v4.ip();
    }
    Ipv4Addr::LOCALHOST
}

/// Derive a stable OpenHome UDN (`uuid:...`) from an explicit UUID or the machine hostname.
pub fn resolve_udn(explicit: &str) -> Result<String, String> {
    let raw = explicit
        .trim()
        .strip_prefix("uuid:")
        .unwrap_or(explicit.trim());
    if !raw.is_empty() {
        let u = Uuid::parse_str(raw).map_err(|e| format!("invalid -uuid {explicit:?}: {e}"))?;
        return Ok(format!("uuid:{u}"));
    }
    let host = hostname_str();
    let u = Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!("tacet-endpoint.{host}").as_bytes(),
    );
    Ok(format!("uuid:{u}"))
}

/// Derive a distinct, deterministic UDN for the co-hosted UPnP AV `MediaRenderer:1`
/// compatibility device from the primary OpenHome UDN.
pub fn derive_upnp_udn(openhome_udn: &str) -> String {
    let u = Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!("{openhome_udn}:upnp-av").as_bytes(),
    );
    format!("uuid:{u}")
}

fn hostname_str() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..len]) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "tacet-endpoint".into())
}

/// Running OpenHome + Songcast + optional UPnP/DLNA endpoint handle.
pub struct RunningEndpoint {
    local_addr: SocketAddr,
    openhome_udn: String,
    openhome_url: String,
    upnp_udn: Option<String>,
    upnp_url: Option<String>,
    sender_uri: Option<String>,
    coordinator: Arc<EndpointCoordinator>,
    sender: Option<Arc<SongcastSender>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_done: AtomicBool,
    monitor_task: Option<JoinHandle<()>>,
    http_task: Option<JoinHandle<()>>,
    ssdp_task: Option<JoinHandle<()>>,
}

impl RunningEndpoint {
    /// Start the endpoint and return a handle to its bound URLs and lifecycle.
    pub async fn start(cfg: EndpointConfig) -> Result<Self, String> {
        let name = cfg.name.trim().to_string();
        if name.is_empty() {
            return Err("renderer name cannot be empty".into());
        }
        let room = cfg
            .room
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&name)
            .to_string();

        let filter = match cfg.controller.as_deref() {
            Some(s) => Some(Arc::new(ControllerFilter::parse(s)?)),
            None => None,
        };

        let binding = resolve_listen(&cfg.listen)?;
        let openhome_udn = resolve_udn(&cfg.uuid)?;
        let upnp_udn = (!cfg.no_upnp).then(|| derive_upnp_udn(&openhome_udn));
        validate_target(&cfg.audio_device)?;

        let exclusive = is_exclusive_target(&cfg.audio_device, cfg.exclusive);
        if let Some(v) = cfg.initial_volume
            && v > 100
        {
            return Err("initial volume must be 0-100".into());
        }

        let runtime = tokio::runtime::Handle::current();

        // Bind optional Songcast Sender before coordinator init.
        let (sender, sender_uri, sender_metadata) = if !cfg.no_sender {
            let sc_sender = SongcastSender::bind(SenderConfig {
                bind_addr: SocketAddrV4::new(binding.bind_addr.ip().to_owned(), 0),
                advertise_ip: Some(binding.advertise_ip),
                interface: *binding.bind_addr.ip(),
                ..SenderConfig::default()
            })
            .await
            .map_err(|e| format!("bind Songcast sender: {e}"))?;
            let sc_uri = sc_sender.uri();
            let uri_str = sc_uri.to_string();
            let meta = encode_sender_metadata(
                &room,
                &uri_str,
                matches!(sc_uri, SongcastUri::Multicast(_)),
                None,
            );
            (Some(Arc::new(sc_sender)), Some(uri_str), meta)
        } else {
            (None, None, String::new())
        };

        let (coordinator, channels) = EndpointCoordinator::new(CoordinatorInit {
            room: room.clone(),
            name: name.clone(),
            enable_upnp: !cfg.no_upnp,
            allow_local_files: cfg.allow_local_files,
            songcast_interface: *binding.bind_addr.ip(),
            sender: sender.clone(),
            sender_metadata,
            engine_config: EngineConfig {
                audio_device: cfg.audio_device.clone(),
                exclusive,
                initial_volume: cfg.initial_volume,
            },
            runtime,
            quiet: cfg.quiet,
        })?;

        let oh_registry = coordinator.build_openhome_registry();
        let oh_friendly_name = if room != name {
            format!("{room} ({name})")
        } else {
            name.clone()
        };
        let oh_description = DeviceDescription {
            url_base: None,
            device_type: DEVICE_SOURCE_1.to_string(),
            friendly_name: oh_friendly_name,
            manufacturer: "Tacet".to_string(),
            manufacturer_url: Some("https://github.com/nchapman/tacet-endpoint".to_string()),
            model_description: Some("Tacet Audiophile OpenHome & UPnP/DLNA Endpoint".to_string()),
            model_name: "Tacet Endpoint".to_string(),
            model_number: Some(env!("CARGO_PKG_VERSION").to_string()),
            model_url: Some("https://github.com/nchapman/tacet-endpoint".to_string()),
            serial_number: None,
            udn: openhome_udn.clone(),
            presentation_url: None,
            services: oh_registry.service_descriptions(),
        };
        let oh_description_xml = Bytes::from(encode_device_description(&oh_description));

        let listener = TcpListener::bind(SocketAddr::V4(binding.bind_addr))
            .await
            .map_err(|e| format!("listen on {}: {e}", binding.bind_addr))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;

        let base_url = format!("http://{}:{}", binding.advertise_ip, local_addr.port());
        let openhome_url = format!("{base_url}/description.xml");
        let upnp_url = upnp_udn
            .as_ref()
            .map(|_| format!("{base_url}/upnp/description.xml"));

        let upnp_http = upnp_udn.as_ref().map(|udn| {
            let upnp_name = format!("{name} (UPnP)");
            Arc::new(UpnpHttpConfig::new(upnp_device_xml(&upnp_name, udn)))
        });

        let server_header = Arc::new(format!(
            "{}/1.0 UPnP/1.1 tacet-endpoint/{}",
            std::env::consts::OS,
            env!("CARGO_PKG_VERSION")
        ));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let monitor_task = tokio::spawn(Arc::clone(&coordinator).run_monitor(
            channels.player_rx,
            channels.songcast_rx,
            shutdown_rx.clone(),
        ));

        let http_state = HttpState {
            oh_description_xml,
            oh_registry: oh_registry.clone(),
            upnp: upnp_http,
            coordinator: Arc::clone(&coordinator),
            filter,
            server_header: Arc::clone(&server_header),
        };
        let http_task = tokio::spawn(serve_http(listener, http_state, shutdown_rx.clone()));

        let ssdp_task = if !cfg.no_ssdp {
            let mut ssdp_devices = Vec::new();
            let mut oh_targets = vec![
                UPNP_ROOT_DEVICE.to_string(),
                openhome_udn.clone(),
                DEVICE_SOURCE_1.to_string(),
            ];
            for urn in oh_registry.service_type_urns() {
                oh_targets.push(urn.to_string());
            }
            ssdp_devices.push(SsdpDevice {
                udn: openhome_udn.clone(),
                location: openhome_url.clone(),
                targets: oh_targets,
            });

            if let (Some(u_udn), Some(u_loc)) = (&upnp_udn, &upnp_url) {
                let mut u_targets = vec![
                    UPNP_ROOT_DEVICE.to_string(),
                    u_udn.clone(),
                    UPNP_DEVICE_TYPE.to_string(),
                ];
                for &svc in UPNP_SERVICES {
                    u_targets.push(upnp_service_type(svc));
                }
                ssdp_devices.push(SsdpDevice {
                    udn: u_udn.clone(),
                    location: u_loc.clone(),
                    targets: u_targets,
                });
            }

            let advertise_ip = binding.advertise_ip;
            let hdr = (*server_header).clone();
            let rx = shutdown_rx.clone();
            let quiet = cfg.quiet;
            Some(tokio::spawn(async move {
                if let Err(e) = run_ssdp(ssdp_devices, advertise_ip, hdr, rx).await
                    && !quiet
                {
                    eprintln!("tacet-endpoint: SSDP error: {e}");
                }
            }))
        } else {
            None
        };

        Ok(Self {
            local_addr,
            openhome_udn,
            openhome_url,
            upnp_udn,
            upnp_url,
            sender_uri,
            coordinator,
            sender,
            shutdown_tx,
            shutdown_done: AtomicBool::new(false),
            monitor_task: Some(monitor_task),
            http_task: Some(http_task),
            ssdp_task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn openhome_udn(&self) -> &str {
        &self.openhome_udn
    }

    pub fn openhome_url(&self) -> &str {
        &self.openhome_url
    }

    pub fn upnp_udn(&self) -> Option<&str> {
        self.upnp_udn.as_deref()
    }

    pub fn upnp_url(&self) -> Option<&str> {
        self.upnp_url.as_deref()
    }

    pub fn sender_uri(&self) -> Option<&str> {
        self.sender_uri.as_deref()
    }

    /// Gracefully stop HTTP, SSDP, monitor task, audio playback, and Songcast sender.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        // Drain network & monitor tasks first so no in-flight SOAP call can
        // re-open playback after `coordinator.shutdown()` runs.
        if let Some(t) = self.ssdp_task.take() {
            let _ = t.await;
        }
        if let Some(t) = self.http_task.take() {
            let _ = t.await;
        }
        if let Some(t) = self.monitor_task.take() {
            let _ = t.await;
        }
        if !self.shutdown_done.swap(true, Ordering::AcqRel) {
            let coord = Arc::clone(&self.coordinator);
            let _ = tokio::task::spawn_blocking(move || coord.shutdown()).await;
        }
        if let Some(mut sender) = self.sender.take() {
            // Wait briefly for any in-flight Songcast tap command to finish.
            for _ in 0..20 {
                match Arc::try_unwrap(sender) {
                    Ok(owned) => {
                        owned.shutdown().await;
                        break;
                    }
                    Err(shared) => {
                        sender = shared;
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
            }
        }
    }
}

impl Drop for RunningEndpoint {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if !self.shutdown_done.swap(true, Ordering::AcqRel) {
            self.coordinator.shutdown();
        }
    }
}

/// Start `tacet-endpoint` and run until `shutdown_rx` becomes `true`.
pub async fn serve<W: Write>(
    cfg: EndpointConfig,
    mut shutdown_rx: watch::Receiver<bool>,
    out: &mut W,
) -> Result<(), String> {
    let controller_label = cfg
        .controller
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("any")
        .to_string();
    let room_label = cfg
        .room
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(cfg.name.trim())
        .to_string();
    let name_label = cfg.name.trim().to_string();
    let ssdp_enabled = !cfg.no_ssdp;
    let upnp_enabled = !cfg.no_upnp;

    let endpoint = RunningEndpoint::start(cfg).await?;

    let _ = writeln!(out, "{name_label} ({})", endpoint.openhome_udn());
    let _ = writeln!(out, "TACET_OPENHOME_URL={}", endpoint.openhome_url());
    if let Some(u) = endpoint.upnp_url() {
        let _ = writeln!(out, "TACET_UPNP_URL={u}");
    }
    if let Some(sc) = endpoint.sender_uri() {
        let _ = writeln!(out, "TACET_SONGCAST_URL={sc}");
    }
    let _ = writeln!(
        out,
        "Room: {room_label} · Controller: {controller_label} · SSDP: {ssdp_enabled} · UPnP AV: {upnp_enabled} · Engine: tacet-audio"
    );
    let _ = out.flush();

    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }

    endpoint.shutdown().await;
    Ok(())
}

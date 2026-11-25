//! `tacet-dmr` — Embeddable UPnP / LinkPlay audio renderer powered by `tacet-audio`.

pub mod renderer;

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use tacet_audio::{EngineConfig, is_exclusive_target, validate_target};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;

use crate::renderer::http::{ControllerFilter, Device, run_monitor, serve_http};
use crate::renderer::ssdp::run_ssdp;
use crate::renderer::transport::Transport;

/// Configuration for hosting a UPnP / LinkPlay renderer.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// LAN IPv4:port or interface:port (e.g. `"en0:49152"`, `"127.0.0.1:0"`).
    pub listen: String,
    /// Trusted controller IPv4 or LAN CIDR (e.g. `"192.168.1.0/24"`).
    pub controller: String,
    /// UPnP friendly name.
    pub name: String,
    /// Optional explicit UUID (without `uuid:` prefix). Derived stably from
    /// hostname when empty.
    pub uuid: String,
    /// Disable UDP multicast SSDP announcements.
    pub no_ssdp: bool,
    /// Audio output device ID (`"auto"`, `"null"`, `"coreaudio/<uid>"`, etc.).
    pub audio_device: String,
    /// Request exclusive hardware mode.
    pub exclusive: bool,
    /// Optional explicit initial volume (0..=100).
    pub initial_volume: Option<u8>,
    /// Suppress operational stderr logs.
    pub quiet: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: String::new(),
            controller: String::new(),
            name: "Tacet Renderer".into(),
            uuid: String::new(),
            no_ssdp: false,
            audio_device: "auto".into(),
            exclusive: false,
            initial_volume: None,
            quiet: false,
        }
    }
}

/// Resolve `"IP:PORT"` or `"IFACE:PORT"` (e.g. `"en0:49152"`) into a concrete IPv4 `SocketAddrV4`.
pub fn resolve_listen(raw: &str) -> Result<SocketAddrV4, String> {
    let raw = raw.trim();
    let (host, port_s) = raw
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid -serve address {raw:?}: expected IP:PORT or IFACE:PORT"))?;
    let port: u16 = port_s
        .parse()
        .map_err(|_| format!("invalid port in -serve {raw:?}"))?;

    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        if ip.is_unspecified() {
            return Err("bind -serve to a specific LAN IPv4 or interface (not 0.0.0.0)".into());
        }
        return Ok(SocketAddrV4::new(ip, port));
    }

    // Look up interface name via `if-addrs`.
    let ifaces = if_addrs::get_if_addrs().map_err(|e| format!("list network interfaces: {e}"))?;
    for iface in ifaces {
        if iface.name == host
            && let IpAddr::V4(v4) = iface.ip()
            && !v4.is_unspecified()
        {
            return Ok(SocketAddrV4::new(v4, port));
        }
    }
    Err(format!("network interface {host:?} has no IPv4 address"))
}

/// Derive a stable UPnP UDN (`uuid:...`) from an explicit UUID or the machine hostname.
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
    let u = Uuid::new_v5(&Uuid::NAMESPACE_DNS, format!("tacet-dmr.{host}").as_bytes());
    Ok(format!("uuid:{u}"))
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
        .unwrap_or_else(|| "tacet-dmr".into())
}

/// Start the UPnP/LinkPlay renderer and run until `shutdown_rx` becomes `true`.
pub async fn serve<W: Write>(
    cfg: ServerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
    out: &mut W,
) -> Result<(), String> {
    if cfg.name.trim().is_empty() {
        return Err("renderer name cannot be empty".into());
    }
    if cfg.controller.trim().is_empty() {
        return Err("-controller is required with -serve".into());
    }
    let filter = ControllerFilter::parse(&cfg.controller)?;
    let bind_addr = resolve_listen(&cfg.listen)?;
    let udn = resolve_udn(&cfg.uuid)?;
    validate_target(&cfg.audio_device)?;

    let exclusive = is_exclusive_target(&cfg.audio_device, cfg.exclusive);
    if let Some(v) = cfg.initial_volume
        && v > 100
    {
        return Err("initial volume must be 0-100".into());
    }

    let transport = Transport::new(
        EngineConfig {
            audio_device: cfg.audio_device.clone(),
            exclusive,
            initial_volume: cfg.initial_volume,
        },
        cfg.quiet,
    )?;

    let listener = TcpListener::bind(SocketAddr::V4(bind_addr))
        .await
        .map_err(|e| format!("listen on {bind_addr}: {e}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    let location = format!("http://{local_addr}/description.xml");

    let dev = Device::new(cfg.name.clone(), udn.clone(), filter, transport);

    let monitor_task = tokio::spawn(run_monitor(Arc::clone(&dev), shutdown_rx.clone()));
    let http_task = tokio::spawn(serve_http(listener, Arc::clone(&dev), shutdown_rx.clone()));

    let ssdp_task = if !cfg.no_ssdp {
        let udn_c = udn.clone();
        let loc_c = location.clone();
        let ip = *bind_addr.ip();
        let rx = shutdown_rx.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = run_ssdp(udn_c, loc_c, ip, rx).await {
                eprintln!("tacet-dmr: SSDP error: {e}");
            }
        }))
    } else {
        None
    };

    let _ = writeln!(
        out,
        "{} ({})\nTACET_RENDERER_URL={}\nController: {} · SSDP enabled: {} · Engine: tacet-audio",
        cfg.name, udn, location, cfg.controller, !cfg.no_ssdp
    );
    let _ = out.flush();

    // Wait for shutdown signal.
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }

    dev.close();
    let _ = monitor_task.await;
    let _ = http_task.await;
    if let Some(t) = ssdp_task {
        let _ = t.await;
    }
    Ok(())
}

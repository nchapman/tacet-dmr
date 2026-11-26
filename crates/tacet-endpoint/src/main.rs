//! `tacet-endpoint` CLI entry point.
//!
//! Accepts both single-dash (`-serve`) and double-dash (`--serve`) flags.

use std::io;
use std::process::ExitCode;

use tacet_audio::{
    EngineConfig, PlaybackEvent, Player, is_exclusive_target, list_devices, validate_target,
};
use tacet_endpoint::{EndpointConfig, serve};
use tokio::sync::watch;

const VERSION: &str = match option_env!("TACET_ENDPOINT_VERSION") {
    Some(v) => v,
    None => match option_env!("TACET_DMR_VERSION") {
        Some(v) => v,
        None => env!("CARGO_PKG_VERSION"),
    },
};

#[derive(Default)]
struct CliArgs {
    version: bool,
    list_devices: bool,
    device: String,
    exclusive: bool,
    volume: Option<u8>,
    serve: String,
    controller: Option<String>,
    room: Option<String>,
    name: String,
    uuid: String,
    no_ssdp: bool,
    no_upnp: bool,
    no_sender: bool,
    tracks: Vec<String>,
    help: bool,
}

fn parse_args(raw: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs {
        name: "Tacet Endpoint".into(),
        ..Default::default()
    };
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if !arg.starts_with('-') || arg == "-" {
            out.tracks.push(arg.clone());
            i += 1;
            continue;
        }
        let stripped = arg.trim_start_matches('-');
        let (key, inline_val) = match stripped.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (stripped, None),
        };
        let next_val =
            |idx: &mut usize, inline: Option<String>, flag_name: &str| -> Result<String, String> {
                if let Some(v) = inline {
                    return Ok(v);
                }
                *idx += 1;
                raw.get(*idx)
                    .cloned()
                    .ok_or_else(|| format!("flag -{flag_name} requires a value"))
            };
        match key {
            "h" | "help" => out.help = true,
            "version" => out.version = true,
            "list-devices" => out.list_devices = true,
            "exclusive" => out.exclusive = true,
            "no-ssdp" => out.no_ssdp = true,
            "no-upnp" => out.no_upnp = true,
            "no-sender" => out.no_sender = true,
            "device" => out.device = next_val(&mut i, inline_val, "device")?,
            "serve" => out.serve = next_val(&mut i, inline_val, "serve")?,
            "controller" => out.controller = Some(next_val(&mut i, inline_val, "controller")?),
            "room" => out.room = Some(next_val(&mut i, inline_val, "room")?),
            "name" => out.name = next_val(&mut i, inline_val, "name")?,
            "uuid" => out.uuid = next_val(&mut i, inline_val, "uuid")?,
            "volume" => {
                let s = next_val(&mut i, inline_val, "volume")?;
                let v: u8 = s
                    .parse()
                    .ok()
                    .filter(|&v| v <= 100)
                    .ok_or_else(|| "volume must be an integer from 0 to 100".to_string())?;
                out.volume = Some(v);
            }
            _ => return Err(format!("unknown flag {arg:?}")),
        }
        i += 1;
    }
    Ok(out)
}

fn print_usage() {
    eprintln!(
        "Usage: tacet-endpoint [flags] FILE-OR-HTTP-URL ...\n\
               tacet-endpoint -serve LAN-IP-OR-IFACE:PORT [flags]\n\
               tacet-endpoint -list-devices | -version\n\n\
         Flags:\n\
           -serve ADDR        host OpenHome + UPnP/DLNA endpoint on IPv4:port or iface:port (e.g. en0:49152)\n\
           -room NAME         OpenHome room name (default: value of -name)\n\
           -name NAME         device friendly name (default \"Tacet Endpoint\")\n\
           -controller CIDR   optional trusted controller IPv4 or LAN CIDR allowlist (comma-separated)\n\
           -device ID         output device ID (default \"auto\"; see -list-devices)\n\
           -exclusive         request exclusive hardware output (CoreAudio hog / ALSA hw / WASAPI exclusive)\n\
           -volume 0-100      initial volume\n\
           -uuid UUID         override stable OpenHome UUID\n\
           -no-ssdp           disable multicast SSDP discovery\n\
           -no-upnp           disable basic UPnP/DLNA MediaRenderer:1 compatibility device\n\
           -no-sender         disable OpenHome Songcast Sender:1 multi-room leader rebroadcast\n\
           -list-devices      list audio output device IDs and exit\n\
           -version           print version and exit"
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("tacet-endpoint: {e}");
            return ExitCode::from(1);
        }
    };

    if args.help {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("tacet-endpoint {VERSION}");
        return ExitCode::SUCCESS;
    }
    if args.list_devices {
        if !args.serve.is_empty() || !args.tracks.is_empty() {
            eprintln!("tacet-endpoint: -list-devices cannot be combined with -serve or tracks");
            return ExitCode::from(1);
        }
        for d in list_devices() {
            let mark = if d.is_default { " (default)" } else { "" };
            println!("{}\t{}{mark}", d.id, d.description);
        }
        return ExitCode::SUCCESS;
    }

    if !args.serve.is_empty() {
        if !args.tracks.is_empty() {
            eprintln!("tacet-endpoint: -serve cannot be combined with local tracks");
            return ExitCode::from(1);
        }
        let cfg = EndpointConfig {
            listen: args.serve,
            controller: args.controller,
            room: args.room,
            name: args.name,
            uuid: args.uuid,
            no_ssdp: args.no_ssdp,
            no_upnp: args.no_upnp,
            no_sender: args.no_sender,
            allow_local_files: false,
            audio_device: if args.device.is_empty() {
                "auto".into()
            } else {
                args.device
            },
            exclusive: args.exclusive,
            initial_volume: args.volume,
            quiet: false,
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm = signal(SignalKind::terminate()).ok();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = async {
                        if let Some(ref mut s) = sigterm {
                            s.recv().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            let _ = shutdown_tx.send(true);
        });
        let mut stdout = io::stdout();
        if let Err(e) = serve(cfg, shutdown_rx, &mut stdout).await {
            eprintln!("tacet-endpoint: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    if args.controller.is_some()
        || args.room.is_some()
        || !args.uuid.is_empty()
        || args.no_ssdp
        || args.no_upnp
        || args.no_sender
    {
        eprintln!(
            "tacet-endpoint: -controller, -room, -uuid, -no-ssdp, -no-upnp, and -no-sender require -serve"
        );
        return ExitCode::from(1);
    }
    if args.tracks.is_empty() {
        print_usage();
        return ExitCode::from(1);
    }

    let device = if args.device.is_empty() {
        "auto".to_string()
    } else {
        args.device
    };
    if let Err(e) = validate_target(&device) {
        eprintln!("tacet-endpoint: {e}");
        return ExitCode::from(1);
    }
    let exclusive = is_exclusive_target(&device, args.exclusive);
    let (player, rx) = match Player::new(EngineConfig {
        audio_device: device,
        exclusive,
        initial_volume: args.volume,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tacet-endpoint: {e}");
            return ExitCode::from(1);
        }
    };

    for track in &args.tracks {
        if let Err(e) = player.open(track, false) {
            eprintln!("tacet-endpoint: {e}");
            return ExitCode::from(1);
        }
        while let Ok(ev) = rx.recv() {
            match ev {
                PlaybackEvent::Ready { active, spec, .. } => {
                    eprintln!(
                        "tacet-endpoint: playing {} ({} Hz, {} ch, {}, exclusive={})",
                        active.device_name,
                        active.sample_rate,
                        spec.channels,
                        active.sample_format,
                        active.exclusive
                    );
                }
                PlaybackEvent::TrackEnded { .. } => break,
                PlaybackEvent::Error { message: e, .. } => {
                    eprintln!("tacet-endpoint: {e}");
                    return ExitCode::from(1);
                }
                _ => {}
            }
        }
    }
    player.close();
    ExitCode::SUCCESS
}

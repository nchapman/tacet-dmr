//! `tacet-dmr` CLI entry point.
//!
//! Accepts both single-dash (`-serve`) and double-dash (`--serve`) flags for
//! drop-in compatibility with existing scripts, `interop/*.py`, and Tacet tests.

use std::io;
use std::process::ExitCode;

use tacet_audio::{
    EngineConfig, PlaybackEvent, Player, is_exclusive_target, list_devices, validate_target,
};
use tacet_dmr::{ServerConfig, serve};
use tokio::sync::watch;

const VERSION: &str = match option_env!("TACET_DMR_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Default)]
struct CliArgs {
    version: bool,
    list_devices: bool,
    device: String,
    exclusive: bool,
    volume: Option<u8>,
    serve: String,
    controller: String,
    name: String,
    uuid: String,
    no_ssdp: bool,
    tracks: Vec<String>,
    help: bool,
}

fn parse_args(raw: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs {
        name: "Tacet Renderer".into(),
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
            "device" => out.device = next_val(&mut i, inline_val, "device")?,
            "serve" => out.serve = next_val(&mut i, inline_val, "serve")?,
            "controller" => out.controller = next_val(&mut i, inline_val, "controller")?,
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
        "Usage: tacet-dmr [flags] FILE-OR-HTTP-URL ...\n\
               tacet-dmr -serve LAN-IP-OR-IFACE:PORT -controller TACET-IP-OR-CIDR [flags]\n\
               tacet-dmr -list-devices | -version\n\n\
         Flags:\n\
           -serve ADDR        host a UPnP renderer on LAN IPv4:port or interface:port (e.g. en0:49152)\n\
           -controller CIDR   trusted controller IPv4 or LAN CIDR (required with -serve)\n\
           -name NAME         UPnP friendly name (default \"Tacet Renderer\")\n\
           -device ID         output device ID (default \"auto\"; see -list-devices)\n\
           -exclusive         request exclusive hardware output (CoreAudio hog / ALSA hw / WASAPI exclusive)\n\
           -volume 0-100      initial volume\n\
           -uuid UUID         override stable renderer UUID\n\
           -no-ssdp           disable multicast SSDP discovery\n\
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
            eprintln!("tacet-dmr: {e}");
            return ExitCode::from(1);
        }
    };

    if args.help {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("tacet-dmr {VERSION}");
        return ExitCode::SUCCESS;
    }
    if args.list_devices {
        if !args.serve.is_empty() || !args.tracks.is_empty() {
            eprintln!("tacet-dmr: -list-devices cannot be combined with -serve or tracks");
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
            eprintln!("tacet-dmr: -serve cannot be combined with local tracks");
            return ExitCode::from(1);
        }
        let cfg = ServerConfig {
            listen: args.serve,
            controller: args.controller,
            name: args.name,
            uuid: args.uuid,
            no_ssdp: args.no_ssdp,
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
            eprintln!("tacet-dmr: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    if !args.controller.is_empty() || !args.uuid.is_empty() || args.no_ssdp {
        eprintln!("tacet-dmr: -controller, -uuid, and -no-ssdp require -serve");
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
        eprintln!("tacet-dmr: {e}");
        return ExitCode::from(1);
    }
    let exclusive = is_exclusive_target(&device, args.exclusive);
    let initial_volume = args.volume.unwrap_or(if exclusive { 30 } else { 100 });
    let (player, rx) = match Player::new(EngineConfig {
        audio_device: device,
        exclusive,
        initial_volume,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tacet-dmr: {e}");
            return ExitCode::from(1);
        }
    };

    for track in &args.tracks {
        if let Err(e) = player.open(track, false) {
            eprintln!("tacet-dmr: {e}");
            return ExitCode::from(1);
        }
        while let Ok(ev) = rx.recv() {
            match ev {
                PlaybackEvent::Ready { active, spec, .. } => {
                    eprintln!(
                        "tacet-dmr: playing {} ({} Hz, {} ch, {}, exclusive={})",
                        active.device_name,
                        active.sample_rate,
                        spec.channels,
                        active.sample_format,
                        active.exclusive
                    );
                }
                PlaybackEvent::TrackEnded { .. } => break,
                PlaybackEvent::Error { message: e, .. } => {
                    eprintln!("tacet-dmr: {e}");
                    return ExitCode::from(1);
                }
                _ => {}
            }
        }
    }
    player.close();
    ExitCode::SUCCESS
}

//! UPnP AVTransport:1, RenderingControl:1, ConnectionManager:1, and
//! LinkPlay PlayQueue:1 state machine backed by `tacet-audio`.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use tacet_audio::{EngineConfig, PlaybackEvent, Player};

use crate::renderer::browse::{BrowseSnapshot, snapshot_for_browse};
use crate::renderer::events::{EventState, format_timestamp};
use crate::renderer::queue::{
    MAX_QUEUE_BYTES, MAX_TRACKS, NORMAL, Playlist, TOTAL_QUEUE, Track, is_valid_play_mode,
    loop_code_to_mode, mode_to_loop_code, parse_context, parse_index, track_bytes, valid_uri,
};
use crate::renderer::schema::{CM, Fault, PQ, RC, sink_protocols};

pub type Fields = HashMap<String, String>;

pub struct Transport {
    player: Player,
    events: Receiver<PlaybackEvent>,
    active_session_id: u64,
    pub q: Playlist,
    pub state: String,
    pub status: String,
    pub pos_secs: f64,
    pub duration_secs: f64,
    pub volume: u8,
    pub default_volume: u8,
    pub mute: bool,
    pub active_open: bool,
    pub ended: bool,
    pub queued_next_uri: Option<String>,
    pub quiet: bool,
}

impl Transport {
    pub fn new(cfg: EngineConfig, quiet: bool) -> Result<Self, String> {
        let default_volume = if cfg.exclusive { 30 } else { 100 };
        let initial_vol = cfg.initial_volume.min(100);
        let (player, events) = Player::new(cfg)?;
        let st = player.status();
        let volume = if st.volume <= 100 {
            st.volume
        } else {
            initial_vol
        };
        Ok(Self {
            player,
            events,
            active_session_id: 0,
            q: Playlist::default(),
            state: "NO_MEDIA_PRESENT".into(),
            status: "OK".into(),
            pos_secs: 0.0,
            duration_secs: 0.0,
            volume,
            default_volume,
            mute: st.muted,
            active_open: false,
            ended: false,
            queued_next_uri: None,
            quiet,
        })
    }

    fn log(&self, msg: &str) {
        if !self.quiet {
            eprintln!("tacet-dmr: {msg}");
        }
    }

    pub fn is_active(&self) -> bool {
        self.active_open
    }

    pub fn event_state(&mut self) -> EventState {
        let (num, _) = self.q.track_numbers();
        let next = self.q.next_track().uri;
        EventState {
            state: self.state.clone(),
            status: self.status.clone(),
            uri: self.q.current().uri,
            mode: self.q.mode.clone(),
            next,
            duration_sec: self.duration_secs.max(0.0).floor() as u64,
            track: num,
            volume: self.volume,
            mute: self.mute,
        }
    }

    /// Sync volume/mute from OS hardware if available.
    pub fn sync_volume(&mut self) {
        let (v, m) = self.player.sync_volume();
        self.volume = v;
        self.mute = m;
    }

    /// Refresh live position from `Player::position()` (O(1), lock-free).
    pub fn sample_position(&mut self) {
        if !self.active_open {
            return;
        }
        let p = self.player.position();
        if p.duration_secs > 0.0 {
            self.duration_secs = p.duration_secs;
        }
        if !self.ended {
            self.pos_secs = p.position_secs.max(0.0);
        }
    }

    /// Drain pending `PlaybackEvent`s from `tacet-audio` and update transport state.
    /// Returns `true` if any observable event state changed.
    pub fn poll_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.events.try_recv() {
            // Ignore stale events left over from a cancelled/previous session.
            if self.active_session_id == 0 || ev.session_id() != self.active_session_id {
                continue;
            }
            changed = true;
            match ev {
                PlaybackEvent::Ready {
                    spec,
                    duration_secs,
                    active,
                    ..
                } => {
                    if let Some(d) = duration_secs {
                        self.duration_secs = d;
                    }
                    self.status = "OK".into();
                    self.ended = false;
                    self.log(&format!(
                        "output: {} ({} Hz, {} ch, {}, exclusive={})",
                        active.device_name,
                        active.sample_rate,
                        spec.channels,
                        active.sample_format,
                        active.exclusive
                    ));
                    self.sync_gapless_next();
                }
                PlaybackEvent::TrackChanged {
                    url,
                    duration_secs,
                    active,
                    ..
                } => {
                    // Advance queue cursor to the track that just started playing gaplessly.
                    // Verify `url` matches the expected next track in case the queue was
                    // mutated during the ~1.5s pre-buffer window.
                    let expected_next = self.q.step(1, true);
                    if let Some(next_idx) = expected_next
                        && self.q.items.get(next_idx).is_some_and(|t| t.uri == url)
                    {
                        self.q.index = next_idx;
                        if self.q.name.is_empty() && self.q.items.len() > 1 {
                            // Generic AVTransport SetNextAVTransportURI: promote next to current.
                            let cur = self.q.items[next_idx].clone();
                            self.q.items = vec![cur];
                            self.q.index = 0;
                        }
                    } else if let Some(found_idx) = self.q.items.iter().position(|t| t.uri == url) {
                        self.q.index = found_idx;
                    } else if let Some(next_idx) = expected_next {
                        // The pre-buffered track was removed mid-buffer; switch to the intended next track.
                        let _ = self.select_track(next_idx, "PLAYING");
                        continue;
                    }
                    self.pos_secs = 0.0;
                    self.duration_secs = duration_secs.unwrap_or(0.0);
                    self.ended = false;
                    self.queued_next_uri = None;
                    self.log(&format!(
                        "gapless next: {} ({} Hz, {})",
                        active.device_name, active.sample_rate, active.sample_format
                    ));
                    self.sync_gapless_next();
                }
                PlaybackEvent::TrackEnded { .. } => {
                    // If a successor exists (e.g. queued late or repeat), start it;
                    // otherwise settle cleanly into STOPPED.
                    if let Some(next_idx) = self.q.step(1, true)
                        && self.state == "PLAYING"
                    {
                        if self.q.name.is_empty() && self.q.items.len() > 1 {
                            let cur = self.q.items[next_idx].clone();
                            self.q.items = vec![cur];
                            let _ = self.select_track(0, "PLAYING");
                        } else {
                            let _ = self.select_track(next_idx, "PLAYING");
                        }
                    } else {
                        self.ended = true;
                        self.state = "STOPPED".into();
                        self.active_session_id = 0;
                        self.pos_secs = 0.0;
                        self.active_open = false;
                        self.queued_next_uri = None;
                        self.player.stop();
                    }
                }
                PlaybackEvent::Error { message: e, .. } => {
                    self.log(&format!("error: playback: {e}"));
                    self.status = "ERROR_OCCURRED".into();
                    self.state = "STOPPED".into();
                    self.active_session_id = 0;
                    self.active_open = false;
                    self.ended = false;
                    self.queued_next_uri = None;
                    self.player.stop();
                }
            }
        }
        changed
    }

    fn sync_gapless_next(&mut self) {
        if !self.active_open || self.ended {
            return;
        }
        let next = self.q.next_track();
        let next_opt = if next.uri.is_empty() {
            None
        } else {
            Some(next.uri)
        };
        if next_opt != self.queued_next_uri {
            match &next_opt {
                Some(u) => self.player.queue_next(u),
                None => self.player.clear_next(),
            }
            self.queued_next_uri = next_opt;
        }
    }

    fn release(&mut self) {
        self.active_session_id = 0;
        self.player.stop();
        while self.events.try_recv().is_ok() {}
        self.active_open = false;
        self.ended = false;
        self.queued_next_uri = None;
        self.state = if self.q.items.is_empty() {
            "NO_MEDIA_PRESENT".into()
        } else {
            "STOPPED".into()
        };
    }

    fn reset_track(&mut self) {
        self.release();
        self.pos_secs = 0.0;
        self.duration_secs = 0.0;
        self.status = "OK".into();
    }

    fn open_current(&mut self, paused: bool) -> Result<(), Fault> {
        let cur = self.q.current();
        if cur.uri.is_empty() {
            return Err(Fault::TRANSITION_UNAVAILABLE);
        }
        self.pos_secs = 0.0;
        self.duration_secs = 0.0;
        self.ended = false;
        self.queued_next_uri = None;
        let sid = self.player.open(&cur.uri, paused).map_err(|e| {
            self.log(&format!("error: open track: {e}"));
            self.status = "ERROR_OCCURRED".into();
            Fault::ACTION_FAILED
        })?;
        self.active_session_id = sid;
        self.active_open = true;
        self.status = "OK".into();
        self.state = if paused {
            "PAUSED_PLAYBACK".into()
        } else {
            "PLAYING".into()
        };
        self.sync_gapless_next();
        Ok(())
    }

    fn select_track(&mut self, index: usize, target_state: &str) -> Result<(), Fault> {
        if index >= self.q.items.len() {
            return Err(Fault::INVALID_ARGS);
        }
        self.q.index = index;
        match target_state {
            "PLAYING" => self.open_current(false),
            "PAUSED_PLAYBACK" => self.open_current(true),
            _ => {
                self.reset_track();
                Ok(())
            }
        }
    }

    /// Snapshot `BrowseQueue` without formatting the full XML under the lock.
    pub fn browse_snapshot(&self, queue_name: &str) -> Result<BrowseSnapshot, Fault> {
        snapshot_for_browse(&self.q, queue_name)
    }

    /// Execute a UPnP / LinkPlay SOAP action.
    pub fn call(&mut self, service: &str, name: &str, args: &Fields) -> Result<Fields, Fault> {
        self.poll_events();

        if service == PQ {
            return self.queue_action(name, args);
        }

        if service != CM {
            let inst = args.get("InstanceID").map(String::as_str).unwrap_or("");
            if inst != "0" {
                let code = if service == RC { 702 } else { 718 };
                return Err(Fault {
                    code,
                    desc: "Invalid InstanceID",
                });
            }
        }

        if let Some(ch) = args.get("Channel")
            && ch != "Master"
        {
            return Err(Fault::INVALID_ARGS);
        }

        let cur = self.q.current();
        let (num, nr) = self.q.track_numbers();

        match name {
            // --- AVTransport:1 ---
            "SetAVTransportURI" => {
                let uri = args.get("CurrentURI").map(String::as_str).unwrap_or("");
                if !uri.is_empty() && !valid_uri(uri) {
                    return Err(Fault {
                        code: 714,
                        desc: "Illegal MIME-Type or unsupported URI",
                    });
                }
                self.q = Playlist {
                    mode: NORMAL.into(),
                    ..Default::default()
                };
                if !uri.is_empty() {
                    let meta = args.get("CurrentURIMetaData").cloned().unwrap_or_default();
                    self.q.items.push(Track {
                        uri: uri.to_string(),
                        metadata: meta,
                    });
                }
                self.reset_track();
                Ok(Fields::new())
            }
            "SetNextAVTransportURI" => {
                if self.q.items.is_empty() || !self.q.name.is_empty() {
                    return Err(Fault::TRANSITION_UNAVAILABLE);
                }
                let uri = args.get("NextURI").map(String::as_str).unwrap_or("");
                if !uri.is_empty() && !valid_uri(uri) {
                    return Err(Fault {
                        code: 714,
                        desc: "Unsupported URI",
                    });
                }
                self.q.items = vec![cur];
                self.q.index = 0;
                if !uri.is_empty() {
                    let meta = args.get("NextURIMetaData").cloned().unwrap_or_default();
                    self.q.items.push(Track {
                        uri: uri.to_string(),
                        metadata: meta,
                    });
                }
                self.q.rebuild_order();
                self.sync_gapless_next();
                Ok(Fields::new())
            }
            "Next" | "Previous" => {
                let delta = if name == "Previous" { -1 } else { 1 };
                let Some(next_i) = self.q.step(delta, false) else {
                    return Err(Fault::TRANSITION_UNAVAILABLE);
                };
                let target_state = self.state.clone();
                self.select_track(next_i, &target_state)?;
                Ok(Fields::new())
            }
            "SetPlayMode" => {
                let mode = args.get("NewPlayMode").map(String::as_str).unwrap_or("");
                if !is_valid_play_mode(mode) {
                    return Err(Fault::PLAY_MODE_UNSUPPORTED);
                }
                self.q.set_mode(mode);
                self.sync_gapless_next();
                Ok(Fields::new())
            }
            "GetCurrentTransportActions" => {
                let mut list: Vec<&str> = Vec::new();
                if num != 0 {
                    list.push("Play");
                    list.push("Stop");
                    if self.active_open {
                        list.push("Pause");
                        if !self.ended {
                            list.push("Seek");
                        }
                    }
                    if self.q.step(1, false).is_some() {
                        list.push("Next");
                    }
                    if self.q.step(-1, false).is_some() {
                        list.push("Previous");
                    }
                }
                Ok(Fields::from([("Actions".into(), list.join(","))]))
            }
            "Play" => {
                let speed = args.get("Speed").map(String::as_str).unwrap_or("");
                if speed != "1" {
                    return Err(Fault {
                        code: 717,
                        desc: "Play speed not supported",
                    });
                }
                if self.q.items.is_empty() {
                    return Err(Fault::TRANSITION_UNAVAILABLE);
                }
                if self.active_open && !self.ended {
                    self.player.play();
                    self.state = "PLAYING".into();
                    self.status = "OK".into();
                } else {
                    self.open_current(false)?;
                }
                Ok(Fields::new())
            }
            "Pause" => {
                if !self.active_open {
                    return Err(Fault::TRANSITION_UNAVAILABLE);
                }
                self.player.pause();
                if self.state != "PAUSED_PLAYBACK" {
                    self.log("pause");
                }
                self.state = "PAUSED_PLAYBACK".into();
                Ok(Fields::new())
            }
            "Stop" => {
                if self.active_open {
                    self.log("stop");
                }
                self.release();
                self.pos_secs = 0.0;
                self.status = "OK".into();
                Ok(Fields::new())
            }
            "Seek" => {
                let unit = args.get("Unit").map(String::as_str).unwrap_or("");
                if unit != "REL_TIME" && unit != "ABS_TIME" {
                    return Err(Fault::SEEK_MODE_UNSUPPORTED);
                }
                let target = args.get("Target").map(String::as_str).unwrap_or("");
                let secs = parse_time(target).ok_or(Fault::ILLEGAL_SEEK_TARGET)?;
                if !self.active_open || self.ended {
                    return Err(Fault::TRANSITION_UNAVAILABLE);
                }
                self.player
                    .seek(secs)
                    .map_err(|_| Fault::ILLEGAL_SEEK_TARGET)?;
                self.pos_secs = secs;
                Ok(Fields::new())
            }
            "GetTransportInfo" => Ok(Fields::from([
                ("CurrentTransportState".into(), self.state.clone()),
                ("CurrentTransportStatus".into(), self.status.clone()),
                ("CurrentSpeed".into(), "1".into()),
            ])),
            "GetPositionInfo" => {
                self.sample_position();
                let dur = format_timestamp(self.duration_secs.max(0.0).floor() as u64);
                let pos = format_timestamp(self.pos_secs.max(0.0).floor() as u64);
                Ok(Fields::from([
                    ("Track".into(), num.to_string()),
                    ("TrackDuration".into(), dur),
                    ("TrackMetaData".into(), cur.metadata),
                    ("TrackURI".into(), cur.uri),
                    ("RelTime".into(), pos.clone()),
                    ("AbsTime".into(), pos),
                    ("RelCount".into(), "2147483647".into()),
                    ("AbsCount".into(), "2147483647".into()),
                ]))
            }
            "GetMediaInfo" => {
                let next = self.q.next_track();
                if nr <= 1 && self.duration_secs == 0.0 {
                    self.sample_position();
                }
                let dur = if nr > 1 {
                    0
                } else {
                    self.duration_secs.max(0.0).floor() as u64
                };
                Ok(Fields::from([
                    ("NrTracks".into(), nr.to_string()),
                    ("MediaDuration".into(), format_timestamp(dur)),
                    ("CurrentURI".into(), cur.uri),
                    ("CurrentURIMetaData".into(), cur.metadata),
                    ("NextURI".into(), next.uri),
                    ("NextURIMetaData".into(), next.metadata),
                    ("PlayMedium".into(), "NETWORK".into()),
                    ("RecordMedium".into(), "NOT_IMPLEMENTED".into()),
                    ("WriteStatus".into(), "NOT_IMPLEMENTED".into()),
                ]))
            }
            "GetDeviceCapabilities" => Ok(Fields::from([
                ("PlayMedia".into(), "NETWORK".into()),
                ("RecMedia".into(), "NOT_IMPLEMENTED".into()),
                ("RecQualityModes".into(), "NOT_IMPLEMENTED".into()),
            ])),
            "GetTransportSettings" => Ok(Fields::from([
                ("PlayMode".into(), self.q.mode.clone()),
                ("RecQualityMode".into(), "NOT_IMPLEMENTED".into()),
            ])),

            // --- RenderingControl:1 ---
            "ListPresets" => Ok(Fields::from([(
                "CurrentPresetNameList".into(),
                "FactoryDefaults".into(),
            )])),
            "SelectPreset" => {
                if args.get("PresetName").map(String::as_str) != Some("FactoryDefaults") {
                    return Err(Fault::INVALID_ARGS);
                }
                let def = self.default_volume;
                self.player
                    .set_volume(def)
                    .map_err(|_| Fault::ACTION_FAILED)?;
                self.player
                    .set_mute(false)
                    .map_err(|_| Fault::ACTION_FAILED)?;
                self.sync_volume();
                self.mute = false;
                Ok(Fields::new())
            }
            "GetVolume" => {
                self.sync_volume();
                Ok(Fields::from([(
                    "CurrentVolume".into(),
                    self.volume.to_string(),
                )]))
            }
            "SetVolume" => {
                let v = args
                    .get("DesiredVolume")
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|&v| v <= 100)
                    .ok_or(Fault::INVALID_ARGS)?;
                self.player.set_volume(v).map_err(|e| {
                    self.log(&format!("error: volume write: {e}"));
                    Fault::ACTION_FAILED
                })?;
                self.sync_volume();
                self.volume = self.volume.min(100);
                Ok(Fields::new())
            }
            "GetMute" => {
                self.sync_volume();
                Ok(Fields::from([(
                    "CurrentMute".into(),
                    if self.mute { "1" } else { "0" }.into(),
                )]))
            }
            "SetMute" => {
                let raw = args.get("DesiredMute").map(String::as_str).unwrap_or("");
                let m = parse_upnp_bool(raw).ok_or(Fault::INVALID_ARGS)?;
                self.player.set_mute(m).map_err(|e| {
                    self.log(&format!("error: mute write: {e}"));
                    Fault::ACTION_FAILED
                })?;
                self.mute = m;
                Ok(Fields::new())
            }

            // --- ConnectionManager:1 ---
            "GetProtocolInfo" => Ok(Fields::from([
                ("Source".into(), String::new()),
                ("Sink".into(), sink_protocols()),
            ])),
            "GetCurrentConnectionIDs" => Ok(Fields::from([("ConnectionIDs".into(), "0".into())])),
            "GetCurrentConnectionInfo" => {
                if args.get("ConnectionID").map(String::as_str) != Some("0") {
                    return Err(Fault {
                        code: 706,
                        desc: "Invalid connection reference",
                    });
                }
                Ok(Fields::from([
                    ("RcsID".into(), "0".into()),
                    ("AVTransportID".into(), "0".into()),
                    ("ProtocolInfo".into(), String::new()),
                    ("PeerConnectionManager".into(), String::new()),
                    ("PeerConnectionID".into(), "-1".into()),
                    ("Direction".into(), "Input".into()),
                    ("Status".into(), "OK".into()),
                ]))
            }

            _ => Err(Fault::INVALID_ACTION),
        }
    }

    fn queue_action(&mut self, name: &str, args: &Fields) -> Result<Fields, Fault> {
        match name {
            "GetQueueLoopMode" => Ok(Fields::from([(
                "LoopMode".into(),
                mode_to_loop_code(&self.q.mode).into(),
            )])),
            "CreateQueue" | "AppendTracksInQueue" => {
                let raw = args.get("QueueContext").map(String::as_str).unwrap_or("");
                let (list, items) = parse_context(raw)?;
                if name == "CreateQueue" {
                    if list == TOTAL_QUEUE {
                        return Err(Fault::INVALID_ARGS);
                    }
                    self.q = Playlist {
                        name: list,
                        items,
                        index: 0,
                        mode: NORMAL.into(),
                        order: Vec::new(),
                        order_pos: None,
                    };
                    self.reset_track();
                    return Ok(Fields::new());
                }
                if !self.q.matches(&list) {
                    return Err(Fault::INVALID_ARGS);
                }
                if self.q.items.len() + items.len() > MAX_TRACKS
                    || track_bytes(&self.q.items) + track_bytes(&items) > MAX_QUEUE_BYTES
                {
                    return Err(Fault::QUEUE_CAPACITY);
                }
                let was_empty = self.q.items.is_empty();
                self.q.append_items(items);
                if was_empty {
                    self.reset_track();
                } else {
                    self.sync_gapless_next();
                }
                Ok(Fields::new())
            }
            "SetQueueLoopMode" => {
                if self.q.name.is_empty() {
                    return Err(Fault::INVALID_ARGS);
                }
                let code = args.get("LoopMode").map(String::as_str).unwrap_or("");
                let Some(mode) = loop_code_to_mode(code) else {
                    return Err(Fault::INVALID_ARGS);
                };
                self.q.set_mode(mode);
                self.sync_gapless_next();
                Ok(Fields::new())
            }
            _ => {
                let qname = args.get("QueueName").map(String::as_str).unwrap_or("");
                if !self.q.matches(qname) {
                    return Err(Fault::INVALID_ARGS);
                }
                match name {
                    "GetQueueIndex" => {
                        let n = if self.q.items.is_empty() {
                            0
                        } else {
                            self.q.index + 1
                        };
                        let preloading = if self.queued_next_uri.is_some() {
                            self.q.step(1, true).map(|i| i + 1).unwrap_or(0)
                        } else {
                            0
                        };
                        let page = if n > 0 { "1" } else { "0" };
                        Ok(Fields::from([
                            ("CurrentIndex".into(), n.to_string()),
                            ("PreloadingIndex".into(), preloading.to_string()),
                            ("CurrentPage".into(), page.into()),
                            ("TrackNums".into(), self.q.items.len().to_string()),
                        ]))
                    }
                    "PlayQueueWithIndex" => {
                        let raw = args.get("Index").map(String::as_str).unwrap_or("");
                        let idx =
                            parse_index(raw, self.q.items.len()).ok_or(Fault::INVALID_ARGS)?;
                        if self.q.is_shuffle() {
                            self.q.index = idx;
                            self.q.rebuild_order();
                        }
                        self.select_track(idx, "PLAYING")?;
                        Ok(Fields::new())
                    }
                    "RemoveTracksInQueue" => {
                        let lo = args
                            .get("RangStart")
                            .and_then(|s| parse_index(s, self.q.items.len()))
                            .ok_or(Fault::INVALID_ARGS)?;
                        let hi = args
                            .get("RangEnd")
                            .and_then(|s| parse_index(s, self.q.items.len()))
                            .ok_or(Fault::INVALID_ARGS)?;
                        if lo > hi || args.get("Action").map(String::as_str) != Some("0") {
                            return Err(Fault::INVALID_ARGS);
                        }
                        if self.q.remove(lo, hi) {
                            self.reset_track();
                        } else {
                            self.sync_gapless_next();
                        }
                        Ok(Fields::new())
                    }
                    "MoveTracksInQueue" => {
                        let from = args
                            .get("IndexList")
                            .and_then(|s| parse_index(s, self.q.items.len()))
                            .ok_or(Fault::INVALID_ARGS)?;
                        let to = args
                            .get("ToIndex")
                            .and_then(|s| parse_index(s, self.q.items.len()))
                            .ok_or(Fault::INVALID_ARGS)?;
                        self.q.move_item(from, to);
                        self.sync_gapless_next();
                        Ok(Fields::new())
                    }
                    _ => Err(Fault::INVALID_ACTION),
                }
            }
        }
    }

    pub fn close(&mut self) {
        self.player.close();
    }
}

fn parse_upnp_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "true" | "TRUE" | "yes" | "YES" => Some(true),
        "0" | "false" | "FALSE" | "no" | "NO" => Some(false),
        _ => None,
    }
}

/// Parse UPnP `H:MM:SS` (with optional `.frac`) into seconds.
fn parse_time(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let mut parts = raw.split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec_str = parts.next()?;
    if parts.next().is_some() || m >= 60 {
        return None;
    }
    let s: f64 = sec_str.parse().ok()?;
    if !(0.0..60.0).contains(&s) || !s.is_finite() {
        return None;
    }
    let total_whole = h.checked_mul(3600)?.checked_add(m * 60)?;
    Some(total_whole as f64 + s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_time_rejects_overflow_and_invalid_values() {
        assert_eq!(parse_time("0:01:30.5"), Some(90.5));
        assert_eq!(parse_time("9999999999999999999:00:00"), None);
        assert_eq!(parse_time("0:60:00"), None);
        assert_eq!(parse_time("0:00:NaN"), None);
    }

    fn test_transport(exclusive: bool) -> Transport {
        Transport::new(
            EngineConfig {
                audio_device: "null".into(),
                exclusive,
                initial_volume: if exclusive { 30 } else { 100 },
            },
            true,
        )
        .unwrap()
    }

    fn fields(pairs: &[(&str, &str)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn transport_exclusive_volume_safety_and_protocol_info_honesty() {
        let mut tr = test_transport(true);
        // Exclusive startup volume is conservative 30, not 100.
        let r = tr
            .call(
                RC,
                "GetVolume",
                &fields(&[("InstanceID", "0"), ("Channel", "Master")]),
            )
            .unwrap();
        assert_eq!(r["CurrentVolume"], "30");

        // SetVolume + SetMute + FactoryDefault resets to conservative 30.
        tr.call(
            RC,
            "SetVolume",
            &fields(&[
                ("InstanceID", "0"),
                ("Channel", "Master"),
                ("DesiredVolume", "75"),
            ]),
        )
        .unwrap();
        tr.call(
            RC,
            "SetMute",
            &fields(&[
                ("InstanceID", "0"),
                ("Channel", "Master"),
                ("DesiredMute", "1"),
            ]),
        )
        .unwrap();
        tr.call(
            RC,
            "SelectPreset",
            &fields(&[("InstanceID", "0"), ("PresetName", "FactoryDefaults")]),
        )
        .unwrap();
        let r = tr
            .call(
                RC,
                "GetVolume",
                &fields(&[("InstanceID", "0"), ("Channel", "Master")]),
            )
            .unwrap();
        assert_eq!(r["CurrentVolume"], "30");
        let r = tr
            .call(
                RC,
                "GetMute",
                &fields(&[("InstanceID", "0"), ("Channel", "Master")]),
            )
            .unwrap();
        assert_eq!(r["CurrentMute"], "0");

        // ConnectionManager GetProtocolInfo advertises only natively decoded formats.
        let info = tr.call(CM, "GetProtocolInfo", &Fields::new()).unwrap();
        let sink = &info["Sink"];
        assert!(sink.contains("audio/flac"));
        assert!(sink.contains("audio/wav"));
        assert!(!sink.contains("audio/opus"));
        assert!(!sink.contains("audio/L16"));
        assert!(!sink.contains("audio/dsf"));
    }

    #[test]
    fn transport_rejects_invalid_instance_action_and_loop_mode() {
        use crate::renderer::schema::AV;
        let mut tr = test_transport(false);
        assert_eq!(
            tr.call(AV, "GetTransportInfo", &fields(&[("InstanceID", "1")]))
                .unwrap_err()
                .code,
            Fault::INVALID_INSTANCE.code
        );
        assert_eq!(
            tr.call(AV, "NoSuchAction", &fields(&[("InstanceID", "0")]))
                .unwrap_err()
                .code,
            Fault::INVALID_ACTION.code
        );
        assert_eq!(
            tr.call(PQ, "SetQueueLoopMode", &fields(&[("LoopMode", "99")]))
                .unwrap_err()
                .code,
            Fault::INVALID_ARGS.code
        );
    }

    #[test]
    fn transport_queue_lifecycle_and_delete() {
        let mut tr = test_transport(false);
        let ctx = r#"<PlayList><ListName>Album</ListName><ListInfo><TrackNumber>2</TrackNumber></ListInfo><Tracks><Track1><URL>http://127.0.0.1:9/a.flac</URL><Metadata>&lt;DIDL/&gt;</Metadata></Track1><Track2><URL>http://127.0.0.1:9/b.flac</URL><Metadata>&lt;DIDL/&gt;</Metadata></Track2></Tracks></PlayList>"#;
        tr.call(PQ, "CreateQueue", &fields(&[("QueueContext", ctx)]))
            .unwrap();
        let qi = tr
            .call(PQ, "GetQueueIndex", &fields(&[("QueueName", "Album")]))
            .unwrap();
        assert_eq!(qi["CurrentIndex"], "1");
        assert_eq!(qi["TrackNums"], "2");

        tr.call(PQ, "SetQueueLoopMode", &fields(&[("LoopMode", "0")]))
            .unwrap();
        let ts = tr
            .call(
                crate::renderer::schema::AV,
                "GetTransportSettings",
                &fields(&[("InstanceID", "0")]),
            )
            .unwrap();
        assert_eq!(ts["PlayMode"], "REPEAT_ALL");

        tr.call(
            PQ,
            "RemoveTracksInQueue",
            &fields(&[
                ("QueueName", "Album"),
                ("RangStart", "1"),
                ("RangEnd", "2"),
                ("Action", "0"),
            ]),
        )
        .unwrap();
        assert!(tr.q.items.is_empty());
        assert_eq!(tr.state, "NO_MEDIA_PRESENT");
    }
}

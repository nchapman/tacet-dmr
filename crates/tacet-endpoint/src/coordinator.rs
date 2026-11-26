//! Unified [`EndpointCoordinator`] bridging OpenHome Media (`Product`, `Playlist`,
//! `Info`, `Time`, `Volume`, `Receiver`, `Sender`) and minimal UPnP AV / DLNA
//! (`AVTransport`, `RenderingControl`, `ConnectionManager`) onto a single
//! `tacet-audio` [`Player`] with clean source arbitration and shared volume.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver as StdReceiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openhome_core::{
    DeviceAttributes, Fault as OhFault, InfoCounters, ProductInfo, ReceiverState, SenderStatus,
    Source, SourceType, TrackDetails, TrackEntry, TrackTime, TransportState, VolumeCharacteristics,
    VolumeState,
};
use openhome_server::dispatch::{
    SLUG_INFO, SLUG_PLAYLIST, SLUG_PRODUCT, SLUG_RECEIVER, SLUG_SENDER, SLUG_TIME, SLUG_VOLUME,
    ServiceRegistry,
};
use openhome_server::{
    ActionResult, EventHub, InfoService, PlaylistService, ProductService, ReceiverService,
    SenderService, TimeService, VolumeService,
};
use openhome_songcast::{SongcastSender, SongcastUri};
use tacet_audio::{EngineConfig, PcmTap, PlaybackEvent, Player};
use tokio::sync::{Notify, mpsc as tokio_mpsc, watch};

use crate::songcast::{SongcastEvent, SongcastReceiverWorker, SongcastTap};
use crate::upnp::events::{
    EventState as UpnpEventState, SubRegistry, format_timestamp, parse_timestamp,
};
use crate::upnp::schema::{AV, CM, Fault as UpnpFault, RC, sink_protocols};

const DEFAULT_RECEIVER_PROTOCOL_INFO: &str = "ohz:*:*:*,ohm:*:*:*,ohu:*:*:*";
const MAX_URI_BYTES: usize = 8 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_PLAYLIST_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_READ_LIST_IDS: usize = 1024;
const TRACKS_MAX: u32 = 16_384;

pub struct CoordinatorInit {
    pub room: String,
    pub name: String,
    pub enable_upnp: bool,
    pub allow_local_files: bool,
    pub songcast_interface: Ipv4Addr,
    pub sender: Option<Arc<SongcastSender>>,
    pub sender_metadata: String,
    pub engine_config: EngineConfig,
    pub runtime: tokio::runtime::Handle,
    pub quiet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePlaybackOwner {
    None,
    Playlist,
    Receiver,
    UpnpAv,
}

struct CoordinatorState {
    // OpenHome Product
    product_info: ProductInfo,
    attributes: DeviceAttributes,
    sources: Vec<Source>,
    source_index: u32,
    source_xml_change_count: u32,
    standby: bool,

    // Active playback owner & player session tracking
    owner: ActivePlaybackOwner,
    active_session_id: u64,
    playlist_loaded_id: u32,
    playlist_queued_next_id: Option<u32>,

    // OpenHome Playlist
    tracks: Vec<TrackEntry>,
    total_track_bytes: usize,
    next_track_id: u32,
    id_array_token: u32,
    current_id: u32,
    transport_state: TransportState,
    repeat: bool,
    shuffle: bool,
    shuffle_order: Vec<u32>,
    shuffle_seed: u64,
    playlist_protocol_info: String,

    // OpenHome Info & Time
    counters: InfoCounters,
    current_uri: String,
    current_metadata: String,
    details: TrackDetails,
    metatext: String,
    time: TrackTime,

    // Shared Volume (OpenHome Volume:1 + UPnP RenderingControl:1)
    volume_chars: VolumeCharacteristics,
    volume_state: VolumeState,

    // OpenHome Receiver (Songcast)
    receiver_uri: String,
    receiver_metadata: String,
    receiver_state: ReceiverState,
    receiver_worker: Option<SongcastReceiverWorker>,

    // OpenHome Sender (Songcast)
    sender_enabled: bool,
    sender_presentation_url: String,
    sender_metadata: String,
    sender_audio: bool,
    sender_status: SenderStatus,
    sender_attributes: String,

    // Minimal UPnP AV / DLNA MediaRenderer:1
    upnp_state: &'static str,
    upnp_status: &'static str,
    upnp_uri: String,
    upnp_meta: String,
    upnp_next_uri: String,
    upnp_next_meta: String,
    upnp_duration_secs: f64,
    upnp_pos_secs: f64,
}

impl CoordinatorState {
    fn is_source_type_active(&self, ty: &SourceType) -> bool {
        self.sources
            .get(self.source_index as usize)
            .is_some_and(|s| &s.source_type == ty)
    }

    fn source_index_of(&self, ty: &SourceType) -> Option<u32> {
        self.sources
            .iter()
            .position(|s| &s.source_type == ty)
            .map(|i| i as u32)
    }

    fn require_playlist_active(&self) -> ActionResult<()> {
        if self.is_source_type_active(&SourceType::Playlist) {
            Ok(())
        } else {
            Err(OhFault::conflict("Playlist source is not active"))
        }
    }

    fn clear_info_and_time(&mut self) {
        self.current_uri.clear();
        self.current_metadata.clear();
        self.details = TrackDetails::default();
        self.metatext.clear();
        self.time.duration = 0;
        self.time.seconds = 0;
    }

    fn set_info_track(&mut self, uri: String, metadata: String) {
        self.current_uri = uri;
        self.current_metadata = metadata;
        self.details = TrackDetails::default();
        self.counters.track_count = self.counters.track_count.wrapping_add(1);
        self.time.track_count = self.counters.track_count;
        self.time.duration = 0;
        self.time.seconds = 0;
    }

    fn set_track_details(&mut self, details: TrackDetails) {
        self.time.duration = details.duration;
        self.details = details;
        self.counters.details_count = self.counters.details_count.wrapping_add(1);
    }

    fn select_track_by_idx(&mut self, pos: usize) {
        let entry = &self.tracks[pos];
        let id = entry.id;
        let uri = entry.uri.clone();
        let meta = entry.metadata.clone();
        self.current_id = id;
        self.set_info_track(uri, meta);
    }

    fn select_track_by_id(&mut self, id: u32) -> bool {
        if let Some(pos) = self.tracks.iter().position(|t| t.id == id) {
            self.select_track_by_idx(pos);
            true
        } else {
            false
        }
    }

    fn clear_current_track(&mut self) {
        self.current_id = 0;
        self.playlist_loaded_id = 0;
        self.playlist_queued_next_id = None;
        self.clear_info_and_time();
    }

    fn rebuild_shuffle_order(&mut self) {
        let mut order: Vec<u32> = self.tracks.iter().map(|t| t.id).collect();
        let mut s = self.shuffle_seed.wrapping_add(order.len() as u64).max(1);
        for i in (1..order.len()).rev() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let j = (s as usize) % (i + 1);
            order.swap(i, j);
        }
        self.shuffle_seed = s;
        if self.current_id != 0
            && let Some(pos) = order.iter().position(|&id| id == self.current_id)
        {
            order.swap(0, pos);
        }
        self.shuffle_order = order;
    }

    /// Peek the next playlist track ID without mutating state (used for gapless `queue_next`).
    fn peek_next_track_id(&mut self) -> Option<u32> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.shuffle {
            if self.shuffle_order.len() != self.tracks.len() {
                self.rebuild_shuffle_order();
            }
            match self
                .shuffle_order
                .iter()
                .position(|&id| id == self.current_id)
            {
                None => self.shuffle_order.first().copied(),
                Some(cur) if cur + 1 < self.shuffle_order.len() => {
                    Some(self.shuffle_order[cur + 1])
                }
                Some(_) if self.repeat => self.shuffle_order.first().copied(),
                Some(_) => None,
            }
        } else {
            match self.tracks.iter().position(|t| t.id == self.current_id) {
                None => self.tracks.first().map(|t| t.id),
                Some(cur) if cur + 1 < self.tracks.len() => Some(self.tracks[cur + 1].id),
                Some(_) if self.repeat => self.tracks.first().map(|t| t.id),
                Some(_) => None,
            }
        }
    }

    fn advance_next(&mut self) -> Option<u32> {
        let next_id = self.peek_next_track_id();
        match next_id {
            Some(id) if self.select_track_by_id(id) => Some(id),
            _ => {
                self.transport_state = TransportState::Stopped;
                None
            }
        }
    }

    fn advance_previous(&mut self) {
        if self.tracks.is_empty() {
            self.transport_state = TransportState::Stopped;
            return;
        }
        if self.shuffle {
            if self.shuffle_order.len() != self.tracks.len() {
                self.rebuild_shuffle_order();
            }
            let prev_id = match self
                .shuffle_order
                .iter()
                .position(|&id| id == self.current_id)
            {
                None => {
                    if self.repeat {
                        self.shuffle_order.last().copied()
                    } else {
                        self.shuffle_order.first().copied()
                    }
                }
                Some(cur) if cur > 0 => Some(self.shuffle_order[cur - 1]),
                Some(_) if self.repeat => self.shuffle_order.last().copied(),
                Some(_) => {
                    self.time.seconds = 0;
                    return;
                }
            };
            if let Some(id) = prev_id {
                let _ = self.select_track_by_id(id);
            }
            return;
        }

        match self.tracks.iter().position(|t| t.id == self.current_id) {
            None => {
                let target = if self.repeat {
                    self.tracks.len() - 1
                } else {
                    0
                };
                self.select_track_by_idx(target);
            }
            Some(cur) if cur > 0 => self.select_track_by_idx(cur - 1),
            Some(_) if self.repeat => {
                let last = self.tracks.len() - 1;
                self.select_track_by_idx(last);
            }
            Some(_) => self.time.seconds = 0,
        }
    }

    /// Recompute `Sender:1` status/audio flags; returns `true` if either changed.
    fn sync_sender_flags(&mut self) -> bool {
        if !self.sender_enabled {
            return false;
        }
        let status = if self.standby || !self.is_source_type_active(&SourceType::Playlist) {
            SenderStatus::Disabled
        } else {
            SenderStatus::Enabled
        };
        let audio =
            status == SenderStatus::Enabled && self.transport_state == TransportState::Playing;
        if self.sender_status != status || self.sender_audio != audio {
            self.sender_status = status;
            self.sender_audio = audio;
            true
        } else {
            false
        }
    }

    /// Switch source index, returning `(changed, stopped_songcast_worker, needs_player_stop)`.
    fn switch_source_index(&mut self, idx: u32) -> (bool, Option<SongcastReceiverWorker>, bool) {
        if self.source_index == idx {
            return (false, None, false);
        }
        let mut stop_player = false;
        if self.is_source_type_active(&SourceType::Playlist) {
            self.transport_state = TransportState::Stopped;
            self.playlist_loaded_id = 0;
            self.playlist_queued_next_id = None;
            if self.owner == ActivePlaybackOwner::Playlist {
                stop_player = true;
            }
        }
        if self.is_source_type_active(&SourceType::Receiver) {
            self.receiver_state = ReceiverState::Stopped;
            if self.owner == ActivePlaybackOwner::Receiver {
                stop_player = true;
            }
        }
        if self.is_source_type_active(&SourceType::UpnpAv) {
            if self.upnp_state != "NO_MEDIA_PRESENT" {
                self.upnp_state = "STOPPED";
            }
            self.upnp_pos_secs = 0.0;
            if self.owner == ActivePlaybackOwner::UpnpAv {
                stop_player = true;
            }
        }
        let rx_worker = self.receiver_worker.take();
        if stop_player || rx_worker.is_some() {
            self.owner = ActivePlaybackOwner::None;
            self.active_session_id = 0;
        }
        self.source_index = idx;
        self.standby = false;
        self.clear_info_and_time();
        self.sync_sender_flags();
        (true, rx_worker, stop_player)
    }

    fn upnp_event_state(&self) -> UpnpEventState {
        let track = if self.upnp_uri.is_empty() { 0 } else { 1 };
        UpnpEventState {
            state: self.upnp_state.to_string(),
            status: self.upnp_status.to_string(),
            uri: self.upnp_uri.clone(),
            mode: "NORMAL".into(),
            next: self.upnp_next_uri.clone(),
            duration_sec: self.upnp_duration_secs.max(0.0) as u64,
            track,
            volume: self.volume_state.volume.min(100) as u8,
            mute: self.volume_state.mute,
        }
    }
}

/// Unified OpenHome + UPnP AV coordinator owning the single [`Player`].
pub struct EndpointCoordinator {
    player: Arc<Player>,
    state: Mutex<CoordinatorState>,
    runtime: tokio::runtime::Handle,
    allow_local_files: bool,
    factory_default_volume: u8,
    songcast_interface: Ipv4Addr,
    songcast_tap: Mutex<Option<Arc<SongcastTap>>>,
    songcast_ev_tx: tokio_mpsc::UnboundedSender<SongcastEvent>,
    songcast_gen: AtomicU64,
    oh_events: EventHub,
    oh_registry: Mutex<Option<ServiceRegistry>>,
    upnp_subs: Mutex<SubRegistry>,
    upnp_wake: Notify,
    quiet: bool,
}

/// Bundled event channels returned alongside a new [`EndpointCoordinator`].
pub struct CoordinatorChannels {
    pub player_rx: StdReceiver<PlaybackEvent>,
    pub songcast_rx: tokio_mpsc::UnboundedReceiver<SongcastEvent>,
}

impl EndpointCoordinator {
    pub fn new(init: CoordinatorInit) -> Result<(Arc<Self>, CoordinatorChannels), String> {
        let factory_default_volume = if init.engine_config.exclusive {
            tacet_audio::EXCLUSIVE_SAFE_VOLUME
        } else {
            100
        };
        let (player, player_rx) = Player::new(init.engine_config)?;
        let (init_vol, init_mute) = player.volume_state();
        let player = Arc::new(player);

        let songcast_tap = init.sender.as_ref().map(|sender| {
            let tap = Arc::new(SongcastTap::new(Arc::clone(sender), init.runtime.clone()));
            player.set_pcm_tap(Some(Arc::clone(&tap) as Arc<dyn PcmTap>));
            tap
        });

        let mut sources = vec![
            Source {
                name: "Playlist".into(),
                source_type: SourceType::Playlist,
                visible: true,
                system_name: "Playlist".into(),
            },
            Source {
                name: "Songcast".into(),
                source_type: SourceType::Receiver,
                // Visible = true for Kazoo compatibility (see openhome-rs/docs/oracle.md).
                visible: true,
                system_name: "Receiver".into(),
            },
        ];
        if init.enable_upnp {
            sources.push(Source {
                name: "UPnP AV".into(),
                source_type: SourceType::UpnpAv,
                visible: true,
                system_name: "UpnpAv".into(),
            });
        }

        let volume_chars = VolumeCharacteristics {
            volume_max: 100,
            volume_unity: 100,
            volume_steps: 100,
            volume_milli_db_per_step: 500,
            balance_max: 0,
            fade_max: 0,
        };
        let volume_state = VolumeState {
            volume: u32::from(init_vol),
            mute: init_mute,
            balance: 0,
            fade: 0,
            volume_limit: 100,
        };

        let sender_enabled = init.sender.is_some();
        let initial_state = CoordinatorState {
            product_info: ProductInfo {
                manufacturer_name: "Tacet".into(),
                manufacturer_info: "Audiophile OpenHome & UPnP/DLNA audio endpoint".into(),
                manufacturer_url: "https://github.com/nchapman/tacet-endpoint".into(),
                manufacturer_image_uri: String::new(),
                model_name: "Tacet Endpoint".into(),
                model_info: "Tacet Audiophile Audio Endpoint".into(),
                model_url: "https://github.com/nchapman/tacet-endpoint".into(),
                model_image_uri: String::new(),
                product_room: init.room,
                product_name: init.name,
                product_info: "Tacet Endpoint".into(),
                product_url: String::new(),
                product_image_uri: String::new(),
            },
            attributes: DeviceAttributes {
                info: true,
                time: true,
                volume: true,
                sender: sender_enabled,
                receiver: true,
                credentials: false,
            },
            sources,
            source_index: 0,
            source_xml_change_count: 0,
            standby: false,
            owner: ActivePlaybackOwner::None,
            active_session_id: 0,
            playlist_loaded_id: 0,
            playlist_queued_next_id: None,
            tracks: Vec::new(),
            total_track_bytes: 0,
            next_track_id: 1,
            id_array_token: 1,
            current_id: 0,
            transport_state: TransportState::Stopped,
            repeat: false,
            shuffle: false,
            shuffle_order: Vec::new(),
            shuffle_seed: 0x9E37_79B9_7F4A_7C15,
            playlist_protocol_info: sink_protocols(),
            counters: InfoCounters::default(),
            current_uri: String::new(),
            current_metadata: String::new(),
            details: TrackDetails::default(),
            metatext: String::new(),
            time: TrackTime::default(),
            volume_chars,
            volume_state,
            receiver_uri: String::new(),
            receiver_metadata: String::new(),
            receiver_state: ReceiverState::Stopped,
            receiver_worker: None,
            sender_enabled,
            sender_presentation_url: String::new(),
            sender_metadata: init.sender_metadata,
            sender_audio: false,
            sender_status: if sender_enabled {
                SenderStatus::Enabled
            } else {
                SenderStatus::Disabled
            },
            sender_attributes: String::new(),
            upnp_state: "NO_MEDIA_PRESENT",
            upnp_status: "OK",
            upnp_uri: String::new(),
            upnp_meta: String::new(),
            upnp_next_uri: String::new(),
            upnp_next_meta: String::new(),
            upnp_duration_secs: 0.0,
            upnp_pos_secs: 0.0,
        };

        let initial_upnp_ev = initial_state.upnp_event_state();
        let (songcast_ev_tx, songcast_ev_rx) = tokio_mpsc::unbounded_channel();

        let coord = Arc::new(Self {
            player,
            state: Mutex::new(initial_state),
            runtime: init.runtime,
            allow_local_files: init.allow_local_files,
            factory_default_volume,
            songcast_interface: init.songcast_interface,
            songcast_tap: Mutex::new(songcast_tap),
            songcast_ev_tx,
            songcast_gen: AtomicU64::new(0),
            oh_events: EventHub::new(),
            oh_registry: Mutex::new(None),
            upnp_subs: Mutex::new(SubRegistry::new(initial_upnp_ev)),
            upnp_wake: Notify::new(),
            quiet: init.quiet,
        });

        Ok((
            coord,
            CoordinatorChannels {
                player_rx,
                songcast_rx: songcast_ev_rx,
            },
        ))
    }

    /// Build the [`ServiceRegistry`] exposing all OpenHome services backed by this coordinator.
    pub fn build_openhome_registry(self: &Arc<Self>) -> ServiceRegistry {
        let sender_enabled = self.state.lock().unwrap().sender_enabled;
        let registry = ServiceRegistry {
            product: Arc::clone(self) as Arc<dyn ProductService>,
            playlist: Some(Arc::clone(self) as Arc<dyn PlaylistService>),
            info: Some(Arc::clone(self) as Arc<dyn InfoService>),
            time: Some(Arc::clone(self) as Arc<dyn TimeService>),
            volume: Some(Arc::clone(self) as Arc<dyn VolumeService>),
            receiver: Some(Arc::clone(self) as Arc<dyn ReceiverService>),
            sender: if sender_enabled {
                Some(Arc::clone(self) as Arc<dyn SenderService>)
            } else {
                None
            },
        };
        *self.oh_registry.lock().unwrap() = Some(registry.clone());
        registry
    }

    pub fn oh_events(&self) -> &EventHub {
        &self.oh_events
    }

    pub fn upnp_subs(&self) -> &Mutex<SubRegistry> {
        &self.upnp_subs
    }

    pub fn upnp_event_state(&self) -> UpnpEventState {
        self.state.lock().unwrap().upnp_event_state()
    }

    pub fn nudge_upnp(&self) {
        self.upnp_wake.notify_one();
    }

    pub fn notify_openhome(&self, slugs: &[&str]) {
        let Some(registry) = self.oh_registry.lock().unwrap().clone() else {
            return;
        };
        for &slug in slugs {
            if !self.oh_events.has_subscribers_for(slug) {
                continue;
            }
            if let Some(snap) = registry.state_snapshot(slug) {
                self.oh_events.notify_snapshot(slug, &snap);
            }
        }
    }

    fn publish_upnp_events(&self) {
        let ev = self.upnp_event_state();
        self.upnp_subs.lock().unwrap().publish(ev);
    }

    fn sync_tap_enabled(&self) {
        if let Some(tap) = self.songcast_tap.lock().unwrap().clone() {
            let st = self.state.lock().unwrap();
            let active = !st.standby && st.is_source_type_active(&SourceType::Playlist);
            drop(st);
            tap.set_enabled(active);
        }
    }

    /// Close all active playback, Songcast workers, and GENA subscriptions.
    pub fn shutdown(&self) {
        self.songcast_gen.fetch_add(1, Ordering::AcqRel);
        let worker = self.state.lock().unwrap().receiver_worker.take();
        if let Some(w) = worker {
            w.stop();
        }
        self.player.set_pcm_tap(None);
        self.player.close();
        self.songcast_tap.lock().unwrap().take();
        self.oh_registry.lock().unwrap().take();
        self.oh_events.clear();
        self.upnp_subs.lock().unwrap().close_all();
    }

    /// Open the current OpenHome Playlist track on `tacet-audio` and queue the gapless next track.
    fn start_playlist_current_track(&self, paused: bool) {
        let (entry, rx_worker) = {
            let mut st = self.state.lock().unwrap();
            let rx_worker = st.receiver_worker.take();
            let id = st.current_id;
            let entry = if id != 0 {
                st.tracks.iter().find(|t| t.id == id).cloned()
            } else {
                None
            };
            if entry.is_some() {
                st.owner = ActivePlaybackOwner::Playlist;
                st.playlist_loaded_id = id;
                st.playlist_queued_next_id = None;
                st.transport_state = if paused {
                    TransportState::Paused
                } else {
                    TransportState::Buffering
                };
            } else {
                st.owner = ActivePlaybackOwner::None;
                st.active_session_id = 0;
                st.playlist_loaded_id = 0;
                st.playlist_queued_next_id = None;
                st.transport_state = TransportState::Stopped;
            }
            st.sync_sender_flags();
            (entry, rx_worker)
        };

        if let Some(w) = rx_worker {
            w.stop();
        }
        self.sync_tap_enabled();

        let Some(entry) = entry else {
            self.player.stop();
            return;
        };

        if let Some(tap) = self.songcast_tap.lock().unwrap().clone() {
            tap.set_track(entry.uri.clone(), entry.metadata.clone());
        }

        match self.player.open(&entry.uri, paused) {
            Ok(sid) => {
                {
                    let mut st = self.state.lock().unwrap();
                    if st.owner == ActivePlaybackOwner::Playlist
                        && st.playlist_loaded_id == entry.id
                    {
                        st.active_session_id = sid;
                    }
                }
                self.sync_playlist_gapless_next();
            }
            Err(err) => {
                if !self.quiet {
                    eprintln!("tacet-endpoint: open failed: {err}");
                }
                let mut st = self.state.lock().unwrap();
                st.transport_state = TransportState::Stopped;
                st.playlist_loaded_id = 0;
                st.owner = ActivePlaybackOwner::None;
                st.active_session_id = 0;
                st.sync_sender_flags();
            }
        }
    }

    /// Synchronize `player.queue_next` with the next track in the OpenHome Playlist.
    fn sync_playlist_gapless_next(&self) {
        let next = {
            let mut st = self.state.lock().unwrap();
            if st.owner != ActivePlaybackOwner::Playlist
                || !matches!(
                    st.transport_state,
                    TransportState::Playing | TransportState::Buffering | TransportState::Paused
                )
            {
                st.playlist_queued_next_id = None;
                return;
            }
            let next_id = st.peek_next_track_id();
            if next_id == st.playlist_queued_next_id {
                return;
            }
            st.playlist_queued_next_id = next_id;
            next_id.and_then(|id| st.tracks.iter().find(|t| t.id == id).map(|t| t.uri.clone()))
        };
        match next {
            Some(uri) => self.player.queue_next(&uri),
            None => self.player.clear_next(),
        }
    }

    fn stop_active_playback(&self) {
        self.songcast_gen.fetch_add(1, Ordering::AcqRel);
        let rx_worker = {
            let mut st = self.state.lock().unwrap();
            st.owner = ActivePlaybackOwner::None;
            st.active_session_id = 0;
            st.playlist_loaded_id = 0;
            st.playlist_queued_next_id = None;
            st.sync_sender_flags();
            st.receiver_worker.take()
        };
        if let Some(w) = rx_worker {
            w.stop();
        }
        self.player.stop();
        self.sync_tap_enabled();
    }

    fn start_songcast_stream(&self, uri_str: &str) {
        let prev_worker = {
            let mut st = self.state.lock().unwrap();
            st.owner = ActivePlaybackOwner::Receiver;
            st.active_session_id = 0;
            st.playlist_loaded_id = 0;
            st.playlist_queued_next_id = None;
            st.receiver_state = ReceiverState::Waiting;
            st.sync_sender_flags();
            st.receiver_worker.take()
        };
        if let Some(w) = prev_worker {
            w.stop();
        }
        self.player.stop();
        self.sync_tap_enabled();

        let Ok(parsed) = SongcastUri::parse(uri_str) else {
            let mut st = self.state.lock().unwrap();
            st.receiver_state = ReceiverState::Stopped;
            st.owner = ActivePlaybackOwner::None;
            return;
        };

        let generation = self.songcast_gen.fetch_add(1, Ordering::AcqRel) + 1;
        match SongcastReceiverWorker::spawn(
            generation,
            parsed,
            self.songcast_interface,
            Arc::clone(&self.player),
            self.runtime.clone(),
            self.songcast_ev_tx.clone(),
        ) {
            Ok(worker) => {
                let mut st = self.state.lock().unwrap();
                if self.songcast_gen.load(Ordering::Acquire) == generation
                    && st.owner == ActivePlaybackOwner::Receiver
                {
                    st.receiver_worker = Some(worker);
                } else {
                    drop(st);
                    worker.stop();
                }
            }
            Err(err) => {
                if !self.quiet {
                    eprintln!("tacet-endpoint: songcast start failed: {err}");
                }
                let mut st = self.state.lock().unwrap();
                st.receiver_state = ReceiverState::Stopped;
                st.owner = ActivePlaybackOwner::None;
            }
        }
    }

    /// Handle one `tacet-audio` [`PlaybackEvent`] from the background monitor task.
    fn handle_player_event(&self, ev: PlaybackEvent) {
        let sid = ev.session_id();
        enum FollowUp {
            None,
            OpenPlaylistCurrent,
            SyncPlaylistNext,
            SyncUpnpNext(Option<String>),
        }

        let mut follow = FollowUp::None;
        let mut notify_oh: Vec<&'static str> = Vec::new();
        let mut notify_upnp = false;
        let mut tap_track: Option<(String, String)> = None;

        {
            let mut st = self.state.lock().unwrap();
            if sid != st.active_session_id || sid == 0 {
                return;
            }
            match st.owner {
                ActivePlaybackOwner::Playlist => match ev {
                    PlaybackEvent::Ready {
                        spec,
                        duration_secs,
                        codec,
                        lossless,
                        active,
                        ..
                    } => {
                        if !self.quiet {
                            eprintln!(
                                "tacet-endpoint: [OpenHome] playing {} ({} Hz, {} ch, {}, exclusive={})",
                                active.device_name,
                                active.sample_rate,
                                spec.channels,
                                active.sample_format,
                                active.exclusive
                            );
                        }
                        st.set_track_details(TrackDetails {
                            duration: duration_secs.unwrap_or(0.0).max(0.0).round() as u32,
                            bit_rate: 0,
                            bit_depth: u32::from(spec.bits_per_sample),
                            sample_rate: spec.sample_rate,
                            lossless,
                            codec_name: codec,
                        });
                        if st.transport_state != TransportState::Paused {
                            st.transport_state = TransportState::Playing;
                        }
                        st.sync_sender_flags();
                        notify_oh.extend_from_slice(&[
                            SLUG_PLAYLIST,
                            SLUG_INFO,
                            SLUG_TIME,
                            SLUG_SENDER,
                        ]);
                    }
                    PlaybackEvent::TrackChanged {
                        spec,
                        duration_secs,
                        codec,
                        lossless,
                        ..
                    } => {
                        let next_id = st.playlist_queued_next_id.take();
                        let selected = next_id.filter(|&id| st.select_track_by_id(id));
                        if let Some(id) = selected {
                            st.playlist_loaded_id = id;
                            tap_track = Some((st.current_uri.clone(), st.current_metadata.clone()));
                        } else if let Some(id) = st.advance_next() {
                            st.playlist_loaded_id = id;
                            tap_track = Some((st.current_uri.clone(), st.current_metadata.clone()));
                        }
                        st.set_track_details(TrackDetails {
                            duration: duration_secs.unwrap_or(0.0).max(0.0).round() as u32,
                            bit_rate: 0,
                            bit_depth: u32::from(spec.bits_per_sample),
                            sample_rate: spec.sample_rate,
                            lossless,
                            codec_name: codec,
                        });
                        st.transport_state = TransportState::Playing;
                        st.sync_sender_flags();
                        follow = FollowUp::SyncPlaylistNext;
                        notify_oh.extend_from_slice(&[
                            SLUG_PLAYLIST,
                            SLUG_INFO,
                            SLUG_TIME,
                            SLUG_SENDER,
                        ]);
                    }
                    PlaybackEvent::TrackEnded { .. } => {
                        st.playlist_queued_next_id = None;
                        if st.advance_next().is_some() {
                            follow = FollowUp::OpenPlaylistCurrent;
                        } else {
                            st.transport_state = TransportState::Stopped;
                            st.playlist_loaded_id = 0;
                            st.owner = ActivePlaybackOwner::None;
                            st.active_session_id = 0;
                            st.time.seconds = 0;
                        }
                        st.sync_sender_flags();
                        notify_oh.extend_from_slice(&[
                            SLUG_PLAYLIST,
                            SLUG_INFO,
                            SLUG_TIME,
                            SLUG_SENDER,
                        ]);
                    }
                    PlaybackEvent::Error { message, .. } => {
                        if !self.quiet {
                            eprintln!("tacet-endpoint: playback error: {message}");
                        }
                        st.transport_state = TransportState::Stopped;
                        st.playlist_loaded_id = 0;
                        st.playlist_queued_next_id = None;
                        st.owner = ActivePlaybackOwner::None;
                        st.active_session_id = 0;
                        st.sync_sender_flags();
                        notify_oh.extend_from_slice(&[
                            SLUG_PLAYLIST,
                            SLUG_INFO,
                            SLUG_TIME,
                            SLUG_SENDER,
                        ]);
                    }
                },
                ActivePlaybackOwner::UpnpAv => match ev {
                    PlaybackEvent::Ready {
                        spec,
                        duration_secs,
                        codec,
                        lossless,
                        active,
                        ..
                    } => {
                        if !self.quiet {
                            eprintln!(
                                "tacet-endpoint: [UPnP AV] playing {} ({} Hz, {} ch, {}, exclusive={})",
                                active.device_name,
                                active.sample_rate,
                                spec.channels,
                                active.sample_format,
                                active.exclusive
                            );
                        }
                        let dur = duration_secs.unwrap_or(0.0).max(0.0);
                        st.upnp_duration_secs = dur;
                        st.upnp_status = "OK";
                        if st.upnp_state != "PAUSED_PLAYBACK" {
                            st.upnp_state = "PLAYING";
                        }
                        st.set_track_details(TrackDetails {
                            duration: dur.round() as u32,
                            bit_rate: 0,
                            bit_depth: u32::from(spec.bits_per_sample),
                            sample_rate: spec.sample_rate,
                            lossless,
                            codec_name: codec,
                        });
                        notify_upnp = true;
                        notify_oh.extend_from_slice(&[SLUG_INFO, SLUG_TIME]);
                    }
                    PlaybackEvent::TrackChanged {
                        url,
                        spec,
                        duration_secs,
                        codec,
                        lossless,
                        ..
                    } => {
                        let promoted_meta = if st.upnp_next_uri == url {
                            std::mem::take(&mut st.upnp_next_meta)
                        } else {
                            String::new()
                        };
                        st.upnp_uri = url.clone();
                        st.upnp_meta = promoted_meta.clone();
                        st.upnp_next_uri.clear();
                        st.upnp_next_meta.clear();
                        let dur = duration_secs.unwrap_or(0.0).max(0.0);
                        st.upnp_duration_secs = dur;
                        st.upnp_pos_secs = 0.0;
                        st.upnp_state = "PLAYING";
                        st.upnp_status = "OK";
                        st.set_info_track(url, promoted_meta);
                        st.set_track_details(TrackDetails {
                            duration: dur.round() as u32,
                            bit_rate: 0,
                            bit_depth: u32::from(spec.bits_per_sample),
                            sample_rate: spec.sample_rate,
                            lossless,
                            codec_name: codec,
                        });
                        follow = FollowUp::SyncUpnpNext(None);
                        notify_upnp = true;
                        notify_oh.extend_from_slice(&[SLUG_INFO, SLUG_TIME]);
                    }
                    PlaybackEvent::TrackEnded { .. } => {
                        st.upnp_state = "STOPPED";
                        st.upnp_pos_secs = 0.0;
                        st.owner = ActivePlaybackOwner::None;
                        st.active_session_id = 0;
                        st.time.seconds = 0;
                        notify_upnp = true;
                        notify_oh.push(SLUG_TIME);
                    }
                    PlaybackEvent::Error { message, .. } => {
                        if !self.quiet {
                            eprintln!("tacet-endpoint: [UPnP AV] playback error: {message}");
                        }
                        st.upnp_state = "STOPPED";
                        st.upnp_status = "ERROR_OCCURRED";
                        st.owner = ActivePlaybackOwner::None;
                        st.active_session_id = 0;
                        notify_upnp = true;
                    }
                },
                ActivePlaybackOwner::None | ActivePlaybackOwner::Receiver => {}
            }
        }

        if let Some((uri, meta)) = tap_track
            && let Some(tap) = self.songcast_tap.lock().unwrap().clone()
        {
            tap.set_track(uri, meta);
        }
        match follow {
            FollowUp::None => {}
            FollowUp::OpenPlaylistCurrent => self.start_playlist_current_track(false),
            FollowUp::SyncPlaylistNext => self.sync_playlist_gapless_next(),
            FollowUp::SyncUpnpNext(Some(uri)) => self.player.queue_next(&uri),
            FollowUp::SyncUpnpNext(None) => self.player.clear_next(),
        }
        if !notify_oh.is_empty() {
            self.notify_openhome(&notify_oh);
        }
        if notify_upnp {
            self.publish_upnp_events();
        }
    }

    fn handle_songcast_event(&self, ev: SongcastEvent) {
        let cur_gen = self.songcast_gen.load(Ordering::Acquire);
        let mut notify_oh: Vec<&'static str> = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            if st.owner != ActivePlaybackOwner::Receiver {
                return;
            }
            match ev {
                SongcastEvent::State { generation, state } if generation == cur_gen => {
                    st.receiver_state = state;
                    if state == ReceiverState::Stopped {
                        st.owner = ActivePlaybackOwner::None;
                    }
                    notify_oh.extend_from_slice(&[SLUG_RECEIVER, SLUG_INFO]);
                }
                SongcastEvent::Track {
                    generation,
                    uri,
                    metadata,
                } if generation == cur_gen => {
                    st.set_info_track(uri, metadata);
                    notify_oh.extend_from_slice(&[SLUG_RECEIVER, SLUG_INFO, SLUG_TIME]);
                }
                SongcastEvent::Metatext {
                    generation,
                    metatext,
                } if generation == cur_gen => {
                    st.metatext = metatext;
                    st.counters.metatext_count = st.counters.metatext_count.wrapping_add(1);
                    notify_oh.push(SLUG_INFO);
                }
                SongcastEvent::Details {
                    generation,
                    details,
                } if generation == cur_gen => {
                    st.set_track_details(details);
                    notify_oh.extend_from_slice(&[SLUG_INFO, SLUG_TIME]);
                }
                SongcastEvent::Time {
                    generation,
                    seconds,
                } if generation == cur_gen => {
                    if st.time.seconds != seconds {
                        st.time.seconds = seconds;
                        notify_oh.push(SLUG_TIME);
                    }
                }
                SongcastEvent::Error {
                    generation,
                    message,
                } if generation == cur_gen => {
                    if !self.quiet {
                        eprintln!("tacet-endpoint: songcast error: {message}");
                    }
                    st.receiver_state = ReceiverState::Stopped;
                    st.owner = ActivePlaybackOwner::None;
                    notify_oh.extend_from_slice(&[SLUG_RECEIVER, SLUG_INFO]);
                }
                _ => return,
            }
        }
        if !notify_oh.is_empty() {
            self.notify_openhome(&notify_oh);
        }
    }

    /// Background monitor task pumping `tacet-audio` events, Songcast events,
    /// playback position ticks, hardware volume sync, and UPnP GENA expiry.
    pub async fn run_monitor(
        self: Arc<Self>,
        player_rx: StdReceiver<PlaybackEvent>,
        mut songcast_rx: tokio_mpsc::UnboundedReceiver<SongcastEvent>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut last_vol_sync = Instant::now();

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                Some(sc_ev) = songcast_rx.recv() => {
                    self.handle_songcast_event(sc_ev);
                }
                _ = self.upnp_wake.notified() => {
                    self.publish_upnp_events();
                }
                _ = tick.tick() => {
                    // Drain any pending tacet-audio events.
                    while let Ok(ev) = player_rx.try_recv() {
                        self.handle_player_event(ev);
                    }

                    // Update playback position when Playlist or UPnP AV is playing.
                    let (owner, playing) = {
                        let st = self.state.lock().unwrap();
                        let playing = match st.owner {
                            ActivePlaybackOwner::Playlist => {
                                st.transport_state == TransportState::Playing
                            }
                            ActivePlaybackOwner::UpnpAv => st.upnp_state == "PLAYING",
                            _ => false,
                        };
                        (st.owner, playing)
                    };
                    if playing {
                        let pos = self.player.position();
                        let sec = pos.position_secs.max(0.0) as u32;
                        let mut time_changed = false;
                        {
                            let mut st = self.state.lock().unwrap();
                            if st.owner == owner {
                                if owner == ActivePlaybackOwner::UpnpAv {
                                    st.upnp_pos_secs = pos.position_secs.max(0.0);
                                    if pos.duration_secs > 0.0 {
                                        st.upnp_duration_secs = pos.duration_secs;
                                    }
                                }
                                if st.time.seconds != sec {
                                    st.time.seconds = sec;
                                    time_changed = true;
                                }
                            }
                        }
                        if time_changed {
                            self.notify_openhome(&[SLUG_TIME]);
                        }
                    }

                    // Sync OS hardware volume every 500 ms and prune expired UPnP GENA subs.
                    let now = Instant::now();
                    if now.duration_since(last_vol_sync) >= Duration::from_millis(500) {
                        last_vol_sync = now;
                        self.upnp_subs.lock().unwrap().expire_stale(now);

                        let (hw_vol, hw_mute) = self.player.sync_volume();
                        let vol_changed = {
                            let mut st = self.state.lock().unwrap();
                            if st.volume_state.volume != u32::from(hw_vol)
                                || st.volume_state.mute != hw_mute
                            {
                                st.volume_state.volume = u32::from(hw_vol);
                                st.volume_state.mute = hw_mute;
                                true
                            } else {
                                false
                            }
                        };
                        if vol_changed {
                            self.notify_openhome(&[SLUG_VOLUME]);
                            self.publish_upnp_events();
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Minimal UPnP AV / DLNA MediaRenderer:1 action dispatcher
    // -----------------------------------------------------------------------

    pub fn call_upnp(
        &self,
        service: &str,
        action: &str,
        args: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, UpnpFault> {
        match service {
            AV => self.call_upnp_av(action, args),
            RC => self.call_upnp_rc(action, args),
            CM => self.call_upnp_cm(action, args),
            _ => Err(UpnpFault::INVALID_ACTION),
        }
    }

    fn require_instance_zero(args: &HashMap<String, String>) -> Result<(), UpnpFault> {
        match args.get("InstanceID").map(|s| s.trim()) {
            Some("0") => Ok(()),
            _ => Err(UpnpFault::INVALID_INSTANCE),
        }
    }

    fn call_upnp_av(
        &self,
        action: &str,
        args: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, UpnpFault> {
        Self::require_instance_zero(args)?;
        let mut out = HashMap::new();

        match action {
            "SetAVTransportURI" => {
                let uri = args
                    .get("CurrentURI")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let meta = args.get("CurrentURIMetaData").cloned().unwrap_or_default();
                if uri.len() > MAX_URI_BYTES || meta.len() > MAX_METADATA_BYTES {
                    return Err(UpnpFault::INVALID_ARGS);
                }
                if !uri.is_empty() && !self.allow_local_files && !is_http_uri(&uri) {
                    return Err(UpnpFault::INVALID_ARGS);
                }

                let (should_open, rx_worker, stop_player, switched_source) = {
                    let mut st = self.state.lock().unwrap();
                    let mut rx_worker = None;
                    let mut stop_player = false;
                    let mut switched_source = false;

                    if !uri.is_empty()
                        && let Some(idx) = st.source_index_of(&SourceType::UpnpAv)
                        && st.source_index != idx
                    {
                        let (ch, w, sp) = st.switch_source_index(idx);
                        switched_source = ch;
                        rx_worker = w;
                        stop_player = sp;
                    }

                    let was_playing = st.owner == ActivePlaybackOwner::UpnpAv
                        && matches!(st.upnp_state, "PLAYING" | "TRANSITIONING");
                    st.upnp_uri = uri.clone();
                    st.upnp_meta = meta.clone();
                    st.upnp_next_uri.clear();
                    st.upnp_next_meta.clear();
                    st.upnp_duration_secs = 0.0;
                    st.upnp_pos_secs = 0.0;
                    st.upnp_status = "OK";

                    if uri.is_empty() {
                        st.upnp_state = "NO_MEDIA_PRESENT";
                        if st.owner == ActivePlaybackOwner::UpnpAv {
                            st.owner = ActivePlaybackOwner::None;
                            st.active_session_id = 0;
                            stop_player = true;
                        }
                    } else {
                        st.set_info_track(uri.clone(), meta);
                        if was_playing {
                            st.upnp_state = "TRANSITIONING";
                        } else {
                            st.upnp_state = "STOPPED";
                            if st.owner == ActivePlaybackOwner::UpnpAv {
                                st.owner = ActivePlaybackOwner::None;
                                st.active_session_id = 0;
                                stop_player = true;
                            }
                        }
                    }
                    (
                        was_playing && !uri.is_empty(),
                        rx_worker,
                        stop_player,
                        switched_source,
                    )
                };

                if let Some(w) = rx_worker {
                    w.stop();
                }
                if should_open {
                    self.open_upnp_current(false)?;
                } else if stop_player {
                    self.player.stop();
                }
                self.sync_tap_enabled();
                self.publish_upnp_events();
                if switched_source {
                    self.notify_openhome(&[
                        SLUG_PRODUCT,
                        SLUG_PLAYLIST,
                        SLUG_RECEIVER,
                        SLUG_INFO,
                        SLUG_TIME,
                        SLUG_SENDER,
                    ]);
                } else {
                    self.notify_openhome(&[SLUG_INFO, SLUG_TIME]);
                }
            }
            "SetNextAVTransportURI" => {
                let next_uri = args
                    .get("NextURI")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let next_meta = args.get("NextURIMetaData").cloned().unwrap_or_default();
                if next_uri.len() > MAX_URI_BYTES || next_meta.len() > MAX_METADATA_BYTES {
                    return Err(UpnpFault::INVALID_ARGS);
                }
                if !next_uri.is_empty() && !self.allow_local_files && !is_http_uri(&next_uri) {
                    return Err(UpnpFault::INVALID_ARGS);
                }
                let is_active = {
                    let mut st = self.state.lock().unwrap();
                    st.upnp_next_uri = next_uri.clone();
                    st.upnp_next_meta = next_meta;
                    st.owner == ActivePlaybackOwner::UpnpAv
                };
                if is_active {
                    if next_uri.is_empty() {
                        self.player.clear_next();
                    } else {
                        self.player.queue_next(&next_uri);
                    }
                }
                self.publish_upnp_events();
            }
            "Play" => {
                if let Some(speed) = args.get("Speed")
                    && !speed.trim().is_empty()
                    && speed.trim() != "1"
                {
                    return Err(UpnpFault::INVALID_ARGS);
                }

                let (resume, has_uri, rx_worker, switched_source) = {
                    let mut st = self.state.lock().unwrap();
                    if st.upnp_uri.is_empty() {
                        return Err(UpnpFault::TRANSITION_UNAVAILABLE);
                    }
                    let mut rx_worker = None;
                    let mut switched_source = false;
                    if let Some(idx) = st.source_index_of(&SourceType::UpnpAv)
                        && st.source_index != idx
                    {
                        let (ch, w, _) = st.switch_source_index(idx);
                        switched_source = ch;
                        rx_worker = w;
                        let uri = st.upnp_uri.clone();
                        let meta = st.upnp_meta.clone();
                        st.set_info_track(uri, meta);
                    }
                    st.standby = false;
                    let resume = st.owner == ActivePlaybackOwner::UpnpAv
                        && st.upnp_state == "PAUSED_PLAYBACK"
                        && st.active_session_id != 0;
                    if resume {
                        st.upnp_state = "PLAYING";
                    } else {
                        st.upnp_state = "TRANSITIONING";
                    }
                    (resume, !st.upnp_uri.is_empty(), rx_worker, switched_source)
                };

                if let Some(w) = rx_worker {
                    w.stop();
                }
                self.sync_tap_enabled();

                if resume {
                    self.player.play();
                } else if has_uri {
                    self.open_upnp_current(false)?;
                }
                self.publish_upnp_events();
                if switched_source {
                    self.notify_openhome(&[
                        SLUG_PRODUCT,
                        SLUG_PLAYLIST,
                        SLUG_RECEIVER,
                        SLUG_INFO,
                        SLUG_TIME,
                        SLUG_SENDER,
                    ]);
                }
            }
            "Pause" => {
                let can_pause = {
                    let mut st = self.state.lock().unwrap();
                    if st.owner == ActivePlaybackOwner::UpnpAv
                        && matches!(st.upnp_state, "PLAYING" | "TRANSITIONING")
                    {
                        st.upnp_state = "PAUSED_PLAYBACK";
                        true
                    } else {
                        false
                    }
                };
                if !can_pause {
                    return Err(UpnpFault::TRANSITION_UNAVAILABLE);
                }
                self.player.pause();
                self.publish_upnp_events();
            }
            "Stop" => {
                let stop_player = {
                    let mut st = self.state.lock().unwrap();
                    if st.upnp_state != "NO_MEDIA_PRESENT" {
                        st.upnp_state = "STOPPED";
                    }
                    st.upnp_pos_secs = 0.0;
                    if st.owner == ActivePlaybackOwner::UpnpAv {
                        st.owner = ActivePlaybackOwner::None;
                        st.active_session_id = 0;
                        st.time.seconds = 0;
                        true
                    } else {
                        false
                    }
                };
                if stop_player {
                    self.player.stop();
                }
                self.publish_upnp_events();
            }
            "Seek" => {
                let unit = args.get("Unit").map(|s| s.trim()).unwrap_or("");
                if unit != "REL_TIME" {
                    return Err(UpnpFault::SEEK_MODE_UNSUPPORTED);
                }
                let target_str = args.get("Target").map(|s| s.trim()).unwrap_or("");
                let secs = parse_timestamp(target_str).ok_or(UpnpFault::ILLEGAL_SEEK_TARGET)?;

                let active = {
                    let st = self.state.lock().unwrap();
                    st.owner == ActivePlaybackOwner::UpnpAv && st.active_session_id != 0
                };
                if !active {
                    return Err(UpnpFault::TRANSITION_UNAVAILABLE);
                }
                self.player
                    .seek(secs)
                    .map_err(|_| UpnpFault::ACTION_FAILED)?;
                {
                    let mut st = self.state.lock().unwrap();
                    st.upnp_pos_secs = secs;
                    st.time.seconds = secs as u32;
                }
                self.notify_openhome(&[SLUG_TIME]);
            }
            "SetPlayMode" => {
                let mode = args.get("NewPlayMode").map(|s| s.trim()).unwrap_or("");
                if mode != "NORMAL" {
                    return Err(UpnpFault::PLAY_MODE_UNSUPPORTED);
                }
            }
            "GetCurrentTransportActions" => {
                let st = self.state.lock().unwrap();
                let actions = match st.upnp_state {
                    "PLAYING" => "Stop,Pause,Seek",
                    "PAUSED_PLAYBACK" => "Play,Stop,Seek",
                    "STOPPED" if !st.upnp_uri.is_empty() => "Play,Stop",
                    _ => "",
                };
                out.insert("Actions".into(), actions.into());
            }
            "GetTransportInfo" => {
                let st = self.state.lock().unwrap();
                out.insert("CurrentTransportState".into(), st.upnp_state.into());
                out.insert("CurrentTransportStatus".into(), st.upnp_status.into());
                out.insert("CurrentSpeed".into(), "1".into());
            }
            "GetPositionInfo" => {
                let (pos_secs, dur_secs, uri, meta) = {
                    let st = self.state.lock().unwrap();
                    let active = st.owner == ActivePlaybackOwner::UpnpAv
                        && matches!(st.upnp_state, "PLAYING" | "PAUSED_PLAYBACK");
                    let p = if active {
                        self.player.position().position_secs.max(0.0)
                    } else {
                        st.upnp_pos_secs
                    };
                    (
                        p,
                        st.upnp_duration_secs,
                        st.upnp_uri.clone(),
                        st.upnp_meta.clone(),
                    )
                };
                let track = if uri.is_empty() { "0" } else { "1" };
                let rel = format_timestamp(pos_secs as u64);
                let dur = format_timestamp(dur_secs.max(0.0) as u64);
                out.insert("Track".into(), track.into());
                out.insert("TrackDuration".into(), dur);
                out.insert("TrackMetaData".into(), meta);
                out.insert("TrackURI".into(), uri);
                out.insert("RelTime".into(), rel.clone());
                out.insert("AbsTime".into(), rel);
                out.insert("RelCount".into(), "2147483647".into());
                out.insert("AbsCount".into(), "2147483647".into());
            }
            "GetMediaInfo" => {
                let st = self.state.lock().unwrap();
                let nr = if st.upnp_uri.is_empty() { "0" } else { "1" };
                let dur = format_timestamp(st.upnp_duration_secs.max(0.0) as u64);
                out.insert("NrTracks".into(), nr.into());
                out.insert("MediaDuration".into(), dur);
                out.insert("CurrentURI".into(), st.upnp_uri.clone());
                out.insert("CurrentURIMetaData".into(), st.upnp_meta.clone());
                out.insert("NextURI".into(), st.upnp_next_uri.clone());
                out.insert("NextURIMetaData".into(), st.upnp_next_meta.clone());
                out.insert("PlayMedium".into(), "NETWORK".into());
                out.insert("RecordMedium".into(), "NOT_IMPLEMENTED".into());
                out.insert("WriteStatus".into(), "NOT_IMPLEMENTED".into());
            }
            "GetDeviceCapabilities" => {
                out.insert("PlayMedia".into(), "NETWORK".into());
                out.insert("RecMedia".into(), "NOT_IMPLEMENTED".into());
                out.insert("RecQualityModes".into(), "NOT_IMPLEMENTED".into());
            }
            "GetTransportSettings" => {
                out.insert("PlayMode".into(), "NORMAL".into());
                out.insert("RecQualityMode".into(), "NOT_IMPLEMENTED".into());
            }
            _ => return Err(UpnpFault::INVALID_ACTION),
        }

        Ok(out)
    }

    fn open_upnp_current(&self, paused: bool) -> Result<(), UpnpFault> {
        let (uri, next_uri) = {
            let st = self.state.lock().unwrap();
            (st.upnp_uri.clone(), st.upnp_next_uri.clone())
        };
        if uri.is_empty() {
            return Err(UpnpFault::TRANSITION_UNAVAILABLE);
        }
        let sid = self.player.open(&uri, paused).map_err(|err| {
            if !self.quiet {
                eprintln!("tacet-endpoint: [UPnP AV] open failed: {err}");
            }
            let mut st = self.state.lock().unwrap();
            st.upnp_state = "STOPPED";
            st.upnp_status = "ERROR_OCCURRED";
            st.owner = ActivePlaybackOwner::None;
            st.active_session_id = 0;
            UpnpFault::ACTION_FAILED
        })?;
        {
            let mut st = self.state.lock().unwrap();
            st.owner = ActivePlaybackOwner::UpnpAv;
            st.active_session_id = sid;
        }
        if !next_uri.is_empty() {
            self.player.queue_next(&next_uri);
        }
        Ok(())
    }

    fn call_upnp_rc(
        &self,
        action: &str,
        args: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, UpnpFault> {
        Self::require_instance_zero(args)?;
        let mut out = HashMap::new();

        match action {
            "ListPresets" => {
                out.insert("CurrentPresetNameList".into(), "FactoryDefaults".into());
            }
            "SelectPreset" => {
                if args.get("PresetName").map(|s| s.trim()) != Some("FactoryDefaults") {
                    return Err(UpnpFault::INVALID_ARGS);
                }
                let _ = self.apply_volume(self.factory_default_volume);
                let _ = self.apply_mute(false);
            }
            "GetVolume" => {
                require_master_channel(args)?;
                let vol = self.state.lock().unwrap().volume_state.volume;
                out.insert("CurrentVolume".into(), vol.to_string());
            }
            "SetVolume" => {
                require_master_channel(args)?;
                let raw = args
                    .get("DesiredVolume")
                    .map(|s| s.trim())
                    .ok_or(UpnpFault::INVALID_ARGS)?;
                let vol: u8 = raw
                    .parse()
                    .ok()
                    .filter(|&v| v <= 100)
                    .ok_or(UpnpFault::INVALID_ARGS)?;
                self.apply_volume(vol)
                    .map_err(|_| UpnpFault::ACTION_FAILED)?;
            }
            "GetMute" => {
                require_master_channel(args)?;
                let mute = self.state.lock().unwrap().volume_state.mute;
                out.insert("CurrentMute".into(), if mute { "1" } else { "0" }.into());
            }
            "SetMute" => {
                require_master_channel(args)?;
                let raw = args
                    .get("DesiredMute")
                    .map(|s| s.trim().to_ascii_lowercase())
                    .ok_or(UpnpFault::INVALID_ARGS)?;
                let mute = match raw.as_str() {
                    "1" | "true" => true,
                    "0" | "false" => false,
                    _ => return Err(UpnpFault::INVALID_ARGS),
                };
                self.apply_mute(mute)
                    .map_err(|_| UpnpFault::ACTION_FAILED)?;
            }
            _ => return Err(UpnpFault::INVALID_ACTION),
        }

        Ok(out)
    }

    fn call_upnp_cm(
        &self,
        action: &str,
        args: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, UpnpFault> {
        let mut out = HashMap::new();
        match action {
            "GetProtocolInfo" => {
                out.insert("Source".into(), String::new());
                out.insert("Sink".into(), sink_protocols());
            }
            "GetCurrentConnectionIDs" => {
                out.insert("ConnectionIDs".into(), "0".into());
            }
            "GetCurrentConnectionInfo" => {
                if args.get("ConnectionID").map(|s| s.trim()) != Some("0") {
                    return Err(UpnpFault {
                        code: 706,
                        desc: "Invalid connection reference",
                    });
                }
                out.insert("RcsID".into(), "0".into());
                out.insert("AVTransportID".into(), "0".into());
                out.insert("ProtocolInfo".into(), String::new());
                out.insert("PeerConnectionManager".into(), String::new());
                out.insert("PeerConnectionID".into(), "-1".into());
                out.insert("Direction".into(), "Input".into());
                out.insert("Status".into(), "OK".into());
            }
            _ => return Err(UpnpFault::INVALID_ACTION),
        }
        Ok(out)
    }

    fn apply_volume(&self, vol: u8) -> Result<(), String> {
        let limit = self.state.lock().unwrap().volume_state.volume_limit;
        let clamped = u32::from(vol.min(100)).min(limit) as u8;
        self.player.set_volume(clamped)?;
        {
            let mut st = self.state.lock().unwrap();
            st.volume_state.volume = u32::from(clamped);
        }
        self.notify_openhome(&[SLUG_VOLUME]);
        self.publish_upnp_events();
        Ok(())
    }

    fn apply_mute(&self, mute: bool) -> Result<(), String> {
        self.player.set_mute(mute)?;
        {
            let mut st = self.state.lock().unwrap();
            st.volume_state.mute = mute;
        }
        self.notify_openhome(&[SLUG_VOLUME]);
        self.publish_upnp_events();
        Ok(())
    }
}

fn require_master_channel(args: &HashMap<String, String>) -> Result<(), UpnpFault> {
    match args.get("Channel").map(|s| s.trim()) {
        Some("Master") => Ok(()),
        _ => Err(UpnpFault::INVALID_ARGS),
    }
}

fn is_http_uri(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

// ---------------------------------------------------------------------------
// OpenHome Service Trait Implementations
// ---------------------------------------------------------------------------

impl ProductService for EndpointCoordinator {
    fn product_info(&self) -> ProductInfo {
        self.state.lock().unwrap().product_info.clone()
    }

    fn attributes(&self) -> DeviceAttributes {
        self.state.lock().unwrap().attributes.clone()
    }

    fn sources(&self) -> Vec<Source> {
        self.state.lock().unwrap().sources.clone()
    }

    fn source_index(&self) -> u32 {
        self.state.lock().unwrap().source_index
    }

    fn set_source_index(&self, index: u32) -> ActionResult<()> {
        let (changed, rx_worker, stop_player) = {
            let mut st = self.state.lock().unwrap();
            if (index as usize) >= st.sources.len() {
                return Err(OhFault::source_not_found());
            }
            st.switch_source_index(index)
        };
        if changed {
            self.songcast_gen.fetch_add(1, Ordering::AcqRel);
            if let Some(w) = rx_worker {
                w.stop();
            }
            if stop_player {
                self.player.stop();
            }
            self.sync_tap_enabled();
            self.publish_upnp_events();
            self.notify_openhome(&[
                SLUG_PRODUCT,
                SLUG_PLAYLIST,
                SLUG_RECEIVER,
                SLUG_INFO,
                SLUG_TIME,
                SLUG_SENDER,
            ]);
        }
        Ok(())
    }

    fn standby(&self) -> bool {
        self.state.lock().unwrap().standby
    }

    fn set_standby(&self, standby: bool) -> ActionResult<()> {
        let stop = {
            let mut st = self.state.lock().unwrap();
            st.standby = standby;
            if standby {
                st.transport_state = TransportState::Stopped;
                st.receiver_state = ReceiverState::Stopped;
                if st.upnp_state != "NO_MEDIA_PRESENT" {
                    st.upnp_state = "STOPPED";
                }
                st.sync_sender_flags();
                true
            } else {
                st.sync_sender_flags();
                false
            }
        };
        if stop {
            self.stop_active_playback();
            self.publish_upnp_events();
        } else {
            self.sync_tap_enabled();
        }
        self.notify_openhome(&[SLUG_SENDER]);
        Ok(())
    }

    fn source_xml_change_count(&self) -> u32 {
        self.state.lock().unwrap().source_xml_change_count
    }
}

impl PlaylistService for EndpointCoordinator {
    fn play(&self) -> ActionResult<()> {
        enum Action {
            Resume,
            Load,
            Stop,
        }
        let (act, rx_worker, switched_upnp) = {
            let mut st = self.state.lock().unwrap();
            let mut rx_worker = None;
            let mut switched_upnp = false;
            if !st.is_source_type_active(&SourceType::Playlist) {
                let was_upnp = st.is_source_type_active(&SourceType::UpnpAv);
                if let Some(idx) = st.source_index_of(&SourceType::Playlist) {
                    let (_, w, _) = st.switch_source_index(idx);
                    rx_worker = w;
                    switched_upnp = was_upnp;
                }
            }
            st.standby = false;
            if st.tracks.is_empty() {
                st.transport_state = TransportState::Stopped;
                st.sync_sender_flags();
                (Action::Stop, rx_worker, switched_upnp)
            } else {
                if st.current_id == 0 {
                    st.select_track_by_idx(0);
                }
                let can_resume = st.owner == ActivePlaybackOwner::Playlist
                    && st.transport_state == TransportState::Paused
                    && st.playlist_loaded_id == st.current_id
                    && st.active_session_id != 0;
                if can_resume {
                    st.transport_state = TransportState::Playing;
                    st.sync_sender_flags();
                    (Action::Resume, rx_worker, switched_upnp)
                } else {
                    (Action::Load, rx_worker, switched_upnp)
                }
            }
        };

        if let Some(w) = rx_worker {
            w.stop();
        }
        match act {
            Action::Resume => {
                self.player.play();
                self.sync_tap_enabled();
                self.notify_openhome(&[SLUG_SENDER]);
            }
            Action::Load => {
                self.start_playlist_current_track(false);
                self.notify_openhome(&[SLUG_SENDER]);
            }
            Action::Stop => {
                self.stop_active_playback();
                self.notify_openhome(&[SLUG_SENDER]);
            }
        }
        if switched_upnp {
            self.publish_upnp_events();
        }
        Ok(())
    }

    fn pause(&self) -> ActionResult<()> {
        let should_pause = {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            if matches!(
                st.transport_state,
                TransportState::Playing | TransportState::Buffering
            ) {
                st.transport_state = TransportState::Paused;
                st.sync_sender_flags();
                true
            } else {
                false
            }
        };
        if should_pause {
            self.player.pause();
            self.notify_openhome(&[SLUG_SENDER]);
        }
        Ok(())
    }

    fn stop(&self) -> ActionResult<()> {
        {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            st.transport_state = TransportState::Stopped;
            st.time.seconds = 0;
            st.sync_sender_flags();
        }
        self.stop_active_playback();
        self.notify_openhome(&[SLUG_SENDER]);
        Ok(())
    }

    fn next(&self) -> ActionResult<()> {
        let should_load = {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            let was_active = matches!(
                st.transport_state,
                TransportState::Playing | TransportState::Buffering
            );
            let advanced = st.advance_next().is_some();
            was_active && advanced
        };
        if should_load {
            self.start_playlist_current_track(false);
        } else {
            self.stop_active_playback();
        }
        self.notify_openhome(&[SLUG_SENDER]);
        Ok(())
    }

    fn previous(&self) -> ActionResult<()> {
        enum PrevAct {
            Load,
            Restart,
            Stop,
            None,
        }
        let act = {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            let prev_id = st.current_id;
            st.advance_previous();
            let new_id = st.current_id;
            match st.transport_state {
                TransportState::Playing | TransportState::Buffering => {
                    if new_id != 0 && new_id == prev_id && st.playlist_loaded_id == new_id {
                        PrevAct::Restart
                    } else {
                        PrevAct::Load
                    }
                }
                TransportState::Paused => {
                    if new_id != prev_id {
                        PrevAct::Stop
                    } else if new_id != 0 && st.playlist_loaded_id == new_id {
                        PrevAct::Restart
                    } else {
                        PrevAct::None
                    }
                }
                TransportState::Stopped => {
                    if new_id != prev_id {
                        PrevAct::Stop
                    } else {
                        PrevAct::None
                    }
                }
            }
        };
        match act {
            PrevAct::Load => self.start_playlist_current_track(false),
            PrevAct::Restart => {
                let _ = self.player.seek(0.0);
                self.state.lock().unwrap().time.seconds = 0;
            }
            PrevAct::Stop => self.stop_active_playback(),
            PrevAct::None => {}
        }
        self.notify_openhome(&[SLUG_SENDER]);
        Ok(())
    }

    fn repeat(&self) -> bool {
        self.state.lock().unwrap().repeat
    }

    fn set_repeat(&self, value: bool) -> ActionResult<()> {
        {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            st.repeat = value;
        }
        self.sync_playlist_gapless_next();
        Ok(())
    }

    fn shuffle(&self) -> bool {
        self.state.lock().unwrap().shuffle
    }

    fn set_shuffle(&self, value: bool) -> ActionResult<()> {
        {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            st.shuffle = value;
            if value {
                st.rebuild_shuffle_order();
            } else {
                st.shuffle_order.clear();
            }
        }
        self.sync_playlist_gapless_next();
        Ok(())
    }

    fn seek_second_absolute(&self, seconds: u32) -> ActionResult<()> {
        {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            if st.time.duration > 0 && seconds > st.time.duration {
                return Err(OhFault::seek_failed());
            }
            st.time.seconds = seconds;
        }
        let _ = self.player.seek(f64::from(seconds));
        Ok(())
    }

    fn seek_second_relative(&self, delta_seconds: i32) -> ActionResult<()> {
        let target = {
            let mut st = self.state.lock().unwrap();
            st.require_playlist_active()?;
            let cur = i64::from(st.time.seconds);
            let next = (cur + i64::from(delta_seconds)).max(0) as u32;
            let clamped = if st.time.duration > 0 {
                next.min(st.time.duration)
            } else {
                next
            };
            st.time.seconds = clamped;
            clamped
        };
        let _ = self.player.seek(f64::from(target));
        Ok(())
    }

    fn seek_id(&self, id: u32) -> ActionResult<()> {
        let (rx_worker, switched_upnp) = {
            let mut st = self.state.lock().unwrap();
            let mut rx_worker = None;
            let mut switched_upnp = false;
            let pos = st
                .tracks
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(OhFault::invalid_id)?;
            if !st.is_source_type_active(&SourceType::Playlist) {
                let was_upnp = st.is_source_type_active(&SourceType::UpnpAv);
                if let Some(idx) = st.source_index_of(&SourceType::Playlist) {
                    let (_, w, _) = st.switch_source_index(idx);
                    rx_worker = w;
                    switched_upnp = was_upnp;
                }
            }
            st.standby = false;
            st.select_track_by_idx(pos);
            (rx_worker, switched_upnp)
        };
        if let Some(w) = rx_worker {
            w.stop();
        }
        self.start_playlist_current_track(false);
        self.notify_openhome(&[SLUG_SENDER]);
        if switched_upnp {
            self.publish_upnp_events();
        }
        Ok(())
    }

    fn seek_index(&self, index: u32) -> ActionResult<()> {
        let (rx_worker, switched_upnp) = {
            let mut st = self.state.lock().unwrap();
            let pos = index as usize;
            if pos >= st.tracks.len() {
                return Err(OhFault::invalid_index());
            }
            let mut rx_worker = None;
            let mut switched_upnp = false;
            if !st.is_source_type_active(&SourceType::Playlist) {
                let was_upnp = st.is_source_type_active(&SourceType::UpnpAv);
                if let Some(idx) = st.source_index_of(&SourceType::Playlist) {
                    let (_, w, _) = st.switch_source_index(idx);
                    rx_worker = w;
                    switched_upnp = was_upnp;
                }
            }
            st.standby = false;
            st.select_track_by_idx(pos);
            (rx_worker, switched_upnp)
        };
        if let Some(w) = rx_worker {
            w.stop();
        }
        self.start_playlist_current_track(false);
        self.notify_openhome(&[SLUG_SENDER]);
        if switched_upnp {
            self.publish_upnp_events();
        }
        Ok(())
    }

    fn transport_state(&self) -> TransportState {
        let st = self.state.lock().unwrap();
        if st.is_source_type_active(&SourceType::Playlist) {
            st.transport_state
        } else {
            TransportState::Stopped
        }
    }

    fn current_id(&self) -> u32 {
        self.state.lock().unwrap().current_id
    }

    fn read(&self, id: u32) -> ActionResult<TrackEntry> {
        self.state
            .lock()
            .unwrap()
            .tracks
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(OhFault::invalid_id)
    }

    fn read_list(&self, ids: &[u32]) -> Vec<TrackEntry> {
        let st = self.state.lock().unwrap();
        if ids.is_empty() || st.tracks.is_empty() {
            return Vec::new();
        }
        let index: HashMap<u32, &TrackEntry> = st.tracks.iter().map(|t| (t.id, t)).collect();
        let mut out = Vec::with_capacity(ids.len().min(MAX_READ_LIST_IDS));
        for &id in ids.iter().take(MAX_READ_LIST_IDS) {
            if let Some(&entry) = index.get(&id) {
                out.push(entry.clone());
            }
        }
        out
    }

    fn insert(&self, after_id: u32, uri: String, metadata: String) -> ActionResult<u32> {
        if !self.allow_local_files && !is_http_uri(&uri) {
            return Err(OhFault::invalid_args(
                "Only http:// and https:// track URIs are allowed",
            ));
        }
        if uri.len() > MAX_URI_BYTES {
            return Err(OhFault::invalid_args("Uri exceeds maximum allowed length"));
        }
        if metadata.len() > MAX_METADATA_BYTES {
            return Err(OhFault::invalid_args(
                "Metadata exceeds maximum allowed length",
            ));
        }
        let entry_bytes = uri.len() + metadata.len();

        let new_id = {
            let mut st = self.state.lock().unwrap();
            if st.tracks.len() >= TRACKS_MAX as usize {
                return Err(OhFault::playlist_full());
            }
            if st.total_track_bytes.saturating_add(entry_bytes) > MAX_PLAYLIST_TOTAL_BYTES {
                return Err(OhFault::playlist_full());
            }
            let insert_pos = if after_id == 0 {
                0
            } else {
                st.tracks
                    .iter()
                    .position(|t| t.id == after_id)
                    .map(|p| p + 1)
                    .ok_or_else(OhFault::invalid_id)?
            };
            let new_id = st.next_track_id;
            st.next_track_id = st.next_track_id.wrapping_add(1).max(1);
            st.tracks.insert(
                insert_pos,
                TrackEntry {
                    id: new_id,
                    uri,
                    metadata,
                },
            );
            st.total_track_bytes = st.total_track_bytes.saturating_add(entry_bytes);
            st.id_array_token = st.id_array_token.wrapping_add(1);
            if st.shuffle {
                st.shuffle_order.push(new_id);
            }
            new_id
        };

        self.sync_playlist_gapless_next();
        Ok(new_id)
    }

    fn delete_id(&self, id: u32) -> ActionResult<()> {
        enum DelAct {
            None,
            LoadCurrent,
            Stop,
        }
        let act = {
            let mut st = self.state.lock().unwrap();
            let pos = st
                .tracks
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(OhFault::invalid_id)?;
            let removed = st.tracks.remove(pos);
            st.total_track_bytes = st
                .total_track_bytes
                .saturating_sub(removed.uri.len() + removed.metadata.len());
            st.id_array_token = st.id_array_token.wrapping_add(1);
            st.shuffle_order.retain(|&tid| tid != id);
            if st.current_id == id {
                let was_playing = st.owner == ActivePlaybackOwner::Playlist
                    && matches!(
                        st.transport_state,
                        TransportState::Playing | TransportState::Buffering
                    );
                if st.tracks.is_empty() {
                    st.clear_current_track();
                    st.transport_state = TransportState::Stopped;
                    DelAct::Stop
                } else {
                    let next_pos = pos.min(st.tracks.len() - 1);
                    st.select_track_by_idx(next_pos);
                    if was_playing {
                        DelAct::LoadCurrent
                    } else {
                        DelAct::Stop
                    }
                }
            } else {
                DelAct::None
            }
        };

        match act {
            DelAct::LoadCurrent => self.start_playlist_current_track(false),
            DelAct::Stop => self.stop_active_playback(),
            DelAct::None => self.sync_playlist_gapless_next(),
        }
        Ok(())
    }

    fn delete_all(&self) -> ActionResult<()> {
        let stop_needed = {
            let mut st = self.state.lock().unwrap();
            let had_tracks = !st.tracks.is_empty();
            st.tracks.clear();
            st.shuffle_order.clear();
            st.total_track_bytes = 0;
            st.clear_current_track();
            st.transport_state = TransportState::Stopped;
            if had_tracks {
                st.id_array_token = st.id_array_token.wrapping_add(1);
            }
            st.owner == ActivePlaybackOwner::Playlist
        };
        if stop_needed {
            self.stop_active_playback();
        }
        Ok(())
    }

    fn tracks_max(&self) -> u32 {
        TRACKS_MAX
    }

    fn id_array(&self) -> (u32, Vec<u32>) {
        let st = self.state.lock().unwrap();
        let ids = st.tracks.iter().map(|t| t.id).collect();
        (st.id_array_token, ids)
    }

    fn id_array_changed(&self, token: u32) -> bool {
        self.state.lock().unwrap().id_array_token != token
    }

    fn protocol_info(&self) -> String {
        self.state.lock().unwrap().playlist_protocol_info.clone()
    }
}

impl InfoService for EndpointCoordinator {
    fn counters(&self) -> InfoCounters {
        self.state.lock().unwrap().counters
    }

    fn track(&self) -> (String, String) {
        let st = self.state.lock().unwrap();
        (st.current_uri.clone(), st.current_metadata.clone())
    }

    fn details(&self) -> TrackDetails {
        self.state.lock().unwrap().details.clone()
    }

    fn metatext(&self) -> String {
        self.state.lock().unwrap().metatext.clone()
    }
}

impl TimeService for EndpointCoordinator {
    fn time(&self) -> TrackTime {
        self.state.lock().unwrap().time
    }
}

impl VolumeService for EndpointCoordinator {
    fn characteristics(&self) -> VolumeCharacteristics {
        self.state.lock().unwrap().volume_chars
    }

    fn state(&self) -> VolumeState {
        self.state.lock().unwrap().volume_state
    }

    fn set_volume(&self, value: u32) -> ActionResult<()> {
        let clamped = {
            let st = self.state.lock().unwrap();
            if value > st.volume_chars.volume_max {
                return Err(OhFault::invalid_args("volume exceeds VolumeMax"));
            }
            value.min(st.volume_state.volume_limit) as u8
        };
        self.apply_volume(clamped)
            .map_err(|e| OhFault::new(501, e))?;
        Ok(())
    }

    fn volume_inc(&self) -> ActionResult<()> {
        let next = {
            let st = self.state.lock().unwrap();
            let cap = st.volume_state.volume_limit.min(st.volume_chars.volume_max);
            st.volume_state.volume.saturating_add(1).min(cap) as u8
        };
        self.apply_volume(next).map_err(|e| OhFault::new(501, e))?;
        Ok(())
    }

    fn volume_dec(&self) -> ActionResult<()> {
        let next = {
            let st = self.state.lock().unwrap();
            st.volume_state.volume.saturating_sub(1) as u8
        };
        self.apply_volume(next).map_err(|e| OhFault::new(501, e))?;
        Ok(())
    }

    fn set_mute(&self, mute: bool) -> ActionResult<()> {
        self.apply_mute(mute).map_err(|e| OhFault::new(501, e))?;
        Ok(())
    }

    fn set_balance(&self, value: i32) -> ActionResult<()> {
        let max = self.state.lock().unwrap().volume_chars.balance_max as i32;
        if value < -max || value > max {
            return Err(OhFault::invalid_args("balance out of range"));
        }
        self.state.lock().unwrap().volume_state.balance = value;
        Ok(())
    }

    fn set_fade(&self, value: i32) -> ActionResult<()> {
        let max = self.state.lock().unwrap().volume_chars.fade_max as i32;
        if value < -max || value > max {
            return Err(OhFault::invalid_args("fade out of range"));
        }
        self.state.lock().unwrap().volume_state.fade = value;
        Ok(())
    }
}

impl ReceiverService for EndpointCoordinator {
    fn play(&self) -> ActionResult<()> {
        let (uri, rx_worker, switched_upnp) = {
            let mut st = self.state.lock().unwrap();
            let mut rx_worker = None;
            let mut switched_upnp = false;
            if !st.is_source_type_active(&SourceType::Receiver) {
                let was_upnp = st.is_source_type_active(&SourceType::UpnpAv);
                if let Some(idx) = st.source_index_of(&SourceType::Receiver) {
                    let (_, w, _) = st.switch_source_index(idx);
                    rx_worker = w;
                    switched_upnp = was_upnp;
                }
            }
            st.standby = false;
            let uri = st.receiver_uri.clone();
            if uri.is_empty() {
                st.receiver_state = ReceiverState::Stopped;
            }
            st.sync_sender_flags();
            (uri, rx_worker, switched_upnp)
        };

        if let Some(w) = rx_worker {
            w.stop();
        }
        if uri.is_empty() {
            self.stop_active_playback();
        } else {
            self.start_songcast_stream(&uri);
        }
        self.notify_openhome(&[SLUG_SENDER]);
        if switched_upnp {
            self.publish_upnp_events();
        }
        Ok(())
    }

    fn stop(&self) -> ActionResult<()> {
        // Per docs/oracle.md (BubbleUPnP compat): do not reset Product.SourceIndex.
        let (was_receiver, rx_worker) = {
            let mut st = self.state.lock().unwrap();
            st.receiver_state = ReceiverState::Stopped;
            let was_receiver = st.owner == ActivePlaybackOwner::Receiver;
            if was_receiver {
                st.owner = ActivePlaybackOwner::None;
                st.active_session_id = 0;
            }
            st.sync_sender_flags();
            (was_receiver, st.receiver_worker.take())
        };
        self.songcast_gen.fetch_add(1, Ordering::AcqRel);
        if let Some(w) = rx_worker {
            w.stop();
        }
        if was_receiver {
            self.player.stop();
        }
        self.notify_openhome(&[SLUG_SENDER]);
        Ok(())
    }

    fn set_sender(&self, uri: String, metadata: String) -> ActionResult<()> {
        if uri.len() > MAX_URI_BYTES {
            return Err(OhFault::invalid_args("Uri exceeds maximum allowed length"));
        }
        if metadata.len() > MAX_METADATA_BYTES {
            return Err(OhFault::invalid_args(
                "Metadata exceeds maximum allowed length",
            ));
        }
        if !uri.is_empty() {
            SongcastUri::parse(&uri)
                .map_err(|e| OhFault::invalid_args(format!("Invalid Songcast sender URI: {e}")))?;
        }
        let was_active = {
            let mut st = self.state.lock().unwrap();
            let active = st.receiver_state != ReceiverState::Stopped
                && st.is_source_type_active(&SourceType::Receiver);
            st.receiver_uri = uri.clone();
            st.receiver_metadata = metadata;
            active
        };
        if was_active {
            if uri.is_empty() {
                ReceiverService::stop(self)?;
            } else {
                self.start_songcast_stream(&uri);
            }
        }
        Ok(())
    }

    fn sender(&self) -> (String, String) {
        let st = self.state.lock().unwrap();
        (st.receiver_uri.clone(), st.receiver_metadata.clone())
    }

    fn protocol_info(&self) -> String {
        DEFAULT_RECEIVER_PROTOCOL_INFO.to_string()
    }

    fn transport_state(&self) -> ReceiverState {
        let st = self.state.lock().unwrap();
        if st.is_source_type_active(&SourceType::Receiver) {
            st.receiver_state
        } else {
            ReceiverState::Stopped
        }
    }
}

impl SenderService for EndpointCoordinator {
    fn presentation_url(&self) -> String {
        self.state.lock().unwrap().sender_presentation_url.clone()
    }

    fn metadata(&self) -> String {
        self.state.lock().unwrap().sender_metadata.clone()
    }

    fn audio(&self) -> bool {
        self.state.lock().unwrap().sender_audio
    }

    fn status(&self) -> SenderStatus {
        self.state.lock().unwrap().sender_status
    }

    fn attributes(&self) -> String {
        self.state.lock().unwrap().sender_attributes.clone()
    }
}

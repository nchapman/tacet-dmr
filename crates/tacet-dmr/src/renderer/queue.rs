//! PlayQueue:1 state, full LinkPlay LoopMode 0..5 <-> UPnP PlayMode mapping,
//! shuffle permutation with O(1) self-healing cursor cache, and incremental
//! QueueContext XML parsing.

use quick_xml::Reader;
use quick_xml::events::Event;
use rand::seq::SliceRandom;

use crate::renderer::schema::Fault;

pub const MAX_TRACKS: usize = 10_000;
pub const MAX_QUEUE_BYTES: usize = 32 << 20;
pub const CURRENT_QUEUE: &str = "CurrentQueue";
pub const TOTAL_QUEUE: &str = "TotalQueue";

pub const NORMAL: &str = "NORMAL";
pub const REPEAT_ONE: &str = "REPEAT_ONE";
pub const REPEAT_ALL: &str = "REPEAT_ALL";
pub const SHUFFLE_REPEAT_ALL: &str = "SHUFFLE";
pub const SHUFFLE_NO_REPEAT: &str = "SHUFFLE_NOREPEAT";
pub const SHUFFLE_REPEAT_ONE: &str = "SHUFFLE_REPEAT_ONE";

/// LinkPlay LoopMode <-> UPnP AVTransport CurrentPlayMode mapping:
/// 0: repeat all, 1: repeat one, 2: shuffle + repeat all,
/// 3: shuffle (no repeat), 4: normal, 5: shuffle + repeat one.
pub fn mode_to_loop_code(mode: &str) -> &'static str {
    match mode {
        REPEAT_ALL => "0",
        REPEAT_ONE => "1",
        SHUFFLE_REPEAT_ALL => "2",
        SHUFFLE_NO_REPEAT => "3",
        NORMAL => "4",
        SHUFFLE_REPEAT_ONE => "5",
        _ => "4",
    }
}

pub fn loop_code_to_mode(code: &str) -> Option<&'static str> {
    match code {
        "0" => Some(REPEAT_ALL),
        "1" => Some(REPEAT_ONE),
        "2" => Some(SHUFFLE_REPEAT_ALL),
        "3" => Some(SHUFFLE_NO_REPEAT),
        "4" => Some(NORMAL),
        "5" => Some(SHUFFLE_REPEAT_ONE),
        _ => None,
    }
}

pub fn is_valid_play_mode(mode: &str) -> bool {
    matches!(
        mode,
        NORMAL
            | REPEAT_ONE
            | REPEAT_ALL
            | SHUFFLE_REPEAT_ALL
            | SHUFFLE_NO_REPEAT
            | SHUFFLE_REPEAT_ONE
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    pub uri: String,
    pub metadata: String,
}

pub struct Playlist {
    pub name: String,
    pub items: Vec<Track>,
    pub index: usize,
    pub mode: String,
    pub order: Vec<usize>,
    pub order_pos: Option<usize>,
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            name: String::new(),
            items: Vec::new(),
            index: 0,
            mode: NORMAL.to_string(),
            order: Vec::new(),
            order_pos: None,
        }
    }
}

impl Playlist {
    pub fn is_shuffle(&self) -> bool {
        matches!(
            self.mode.as_str(),
            SHUFFLE_REPEAT_ALL | SHUFFLE_NO_REPEAT | SHUFFLE_REPEAT_ONE
        )
    }

    pub fn is_repeat_all(&self) -> bool {
        matches!(self.mode.as_str(), REPEAT_ALL | SHUFFLE_REPEAT_ALL)
    }

    pub fn is_repeat_one(&self) -> bool {
        matches!(self.mode.as_str(), REPEAT_ONE | SHUFFLE_REPEAT_ONE)
    }

    pub fn rebuild_order(&mut self) {
        if !self.is_shuffle() || self.items.is_empty() {
            self.order.clear();
            self.order_pos = None;
            return;
        }
        let mut order = Vec::with_capacity(self.items.len());
        if self.index < self.items.len() {
            order.push(self.index);
        }
        for i in 0..self.items.len() {
            if i != self.index {
                order.push(i);
            }
        }
        if order.len() > 2 {
            let mut rng = rand::rng();
            order[1..].shuffle(&mut rng);
        }
        self.order = order;
        self.order_pos = Some(0);
    }

    pub fn set_mode(&mut self, mode: &str) {
        let was_shuffle = self.is_shuffle();
        self.mode = mode.to_string();
        if !self.is_shuffle() {
            self.order.clear();
            self.order_pos = None;
            return;
        }
        if !was_shuffle || self.order.len() != self.items.len() {
            self.rebuild_order();
        }
    }

    pub fn ensure_order(&mut self) {
        if self.is_shuffle() && self.order.len() != self.items.len() {
            self.rebuild_order();
        }
    }

    pub fn current(&self) -> Track {
        self.items.get(self.index).cloned().unwrap_or_default()
    }

    pub fn matches(&self, name: &str) -> bool {
        !self.name.is_empty() && (name == CURRENT_QUEUE || name == self.name)
    }

    fn pos_of_current(&mut self) -> Option<usize> {
        if let Some(p) = self.order_pos
            && self.order.get(p) == Some(&self.index)
        {
            return Some(p);
        }
        let found = self.order.iter().position(|&idx| idx == self.index);
        self.order_pos = found;
        found
    }

    pub fn step(&mut self, delta: isize, eof: bool) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        if eof && self.is_repeat_one() {
            return Some(self.index);
        }
        if !self.is_shuffle() {
            let i = self.index as isize + delta;
            let n = self.items.len() as isize;
            if i >= 0 && i < n {
                return Some(i as usize);
            }
            if self.is_repeat_all() {
                return Some(i.rem_euclid(n) as usize);
            }
            return None;
        }
        self.ensure_order();
        let pos = self.pos_of_current()?;
        let next = pos as isize + delta;
        let n = self.order.len() as isize;
        if next >= 0 && next < n {
            return Some(self.order[next as usize]);
        }
        if self.is_repeat_all() {
            return Some(self.order[next.rem_euclid(n) as usize]);
        }
        None
    }

    pub fn append_items(&mut self, items: Vec<Track>) {
        let old_len = self.items.len();
        self.items.extend(items);
        if !self.is_shuffle() {
            return;
        }
        if old_len == 0 || self.order.len() != old_len {
            self.rebuild_order();
            return;
        }
        for i in old_len..self.items.len() {
            self.order.push(i);
        }
        if let Some(pos) = self.pos_of_current()
            && pos + 2 < self.order.len()
        {
            let mut rng = rand::rng();
            self.order[pos + 1..].shuffle(&mut rng);
        }
    }

    pub fn remove(&mut self, lo: usize, hi: usize) -> bool {
        let removed_current = lo <= self.index && self.index <= hi;
        self.items.drain(lo..=hi);
        if removed_current {
            self.index = if self.items.is_empty() {
                0
            } else {
                lo.min(self.items.len() - 1)
            };
        } else if hi < self.index {
            self.index -= hi - lo + 1;
        }
        if self.is_shuffle() {
            if self.items.is_empty() {
                self.order.clear();
            } else {
                let span = hi - lo + 1;
                self.order.retain_mut(|idx| {
                    if *idx >= lo && *idx <= hi {
                        false
                    } else {
                        if *idx > hi {
                            *idx -= span;
                        }
                        true
                    }
                });
            }
            self.order_pos = None;
        }
        removed_current
    }

    pub fn move_item(&mut self, from: usize, to: usize) {
        let item = self.items.remove(from);
        self.items.insert(to, item);
        let remap = |idx: usize| -> usize {
            if idx == from {
                to
            } else if from < idx && idx <= to {
                idx - 1
            } else if to <= idx && idx < from {
                idx + 1
            } else {
                idx
            }
        };
        self.index = remap(self.index);
        if self.is_shuffle() {
            for idx in &mut self.order {
                *idx = remap(*idx);
            }
            self.order_pos = None;
        }
    }

    pub fn track_numbers(&self) -> (usize, usize) {
        if self.items.is_empty() {
            return (0, 0);
        }
        if self.name.is_empty() {
            return (1, 1);
        }
        (self.index + 1, self.items.len())
    }

    pub fn next_track(&mut self) -> Track {
        if let Some(i) = self.step(1, true) {
            self.items[i].clone()
        } else {
            Track::default()
        }
    }
}

pub fn track_bytes(items: &[Track]) -> usize {
    items.iter().map(|t| t.uri.len() + t.metadata.len()).sum()
}

pub fn parse_index(raw: &str, n: usize) -> Option<usize> {
    let v: usize = raw.parse().ok()?;
    if v >= 1 && v <= n { Some(v - 1) } else { None }
}

/// Validate a playback URI (http:// or https:// with host, <= 8 KiB, no control chars).
pub fn valid_uri(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 8192 {
        return false;
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    let rest = if let Some(r) = raw.strip_prefix("http://") {
        r
    } else if let Some(r) = raw.strip_prefix("https://") {
        r
    } else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.starts_with(':')
}

/// Parse LinkPlay `QueueContext` XML (`<PlayList>...</PlayList>`).
pub fn parse_context(raw: &str) -> Result<(String, Vec<Track>), Fault> {
    let mut reader = Reader::from_str(raw);
    let mut buf = Vec::new();

    let mut list_name = String::new();
    let mut track_count: Option<usize> = None;
    let mut tracks: Vec<Track> = Vec::new();
    let mut in_playlist = false;
    let mut in_list_info = false;
    let mut in_tracks = false;
    let mut cur_track: Option<(String, Track)> = None;
    let mut cur_field: Option<&'static str> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if !in_playlist {
                    if local == "PlayList" {
                        in_playlist = true;
                    } else {
                        return Err(Fault::INVALID_ARGS);
                    }
                } else if in_tracks {
                    if let Some((_, _)) = &cur_track {
                        match local.as_str() {
                            "URL" => cur_field = Some("URL"),
                            "Metadata" => cur_field = Some("Metadata"),
                            _ => cur_field = None,
                        }
                    } else {
                        let expected = format!("Track{}", tracks.len() + 1);
                        if tracks.len() >= MAX_TRACKS || local != expected {
                            return Err(Fault::INVALID_ARGS);
                        }
                        cur_track = Some((local, Track::default()));
                    }
                } else if in_list_info {
                    if local == "TrackNumber" {
                        cur_field = Some("TrackNumber");
                    }
                } else {
                    match local.as_str() {
                        "ListName" => cur_field = Some("ListName"),
                        "ListInfo" => in_list_info = true,
                        "Tracks" => in_tracks = true,
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(field) = cur_field {
                    let text = e.unescape().map_err(|_| Fault::INVALID_ARGS)?;
                    match field {
                        "ListName" => list_name.push_str(&text),
                        "TrackNumber" => {
                            track_count = text.trim().parse::<usize>().ok();
                        }
                        "URL" => {
                            if let Some((_, t)) = &mut cur_track {
                                t.uri.push_str(&text);
                            }
                        }
                        "Metadata" => {
                            if let Some((_, t)) = &mut cur_track {
                                t.metadata.push_str(&text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if let Some(field) = cur_field {
                    let text = String::from_utf8_lossy(e.as_ref());
                    match field {
                        "ListName" => list_name.push_str(&text),
                        "URL" => {
                            if let Some((_, t)) = &mut cur_track {
                                t.uri.push_str(&text);
                            }
                        }
                        "Metadata" => {
                            if let Some((_, t)) = &mut cur_track {
                                t.metadata.push_str(&text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if cur_field.is_some()
                    && matches!(
                        local.as_str(),
                        "ListName" | "TrackNumber" | "URL" | "Metadata"
                    )
                {
                    cur_field = None;
                } else if let Some((tag_name, _)) = &cur_track
                    && local == *tag_name
                {
                    let (_, t) = cur_track.take().unwrap();
                    if !valid_uri(&t.uri) {
                        return Err(Fault::INVALID_ARGS);
                    }
                    tracks.push(t);
                } else if local == "Tracks" {
                    in_tracks = false;
                } else if local == "ListInfo" {
                    in_list_info = false;
                } else if local == "PlayList" {
                    in_playlist = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(Fault::INVALID_ARGS),
            _ => {}
        }
        buf.clear();
    }

    if list_name.trim().is_empty()
        || list_name.len() > 256
        || track_count.is_none()
        || track_count != Some(tracks.len())
        || tracks.is_empty()
        || tracks.len() > MAX_TRACKS
    {
        return Err(Fault::INVALID_ARGS);
    }

    Ok((list_name, tracks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_tracks(n: usize) -> Vec<Track> {
        (1..=n)
            .map(|i| Track {
                uri: format!("http://127.0.0.1:8080/t{i}.flac"),
                metadata: format!("<DIDL>t{i}</DIDL>"),
            })
            .collect()
    }

    #[test]
    fn loop_modes_all_six_roundtrip() {
        for (code, mode) in [
            ("0", REPEAT_ALL),
            ("1", REPEAT_ONE),
            ("2", SHUFFLE_REPEAT_ALL),
            ("3", SHUFFLE_NO_REPEAT),
            ("4", NORMAL),
            ("5", SHUFFLE_REPEAT_ONE),
        ] {
            assert_eq!(loop_code_to_mode(code), Some(mode));
            assert_eq!(mode_to_loop_code(mode), code);
            assert!(is_valid_play_mode(mode));
        }
        assert_eq!(loop_code_to_mode("6"), None);
    }

    #[test]
    fn shuffle_order_visits_every_track_once_and_respects_repeat() {
        let mut q = Playlist {
            name: "q1".into(),
            items: mk_tracks(5),
            index: 0,
            mode: NORMAL.into(),
            ..Default::default()
        };
        q.set_mode(SHUFFLE_NO_REPEAT);
        assert_eq!(q.order.len(), 5);
        assert_eq!(q.order[0], 0);

        let mut seen = vec![q.index];
        while let Some(next) = q.step(1, true) {
            q.index = next;
            seen.push(next);
        }
        assert_eq!(seen.len(), 5);
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4]);

        // Repeat all wraps cleanly from last to first in shuffle order.
        q.set_mode(SHUFFLE_REPEAT_ALL);
        let wrap = q.step(1, true);
        assert_eq!(wrap, Some(q.order[0]));

        // Shuffle repeat one repeats current on EOF, steps on manual Next.
        q.set_mode(SHUFFLE_REPEAT_ONE);
        assert_eq!(q.step(1, true), Some(q.index));
    }

    #[test]
    fn parse_context_rejects_bad_urls_and_mismatched_counts() {
        let good = r#"<PlayList><ListName>Mix</ListName><ListInfo><TrackNumber>1</TrackNumber></ListInfo><Tracks><Track1><URL>http://10.0.0.1/a.flac</URL><Metadata>&lt;DIDL/&gt;</Metadata></Track1></Tracks></PlayList>"#;
        let (name, tracks) = parse_context(good).unwrap();
        assert_eq!(name, "Mix");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].metadata, "<DIDL/>");

        let bad_scheme = good.replace("http://10.0.0.1/a.flac", "file:///etc/passwd");
        assert!(parse_context(&bad_scheme).is_err());

        let bad_count = good.replace(
            "<TrackNumber>1</TrackNumber>",
            "<TrackNumber>2</TrackNumber>",
        );
        assert!(parse_context(&bad_count).is_err());
    }
}

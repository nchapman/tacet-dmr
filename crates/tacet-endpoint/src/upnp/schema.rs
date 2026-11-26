//! Minimal UPnP AV `MediaRenderer:1` device description, SCPD schemas,
//! SOAP envelopes, and honest `ProtocolInfo` matching `tacet-audio`.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};

pub const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const AV: &str = "AVTransport";
pub const RC: &str = "RenderingControl";
pub const CM: &str = "ConnectionManager";

pub const SERVICES: &[&str] = &[AV, RC, CM];

/// Containerized audio MIME types natively demuxed and decoded by `tacet-audio`
/// (`symphonia` + `symphonia-opus` pure-Rust decoders). Unimplemented codecs
/// (DSF, WavPack, APE, WMA, raw audio/L16) are intentionally omitted so control
/// points transcode them automatically rather than failing playback.
pub const SINK_MIMES: &[&str] = &[
    "audio/flac",
    "audio/x-flac",
    "audio/wav",
    "audio/x-wav",
    "audio/aiff",
    "audio/x-aiff",
    "audio/mpeg",
    "audio/aac",
    "audio/mp4",
    "audio/x-m4a",
    "audio/alac",
    "audio/ogg",
    "audio/opus",
    "application/ogg",
    "audio/webm",
    "audio/x-matroska",
];

/// Comma-separated UPnP `ProtocolInfo` list shared by OpenHome `Playlist:1`
/// and UPnP `ConnectionManager:1`.
pub fn sink_protocols() -> String {
    SINK_MIMES
        .iter()
        .map(|m| format!("http-get:*:{m}:*"))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn service_type(service: &str) -> String {
    format!("urn:schemas-upnp-org:service:{service}:1")
}

/// Write XML-escaped text to `w` without intermediate allocations.
pub fn write_escaped<W: fmt::Write + ?Sized>(w: &mut W, s: &str) -> fmt::Result {
    let bytes = s.as_bytes();
    let mut last = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc = match b {
            b'"' => "&#34;",
            b'\'' => "&#39;",
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'\t' => "&#x9;",
            b'\n' => "&#xA;",
            b'\r' => "&#xD;",
            0x00..=0x1F => "\u{FFFD}",
            _ => continue,
        };
        if i > last {
            w.write_str(&s[last..i])?;
        }
        w.write_str(esc)?;
        last = i + 1;
    }
    if last < s.len() {
        w.write_str(&s[last..])?;
    }
    Ok(())
}

pub fn write_tag<W: fmt::Write + ?Sized>(w: &mut W, name: &str, value: &str) -> fmt::Result {
    w.write_char('<')?;
    w.write_str(name)?;
    w.write_char('>')?;
    write_escaped(w, value)?;
    w.write_str("</")?;
    w.write_str(name)?;
    w.write_char('>')
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let _ = write_escaped(&mut out, s);
    out
}

pub fn tag(name: &str, value: &str) -> String {
    let mut out = String::with_capacity(name.len() * 2 + value.len() + 5);
    let _ = write_tag(&mut out, name, value);
    out
}

/// Render the UPnP `MediaRenderer:1` root device description served at `/upnp/description.xml`.
pub fn device_xml(name: &str, udn: &str) -> String {
    let mut b = String::with_capacity(1024);
    b.push_str(
        r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><device>"#,
    );
    let _ = write_tag(&mut b, "deviceType", DEVICE_TYPE);
    let _ = write_tag(&mut b, "friendlyName", name);
    let _ = write_tag(&mut b, "manufacturer", "Tacet");
    let _ = write_tag(&mut b, "modelName", "tacet-endpoint");
    let _ = write_tag(&mut b, "UDN", udn);
    b.push_str("<serviceList>");
    for &s in SERVICES {
        b.push_str("<service>");
        let _ = write_tag(&mut b, "serviceType", &service_type(s));
        let _ = write_tag(&mut b, "serviceId", &format!("urn:upnp-org:serviceId:{s}"));
        let _ = write_tag(&mut b, "SCPDURL", &format!("/upnp/{s}/scpd.xml"));
        let _ = write_tag(&mut b, "controlURL", &format!("/upnp/{s}/control"));
        let _ = write_tag(&mut b, "eventSubURL", &format!("/upnp/{s}/events"));
        b.push_str("</service>");
    }
    b.push_str("</serviceList></device></root>");
    b
}

#[derive(Debug, Clone, Copy)]
pub struct ActionSpec {
    pub name: &'static str,
    pub inputs: &'static str,
    pub outputs: &'static str,
}

pub fn actions_for(service: &str) -> &'static [ActionSpec] {
    match service {
        AV => &[
            ActionSpec {
                name: "SetAVTransportURI",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID CurrentURI:AVTransportURI CurrentURIMetaData:AVTransportURIMetaData",
                outputs: "",
            },
            ActionSpec {
                name: "SetNextAVTransportURI",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID NextURI:NextAVTransportURI NextURIMetaData:NextAVTransportURIMetaData",
                outputs: "",
            },
            ActionSpec {
                name: "Play",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID Speed:TransportPlaySpeed",
                outputs: "",
            },
            ActionSpec {
                name: "Pause",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "",
            },
            ActionSpec {
                name: "Stop",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "",
            },
            ActionSpec {
                name: "Seek",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID Unit:A_ARG_TYPE_SeekMode Target:A_ARG_TYPE_SeekTarget",
                outputs: "",
            },
            ActionSpec {
                name: "Next",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "",
            },
            ActionSpec {
                name: "Previous",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "",
            },
            ActionSpec {
                name: "SetPlayMode",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID NewPlayMode:CurrentPlayMode",
                outputs: "",
            },
            ActionSpec {
                name: "GetCurrentTransportActions",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "Actions:CurrentTransportActions",
            },
            ActionSpec {
                name: "GetTransportInfo",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "CurrentTransportState:TransportState CurrentTransportStatus:TransportStatus CurrentSpeed:TransportPlaySpeed",
            },
            ActionSpec {
                name: "GetPositionInfo",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "Track:CurrentTrack TrackDuration:CurrentTrackDuration TrackMetaData:CurrentTrackMetaData TrackURI:CurrentTrackURI RelTime:RelativeTimePosition AbsTime:AbsoluteTimePosition RelCount:RelativeCounterPosition AbsCount:AbsoluteCounterPosition",
            },
            ActionSpec {
                name: "GetMediaInfo",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "NrTracks:NumberOfTracks MediaDuration:CurrentMediaDuration CurrentURI:AVTransportURI CurrentURIMetaData:AVTransportURIMetaData NextURI:NextAVTransportURI NextURIMetaData:NextAVTransportURIMetaData PlayMedium:PlaybackStorageMedium RecordMedium:RecordStorageMedium WriteStatus:RecordMediumWriteStatus",
            },
            ActionSpec {
                name: "GetDeviceCapabilities",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "PlayMedia:PossiblePlaybackStorageMedia RecMedia:PossibleRecordStorageMedia RecQualityModes:PossibleRecordQualityModes",
            },
            ActionSpec {
                name: "GetTransportSettings",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "PlayMode:CurrentPlayMode RecQualityMode:CurrentRecordQualityMode",
            },
        ],
        RC => &[
            ActionSpec {
                name: "ListPresets",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID",
                outputs: "CurrentPresetNameList:PresetNameList",
            },
            ActionSpec {
                name: "SelectPreset",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID PresetName:A_ARG_TYPE_PresetName",
                outputs: "",
            },
            ActionSpec {
                name: "GetVolume",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel",
                outputs: "CurrentVolume:Volume",
            },
            ActionSpec {
                name: "SetVolume",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel DesiredVolume:Volume",
                outputs: "",
            },
            ActionSpec {
                name: "GetMute",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel",
                outputs: "CurrentMute:Mute",
            },
            ActionSpec {
                name: "SetMute",
                inputs: "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel DesiredMute:Mute",
                outputs: "",
            },
        ],
        CM => &[
            ActionSpec {
                name: "GetProtocolInfo",
                inputs: "",
                outputs: "Source:SourceProtocolInfo Sink:SinkProtocolInfo",
            },
            ActionSpec {
                name: "GetCurrentConnectionIDs",
                inputs: "",
                outputs: "ConnectionIDs:CurrentConnectionIDs",
            },
            ActionSpec {
                name: "GetCurrentConnectionInfo",
                inputs: "ConnectionID:A_ARG_TYPE_ConnectionID",
                outputs: "RcsID:A_ARG_TYPE_RcsID AVTransportID:A_ARG_TYPE_AVTransportID ProtocolInfo:A_ARG_TYPE_ProtocolInfo PeerConnectionManager:A_ARG_TYPE_ConnectionManager PeerConnectionID:A_ARG_TYPE_ConnectionID Direction:A_ARG_TYPE_Direction Status:A_ARG_TYPE_ConnectionStatus",
            },
        ],
        _ => &[],
    }
}

pub fn find_action(service: &str, name: &str) -> Option<&'static ActionSpec> {
    actions_for(service).iter().find(|a| a.name == name)
}

fn state_var_type(name: &str) -> &'static str {
    match name {
        "A_ARG_TYPE_InstanceID" | "CurrentTrack" | "NumberOfTracks" => "ui4",
        "RelativeCounterPosition"
        | "AbsoluteCounterPosition"
        | "A_ARG_TYPE_RcsID"
        | "A_ARG_TYPE_AVTransportID"
        | "A_ARG_TYPE_ConnectionID" => "i4",
        "Volume" => "ui2",
        "Mute" => "boolean",
        _ => "string",
    }
}

pub fn scpd_xml(service: &str) -> String {
    let mut act_buf = String::new();
    let mut var_buf = String::new();
    let mut seen = HashSet::new();

    let mut add_var = |v: &str, send_events: bool| {
        if !seen.insert(v.to_string()) {
            return;
        }
        let typ = state_var_type(v);
        let send = if send_events { "yes" } else { "no" };
        let _ = write!(var_buf, r#"<stateVariable sendEvents="{send}">"#);
        let _ = write_tag(&mut var_buf, "name", v);
        let _ = write_tag(&mut var_buf, "dataType", typ);
        if v == "Volume" {
            var_buf.push_str(
                "<allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange>",
            );
        }
        var_buf.push_str("</stateVariable>");
    };

    if service == AV || service == RC {
        add_var("LastChange", true);
    }

    for act in actions_for(service) {
        act_buf.push_str("<action>");
        let _ = write_tag(&mut act_buf, "name", act.name);
        act_buf.push_str("<argumentList>");
        for (dir, args) in [("in", act.inputs), ("out", act.outputs)] {
            for f in args.split_whitespace() {
                let (n, v) = f.split_once(':').unwrap_or((f, f));
                act_buf.push_str("<argument>");
                let _ = write_tag(&mut act_buf, "name", n);
                let _ = write_tag(&mut act_buf, "direction", dir);
                let _ = write_tag(&mut act_buf, "relatedStateVariable", v);
                act_buf.push_str("</argument>");
                let ev = service == CM
                    && matches!(
                        v,
                        "CurrentConnectionIDs" | "SinkProtocolInfo" | "SourceProtocolInfo"
                    );
                add_var(v, ev);
            }
        }
        act_buf.push_str("</argumentList></action>");
    }

    format!(
        r#"<?xml version="1.0"?><scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><actionList>{act_buf}</actionList><serviceStateTable>{var_buf}</serviceStateTable></scpd>"#
    )
}

/// UPnP SOAP fault error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub code: u32,
    pub desc: &'static str,
}

impl Fault {
    pub const INVALID_ACTION: Self = Self {
        code: 401,
        desc: "Invalid Action",
    };
    pub const INVALID_ARGS: Self = Self {
        code: 402,
        desc: "Invalid Args",
    };
    pub const ACTION_FAILED: Self = Self {
        code: 501,
        desc: "Action Failed",
    };
    pub const TRANSITION_UNAVAILABLE: Self = Self {
        code: 701,
        desc: "Transition not available",
    };
    pub const SEEK_MODE_UNSUPPORTED: Self = Self {
        code: 710,
        desc: "Seek mode not supported",
    };
    pub const ILLEGAL_SEEK_TARGET: Self = Self {
        code: 711,
        desc: "Illegal seek target",
    };
    pub const PLAY_MODE_UNSUPPORTED: Self = Self {
        code: 712,
        desc: "Play mode not supported",
    };
    pub const INVALID_INSTANCE: Self = Self {
        code: 718,
        desc: "Invalid InstanceID",
    };

    pub fn to_soap_xml(&self) -> String {
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#,
            self.code,
            escape(self.desc)
        )
    }
}

pub fn soap_response_xml(
    service: &str,
    action: &str,
    out_order: &str,
    fields: &HashMap<String, String>,
) -> String {
    let mut b = String::with_capacity(512);
    let _ = write!(
        b,
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action}Response xmlns:u="{}">"#,
        service_type(service)
    );
    for f in out_order.split_whitespace() {
        let name = f.split_once(':').map(|(n, _)| n).unwrap_or(f);
        let val = fields.get(name).map(String::as_str).unwrap_or("");
        let _ = write_tag(&mut b, name, val);
    }
    let _ = write!(b, "</u:{action}Response></s:Body></s:Envelope>");
    b
}

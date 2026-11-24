// Package renderer hosts the UPnP services for one audio output.
package renderer

import (
	"fmt"
	"io"
	"strings"
)

const (
	deviceType = "urn:schemas-upnp-org:device:MediaRenderer:1"
	av         = "AVTransport"
	rc         = "RenderingControl"
	cm         = "ConnectionManager"
	pq         = "PlayQueue"
)

// Containerized audio MIME types natively demuxed and decoded by embedded
// libmpv (FFmpeg lavf/lavc). Headerless raw PCM (audio/L16) is omitted because
// libmpv cannot infer sample format without out-of-band flags. DSD (.dsf) is
// decoded to PCM by libmpv rather than passed through as native DSD/DoP.
var sinkMIMEs = []string{
	"audio/flac", "audio/x-flac",
	"audio/wav", "audio/x-wav",
	"audio/aiff", "audio/x-aiff",
	"audio/mpeg",
	"audio/aac",
	"audio/mp4", "audio/x-m4a", "audio/alac",
	"audio/ogg", "application/ogg", "audio/opus",
	"audio/webm", "audio/x-matroska",
	"audio/dsf", "audio/x-dsf",
	"audio/wavpack", "audio/x-wavpack",
	"audio/x-ape",
	"audio/x-ms-wma",
}

var protocols = func() string {
	entries := make([]string, len(sinkMIMEs))
	for i, m := range sinkMIMEs {
		entries[i] = "http-get:*:" + m + ":*"
	}
	return strings.Join(entries, ",")
}()

var services = []string{av, rc, cm, pq}

func serviceType(service string) string {
	if service == pq {
		return "urn:schemas-wiimu-com:service:PlayQueue:1"
	}
	return "urn:schemas-upnp-org:service:" + service + ":1"
}
func writeEscaped(w io.Writer, s string) error {
	last := 0
	for i := 0; i < len(s); i++ {
		var esc string
		switch s[i] {
		case '"':
			esc = "&#34;"
		case '\'':
			esc = "&#39;"
		case '&':
			esc = "&amp;"
		case '<':
			esc = "&lt;"
		case '>':
			esc = "&gt;"
		case '\t':
			esc = "&#x9;"
		case '\n':
			esc = "&#xA;"
		case '\r':
			esc = "&#xD;"
		default:
			if s[i] >= 0x20 {
				continue
			}
			esc = "\uFFFD"
		}
		if i > last {
			if _, err := io.WriteString(w, s[last:i]); err != nil {
				return err
			}
		}
		if _, err := io.WriteString(w, esc); err != nil {
			return err
		}
		last = i + 1
	}
	if last < len(s) {
		_, err := io.WriteString(w, s[last:])
		return err
	}
	return nil
}

type escapeWriter struct{ w io.Writer }

func (e escapeWriter) Write(p []byte) (int, error) {
	if err := writeEscaped(e.w, string(p)); err != nil {
		return 0, err
	}
	return len(p), nil
}

func (e escapeWriter) WriteString(s string) (int, error) {
	if err := writeEscaped(e.w, s); err != nil {
		return 0, err
	}
	return len(s), nil
}

func writeTag(w io.Writer, name, value string) error {
	if _, err := io.WriteString(w, "<"); err != nil {
		return err
	}
	if _, err := io.WriteString(w, name); err != nil {
		return err
	}
	if _, err := io.WriteString(w, ">"); err != nil {
		return err
	}
	if err := writeEscaped(w, value); err != nil {
		return err
	}
	if _, err := io.WriteString(w, "</"); err != nil {
		return err
	}
	if _, err := io.WriteString(w, name); err != nil {
		return err
	}
	_, err := io.WriteString(w, ">")
	return err
}

func escape(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	_ = writeEscaped(&b, s)
	return b.String()
}
func tag(name, value string) string {
	var b strings.Builder
	b.Grow(len(name)*2 + len(value) + 5)
	_ = writeTag(&b, name, value)
	return b.String()
}
func deviceXML(name, udn string) string {
	var b strings.Builder
	b.WriteString(`<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><device>`)
	b.WriteString(tag("deviceType", deviceType) + tag("friendlyName", name) + tag("manufacturer", "Tacet") + tag("modelName", "tacet-dmr") + tag("UDN", udn) + "<serviceList>")
	for _, s := range services {
		eventURL := "/" + s + "/events"
		id := "urn:upnp-org:serviceId:" + s
		if s == pq {
			eventURL = ""
			id = "urn:wiimu-com:serviceId:PlayQueue"
		}
		b.WriteString("<service>" + tag("serviceType", serviceType(s)) + tag("serviceId", id) + tag("SCPDURL", "/"+s+"/scpd.xml") + tag("controlURL", "/"+s+"/control") + tag("eventSubURL", eventURL) + "</service>")
	}
	return b.String() + "</serviceList></device></root>"
}

// Argument notation: wire name[:related state variable]. The same table drives
// the published SCPD, request validation, and response argument ordering.
type action struct{ name, in, out string }

var actions = map[string][]action{
	av: {
		{"SetAVTransportURI", "InstanceID:A_ARG_TYPE_InstanceID CurrentURI:AVTransportURI CurrentURIMetaData:AVTransportURIMetaData", ""},
		{"Play", "InstanceID:A_ARG_TYPE_InstanceID Speed:TransportPlaySpeed", ""}, {"Pause", "InstanceID:A_ARG_TYPE_InstanceID", ""}, {"Stop", "InstanceID:A_ARG_TYPE_InstanceID", ""},
		{"Seek", "InstanceID:A_ARG_TYPE_InstanceID Unit:A_ARG_TYPE_SeekMode Target:A_ARG_TYPE_SeekTarget", ""},
		{"SetNextAVTransportURI", "InstanceID:A_ARG_TYPE_InstanceID NextURI:NextAVTransportURI NextURIMetaData:NextAVTransportURIMetaData", ""},
		{"Next", "InstanceID:A_ARG_TYPE_InstanceID", ""}, {"Previous", "InstanceID:A_ARG_TYPE_InstanceID", ""},
		{"GetCurrentTransportActions", "InstanceID:A_ARG_TYPE_InstanceID", "Actions:CurrentTransportActions"},
		{"SetPlayMode", "InstanceID:A_ARG_TYPE_InstanceID NewPlayMode:CurrentPlayMode", ""},
		{"GetTransportInfo", "InstanceID:A_ARG_TYPE_InstanceID", "CurrentTransportState:TransportState CurrentTransportStatus:TransportStatus CurrentSpeed:TransportPlaySpeed"},
		{"GetPositionInfo", "InstanceID:A_ARG_TYPE_InstanceID", "Track:CurrentTrack TrackDuration:CurrentTrackDuration TrackMetaData:CurrentTrackMetaData TrackURI:CurrentTrackURI RelTime:RelativeTimePosition AbsTime:AbsoluteTimePosition RelCount:RelativeCounterPosition AbsCount:AbsoluteCounterPosition"},
		{"GetMediaInfo", "InstanceID:A_ARG_TYPE_InstanceID", "NrTracks:NumberOfTracks MediaDuration:CurrentMediaDuration CurrentURI:AVTransportURI CurrentURIMetaData:AVTransportURIMetaData NextURI:NextAVTransportURI NextURIMetaData:NextAVTransportURIMetaData PlayMedium:PlaybackStorageMedium RecordMedium:RecordStorageMedium WriteStatus:RecordMediumWriteStatus"},
		{"GetDeviceCapabilities", "InstanceID:A_ARG_TYPE_InstanceID", "PlayMedia:PossiblePlaybackStorageMedia RecMedia:PossibleRecordStorageMedia RecQualityModes:PossibleRecordQualityModes"},
		{"GetTransportSettings", "InstanceID:A_ARG_TYPE_InstanceID", "PlayMode:CurrentPlayMode RecQualityMode:CurrentRecordQualityMode"},
	},
	pq: {
		{"BrowseQueue", "QueueName:QueueName", "QueueContext:QueueContext"}, {"GetQueueLoopMode", "", "LoopMode:LoopMode"},
		{"CreateQueue", "QueueContext:QueueContext", ""}, {"AppendTracksInQueue", "QueueContext:QueueContext", ""},
		{"PlayQueueWithIndex", "QueueName:QueueName Index:Index", ""}, {"GetQueueIndex", "QueueName:QueueName", "CurrentIndex:Index PreloadingIndex:Index CurrentPage:CurrentPage TrackNums:TrackNums"},
		{"RemoveTracksInQueue", "QueueName:QueueName RangStart:Index RangEnd:Index Action:QueueAction", ""},
		{"MoveTracksInQueue", "QueueName:QueueName IndexList:IndexList ToIndex:Index", ""}, {"SetQueueLoopMode", "LoopMode:LoopMode", ""},
	},
	rc: {
		{"ListPresets", "InstanceID:A_ARG_TYPE_InstanceID", "CurrentPresetNameList:PresetNameList"}, {"SelectPreset", "InstanceID:A_ARG_TYPE_InstanceID PresetName:A_ARG_TYPE_PresetName", ""},
		{"GetVolume", "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel", "CurrentVolume:Volume"}, {"SetVolume", "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel DesiredVolume:Volume", ""},
		{"GetMute", "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel", "CurrentMute:Mute"}, {"SetMute", "InstanceID:A_ARG_TYPE_InstanceID Channel:A_ARG_TYPE_Channel DesiredMute:Mute", ""},
	},
	cm: {
		{"GetProtocolInfo", "", "Source:SourceProtocolInfo Sink:SinkProtocolInfo"}, {"GetCurrentConnectionIDs", "", "ConnectionIDs:CurrentConnectionIDs"},
		{"GetCurrentConnectionInfo", "ConnectionID:A_ARG_TYPE_ConnectionID", "RcsID:A_ARG_TYPE_RcsID AVTransportID:A_ARG_TYPE_AVTransportID ProtocolInfo:A_ARG_TYPE_ProtocolInfo PeerConnectionManager:A_ARG_TYPE_ConnectionManager PeerConnectionID:A_ARG_TYPE_ConnectionID Direction:A_ARG_TYPE_Direction Status:A_ARG_TYPE_ConnectionStatus"},
	},
}
var types = map[string]string{"Index": "ui4", "CurrentPage": "ui4", "TrackNums": "ui4", "LoopMode": "ui4", "A_ARG_TYPE_InstanceID": "ui4", "CurrentTrack": "ui4", "NumberOfTracks": "ui4", "RelativeCounterPosition": "i4", "AbsoluteCounterPosition": "i4", "A_ARG_TYPE_RcsID": "i4", "A_ARG_TYPE_AVTransportID": "i4", "A_ARG_TYPE_ConnectionID": "i4", "Volume": "ui2", "Mute": "boolean"}

func scpdXML(service string) string {
	var a, vars strings.Builder
	seen := map[string]bool{}
	addVar := func(v string, event bool) {
		if seen[v] {
			return
		}
		seen[v] = true
		typ := types[v]
		if typ == "" {
			typ = "string"
		}
		send := "no"
		if event {
			send = "yes"
		}
		fmt.Fprintf(&vars, `<stateVariable sendEvents="%s">`, send)
		vars.WriteString(tag("name", v) + tag("dataType", typ))
		if v == "Volume" {
			vars.WriteString(`<allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange>`)
		}
		vars.WriteString("</stateVariable>")
	}
	if service == av || service == rc {
		addVar("LastChange", true)
	}
	for _, act := range actions[service] {
		a.WriteString("<action>" + tag("name", act.name) + "<argumentList>")
		for _, group := range []struct{ dir, args string }{{"in", act.in}, {"out", act.out}} {
			for _, f := range strings.Fields(group.args) {
				n, v, _ := strings.Cut(f, ":")
				a.WriteString("<argument>" + tag("name", n) + tag("direction", group.dir) + tag("relatedStateVariable", v) + "</argument>")
				addVar(v, service == cm && (v == "CurrentConnectionIDs" || v == "SinkProtocolInfo" || v == "SourceProtocolInfo"))
			}
		}
		a.WriteString("</argumentList></action>")
	}
	return `<?xml version="1.0"?><scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><actionList>` + a.String() + "</actionList><serviceStateTable>" + vars.String() + "</serviceStateTable></scpd>"
}

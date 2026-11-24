package renderer

import (
	"context"
	"fmt"
	"net/http"
	"net/netip"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/google/uuid"
)

const eventNS = "urn:schemas-upnp-org:event-1-0"

type eventState struct {
	state, status, uri, mode, next string
	durationSec                    int64
	track                          int
	volume                         int
	mute                           bool
}

func (t *transport) eventState() eventState {
	num, _ := t.q.trackNumbers()
	next := t.q.nextTrack().uri
	durSec := max(int64(0), int64(t.duration/time.Second))
	return eventState{state: t.state, status: t.status, uri: t.q.current().uri, durationSec: durSec, track: num, mode: t.q.mode, next: next, volume: t.volume, mute: t.mute}
}
func (v eventState) body(s string) string {
	var props string
	switch s {
	case cm:
		for _, p := range []struct{ k, v string }{{"SourceProtocolInfo", ""}, {"SinkProtocolInfo", protocols}, {"CurrentConnectionIDs", "0"}} {
			props += "<e:property>" + tag(p.k, p.v) + "</e:property>"
		}
	default:
		ns := "AVT"
		dur := timestamp(time.Duration(v.durationSec) * time.Second)
		changes := `<TransportState val="` + v.state + `"/><TransportStatus val="` + v.status + `"/><CurrentTrackURI val="` + escape(v.uri) + `"/><CurrentTrackDuration val="` + dur + `"/>`
		changes += fmt.Sprintf(`<CurrentTrack val="%d"/><CurrentPlayMode val="%s"/><NextAVTransportURI val="%s"/>`, v.track, v.mode, escape(v.next))
		if s == rc {
			ns = "RCS"
			changes = fmt.Sprintf(`<Volume channel="Master" val="%d"/><Mute channel="Master" val="%s"/>`, v.volume, onOff(v.mute))
		}
		props = "<e:property>" + tag("LastChange", `<Event xmlns="urn:schemas-upnp-org:metadata-1-0/`+ns+`/"><InstanceID val="0">`+changes+`</InstanceID></Event>`) + "</e:property>"
	}
	return `<?xml version="1.0"?><e:propertyset xmlns:e="` + eventNS + `">` + props + "</e:propertyset>"
}

type subscription struct {
	sid, service, callback string
	peer                   netip.Addr
	expires                time.Time
	cancel                 context.CancelFunc
	updates                chan eventState
}

func (d *Device) publish() {
	v := d.t.eventState()
	old := d.last
	d.last = v
	if v == old {
		return
	}
	for _, s := range d.subs {
		if s.service == cm || (s.service == rc && v.volume == old.volume && v.mute == old.mute) || (s.service == av && v.state == old.state && v.uri == old.uri && v.durationSec == old.durationSec && v.status == old.status && v.track == old.track && v.mode == old.mode && v.next == old.next) {
			continue
		}
		// The initial event is delivered separately. A slow subscriber needs only the
		// latest complete snapshot; it must never block another subscriber or playback.
		select {
		case <-s.updates:
		default:
		}
		s.updates <- v
	}
}
func timeoutSeconds(s string) int {
	if n, ok := strings.CutPrefix(strings.ToLower(s), "second-"); ok {
		if v, err := strconv.Atoi(n); err == nil && v > 0 && v < 300 {
			return v
		}
	}
	return 300
}
func callbackURL(raw string, peer netip.Addr) (string, bool) {
	s := strings.TrimSpace(raw)
	if !strings.HasPrefix(s, "<") || !strings.HasSuffix(s, ">") {
		return "", false
	}
	// UPnP UDA 1.0 permits multiple <url> entries; parse each cleanly without
	// letting an inner '>' bleed into the path of the first URL.
	for len(s) > 0 {
		s = strings.TrimSpace(s)
		if !strings.HasPrefix(s, "<") {
			return "", false
		}
		end := strings.IndexByte(s, '>')
		if end <= 1 {
			return "", false
		}
		token := s[1:end]
		if strings.ContainsRune(token, '<') {
			return "", false
		}
		s = strings.TrimSpace(s[end+1:])
		u, err := url.Parse(token)
		if err != nil || u.Scheme != "http" || u.User != nil || u.Fragment != "" {
			continue
		}
		ip, err := netip.ParseAddr(u.Hostname())
		if err != nil || ip.Unmap() != peer.Unmap() {
			continue
		}
		return u.String(), true
	}
	return "", false
}

func (d *Device) hasRCSubs() bool {
	for _, s := range d.subs {
		if s.service == rc {
			return true
		}
	}
	return false
}

// Acquires d.mu only for state validation and registry updates; writes and
// flushes the HTTP response after releasing d.mu so a slow client cannot stall
// playback or other SOAP requests.
func (d *Device) subscription(w http.ResponseWriter, r *http.Request, service string) {
	sid := r.Header.Get("SID")
	peer := requestPeer(r)
	cb := r.Header.Get("Callback")
	nt := r.Header.Get("NT")
	sec := timeoutSeconds(r.Header.Get("Timeout"))
	expires := time.Now().Add(time.Duration(sec) * time.Second)

	d.mu.Lock()
	if d.closed {
		d.mu.Unlock()
		w.WriteHeader(503)
		return
	}
	sub := d.subs[sid]
	if sid != "" {
		if cb != "" || nt != "" || sub == nil || sub.peer != peer || sub.service != service || time.Now().After(sub.expires) {
			d.mu.Unlock()
			w.WriteHeader(412)
			return
		}
		if r.Method == "UNSUBSCRIBE" {
			d.remove(sub)
			d.mu.Unlock()
			d.nudge()
			w.WriteHeader(200)
			return
		}
		sub.expires = expires
		d.mu.Unlock()
		d.nudge()
		w.Header().Set("SID", sid)
		w.Header().Set("TIMEOUT", fmt.Sprintf("Second-%d", sec))
		w.WriteHeader(200)
		return
	}
	if r.Method != "SUBSCRIBE" || nt != "upnp:event" {
		d.mu.Unlock()
		w.WriteHeader(412)
		return
	}
	target, ok := callbackURL(cb, peer)
	if !ok {
		d.mu.Unlock()
		w.WriteHeader(412)
		return
	}
	if len(d.subs) >= 16 {
		d.mu.Unlock()
		w.WriteHeader(503)
		return
	}
	if service == rc {
		d.t.syncVolume(true, true)
		d.publish()
	}
	sid = "uuid:" + uuid.NewString()
	ctx, cancel := context.WithCancel(context.Background())
	sub = &subscription{sid: sid, service: service, callback: target, peer: peer, expires: expires, cancel: cancel, updates: make(chan eventState, 1)}
	d.subs[sid] = sub
	initial := d.t.eventState()
	d.wg.Add(1)
	d.mu.Unlock()

	d.nudge()
	w.Header().Set("SID", sid)
	w.Header().Set("TIMEOUT", fmt.Sprintf("Second-%d", sec))
	w.WriteHeader(200)
	_ = http.NewResponseController(w).Flush()
	go d.notify(ctx, sub, initial)
}
func (d *Device) remove(s *subscription) { s.cancel(); delete(d.subs, s.sid) }
func (d *Device) notify(ctx context.Context, s *subscription, v eventState) {
	defer d.wg.Done()
	if ctx.Err() != nil {
		return
	}
	var seq uint32
	for {
		var err error
		for attempt := 0; attempt < 2; attempt++ {
			if attempt > 0 {
				timer := time.NewTimer(150 * time.Millisecond)
				select {
				case <-ctx.Done():
					timer.Stop()
					return
				case <-timer.C:
				}
			}
			var req *http.Request
			req, err = http.NewRequestWithContext(ctx, "NOTIFY", s.callback, strings.NewReader(v.body(s.service)))
			if err == nil {
				req.Header.Set("Content-Type", `text/xml; charset="utf-8"`)
				req.Header.Set("NT", "upnp:event")
				req.Header.Set("NTS", "upnp:propchange")
				req.Header.Set("SID", s.sid)
				req.Header.Set("SEQ", strconv.FormatUint(uint64(seq), 10))
				var resp *http.Response
				resp, err = d.client.Do(req)
				if resp != nil {
					resp.Body.Close()
					if resp.StatusCode/100 != 2 {
						err = fmt.Errorf("callback returned %d", resp.StatusCode)
					}
				}
			}
			if err == nil {
				break
			}
		}
		if err != nil {
			d.mu.Lock()
			if d.subs[s.sid] == s {
				d.remove(s)
			}
			d.mu.Unlock()
			return
		}
		seq++
		if seq == 0 {
			seq = 1
		}
		select {
		case <-ctx.Done():
			return
		case v = <-s.updates:
		}
	}
}

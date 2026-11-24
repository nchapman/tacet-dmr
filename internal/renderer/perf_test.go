package renderer

import (
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type countingEngine struct {
	*queueEngine
	statusCalls int
	pos         time.Duration
	dur         time.Duration
}

func (c *countingEngine) Status() playback.Status {
	c.statusCalls++
	return playback.Status{Position: c.pos, Duration: c.dur}
}

func TestRefreshAvoidsRedundantStatusCallsOnceDurationKnown(t *testing.T) {
	var eng *countingEngine
	tpt := newTransport(func() (Engine, error) {
		eng = &countingEngine{
			queueEngine: &queueEngine{entries: make(map[int64]string)},
			pos:         2 * time.Second,
			dur:         180 * time.Second,
		}
		return eng, nil
	}, deviceOptions{})
	tpt.call(av, "SetAVTransportURI", fields{"InstanceID": "0", "CurrentURI": "http://127.0.0.1/a.flac", "CurrentURIMetaData": ""})
	tpt.call(av, "Play", fields{"InstanceID": "0", "Speed": "1"})
	tpt.refresh()

	// Once duration is known, steady-state refresh(), GetTransportInfo, and
	// GetMediaInfo must not hammer libmpv with Status() calls.
	baseline := eng.statusCalls
	for range 20 {
		tpt.refresh()
		tpt.call(av, "GetTransportInfo", fields{"InstanceID": "0"})
		tpt.call(av, "GetMediaInfo", fields{"InstanceID": "0"})
	}
	if eng.statusCalls != baseline {
		t.Fatalf("steady-state polling made %d extra Status() calls", eng.statusCalls-baseline)
	}

	// GetPositionInfo must still query fresh live position on demand.
	eng.pos = 15 * time.Second
	pos, f := tpt.call(av, "GetPositionInfo", fields{"InstanceID": "0"})
	if f != nil || pos["RelTime"] != "0:00:15" || pos["TrackDuration"] != "0:03:00" {
		t.Fatalf("GetPositionInfo = %+v, fault=%v", pos, f)
	}
	if eng.statusCalls != baseline+1 {
		t.Fatalf("expected exactly 1 on-demand Status() call, got %d", eng.statusCalls-baseline)
	}
}

type countingVolumeControl struct {
	reads atomic.Int64
	vol   int
}

func (c *countingVolumeControl) Volume() (int, error) {
	c.reads.Add(1)
	return c.vol, nil
}
func (c *countingVolumeControl) SetVolume(v int) error { c.vol = v; return nil }
func (c *countingVolumeControl) Mute() (bool, error)   { return false, playback.ErrUnsupported }
func (c *countingVolumeControl) SetMute(bool) error    { return playback.ErrUnsupported }

func TestIdleDeviceSkipsHardwareVolumePollingWithoutRCSubscribers(t *testing.T) {
	vol := &countingVolumeControl{vol: 40}
	d := newDevice("Desk", "uuid:idle-test", "127.0.0.1:49152", netip.MustParsePrefix("127.0.0.1/32"), deviceOptions{volume: vol}, nil)
	defer d.Close()

	// Wait briefly for initial monitor() pass to settle, then confirm idle
	// device performs zero periodic HAL volume reads without RC subscribers.
	time.Sleep(50 * time.Millisecond)
	before := vol.reads.Load()
	time.Sleep(250 * time.Millisecond)
	if got := vol.reads.Load() - before; got != 0 {
		t.Fatalf("idle device polled hardware volume %d times without RC subscribers", got)
	}
}

func TestBrowseQueueStreamingMatchesNonStreaming(t *testing.T) {
	d := New("Desk", "uuid:browse-eq", "127.0.0.1:49152", netip.MustParsePrefix("127.0.0.1/32"), 100, nil)
	defer d.Close()
	items := []track{
		{uri: "http://127.0.0.1/a.flac?x=1&y=2", metadata: `<DIDL-Lite><dc:title>A &amp; B</dc:title></DIDL-Lite>`},
		{uri: "http://127.0.0.1/b.flac", metadata: `<DIDL-Lite><dc:title>"Quoted" &lt;tag&gt;</dc:title></DIDL-Lite>`},
	}
	d.mu.Lock()
	d.t.q = playlist{name: "tacet", items: items, mode: normal}
	wantFields, f := d.t.q.browse(currentQueue)
	d.mu.Unlock()
	if f != nil {
		t.Fatal(f)
	}
	ns := serviceType(pq)
	want := fmt.Sprintf(`<?xml version="1.0"?><s:Envelope xmlns:s="%s" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:BrowseQueueResponse xmlns:u="%s">%s</u:BrowseQueueResponse></s:Body></s:Envelope>`, soapNS, ns, tag("QueueContext", wantFields["QueueContext"]))

	body := `<s:Envelope xmlns:s="` + soapNS + `"><s:Body><u:BrowseQueue xmlns:u="` + ns + `"><QueueName>CurrentQueue</QueueName></u:BrowseQueue></s:Body></s:Envelope>`
	req := httptest.NewRequest(http.MethodPost, "http://127.0.0.1:49152/PlayQueue/control", strings.NewReader(body))
	req.RemoteAddr = "127.0.0.1:54321"
	req.Header.Set("SOAPAction", `"`+ns+`#BrowseQueue"`)
	w := httptest.NewRecorder()
	d.ServeHTTP(w, req)
	if w.Code != 200 || w.Body.String() != want {
		t.Fatalf("streaming BrowseQueue mismatch:\ngot:  %s\nwant: %s", w.Body.String(), want)
	}
}

func BenchmarkBrowseQueueLarge(b *testing.B) {
	d := New("Desk", "uuid:bench", "127.0.0.1:49152", netip.MustParsePrefix("127.0.0.1/32"), 100, nil)
	defer d.Close()

	items := make([]track, 10000)
	for i := range items {
		items[i] = track{
			uri:      fmt.Sprintf("http://192.168.1.10:8080/media/tracks/%05d.flac?token=abc&fmt=flac", i),
			metadata: fmt.Sprintf(`<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"><item id="%d"><dc:title>Track %d &amp; Friends</dc:title></item></DIDL-Lite>`, i, i),
		}
	}
	d.mu.Lock()
	d.t.q = playlist{name: "tacet", items: items, mode: normal}
	d.mu.Unlock()

	ns := serviceType(pq)
	body := `<s:Envelope xmlns:s="` + soapNS + `"><s:Body><u:BrowseQueue xmlns:u="` + ns + `"><QueueName>CurrentQueue</QueueName></u:BrowseQueue></s:Body></s:Envelope>`

	b.ReportAllocs()
	b.ResetTimer()
	for range b.N {
		req := httptest.NewRequest(http.MethodPost, "http://127.0.0.1:49152/PlayQueue/control", strings.NewReader(body))
		req.RemoteAddr = "127.0.0.1:54321"
		req.Header.Set("SOAPAction", `"`+ns+`#BrowseQueue"`)
		w := &discardResponseWriter{hdr: make(http.Header)}
		d.ServeHTTP(w, req)
		if w.code != 200 || w.n == 0 {
			b.Fatalf("unexpected response: code=%d bytes=%d", w.code, w.n)
		}
	}
}

type discardResponseWriter struct {
	hdr  http.Header
	code int
	n    int
}

func (w *discardResponseWriter) Header() http.Header { return w.hdr }
func (w *discardResponseWriter) WriteHeader(code int) {
	w.code = code
}
func (w *discardResponseWriter) Write(p []byte) (int, error) {
	if w.code == 0 {
		w.code = 200
	}
	w.n += len(p)
	return io.Discard.Write(p)
}

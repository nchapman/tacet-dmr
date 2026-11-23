package mpvplayer

import (
	"errors"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/huin/goupnp"
	"github.com/huin/goupnp/dcps/av1"
	"github.com/huin/goupnp/soap"
	"github.com/nchapman/tacet-dmr/internal/renderer"
)

// Use the same generated control-point client as Tacet, not a hand-authored
// imitation of its SOAP messages. No dependency on a running Tacet or a DAC.
func TestRendererWithAV1Controller(t *testing.T) {
	srv := httptest.NewUnstartedServer(nil)
	d := renderer.New("Study & DAC", "uuid:0ee23216-478e-49b5-9353-79741b89dd6b", srv.Listener.Addr().String(), netip.MustParseAddr("127.0.0.1"), func() (renderer.Engine, error) { return OpenNetwork("null/", false) })
	defer d.Close()
	srv.Config.Handler = d
	srv.Start()
	defer srv.Close()
	u, _ := url.Parse(srv.URL + "/description.xml")
	root, err := goupnp.DeviceByURL(u)
	if err != nil {
		t.Fatal(err)
	}
	if root.Device.FriendlyName != "Study & DAC" || len(root.Device.FindService("urn:schemas-wiimu-com:service:PlayQueue:1")) != 1 {
		t.Fatalf("device: %+v", root.Device)
	}
	avs, err := av1.NewAVTransport1ClientsFromRootDevice(root, u)
	if err != nil || len(avs) != 1 {
		t.Fatalf("AVTransport: %v", err)
	}
	av := avs[0]
	spec, err := av.Service.RequestSCDP()
	if err != nil {
		t.Fatal(err)
	}
	if spec.GetAction("Seek") == nil || spec.GetAction("SetNextAVTransportURI") == nil {
		t.Fatal("incorrect optional actions")
	}
	cms, err := av1.NewConnectionManager1ClientsFromRootDevice(root, u)
	if err != nil || len(cms) != 1 {
		t.Fatalf("ConnectionManager: %v", err)
	}
	_, sink, err := cms[0].GetProtocolInfo()
	if err != nil || !strings.Contains(sink, "audio/flac") {
		t.Fatalf("protocolInfo: %s %v", sink, err)
	}
	rcs, err := av1.NewRenderingControl1ClientsFromRootDevice(root, u)
	if err != nil || len(rcs) != 1 {
		t.Fatalf("RenderingControl: %v", err)
	}
	rc := rcs[0]
	check := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	expect := func(state, status string) {
		t.Helper()
		deadline := time.Now().Add(5 * time.Second)
		for {
			st, ss, _, err := av.GetTransportInfo(0)
			check(err)
			if st == state && ss == status {
				return
			}
			if time.Now().After(deadline) {
				t.Fatalf("want %s/%s, last got %s/%s", state, status, st, ss)
			}
			time.Sleep(15 * time.Millisecond)
		}
	}
	expect("NO_MEDIA_PRESENT", "OK")
	media := server(t, 8000*4) + "?id=1&n=2"
	meta := `<DIDL-Lite><title>A &amp; B</title></DIDL-Lite>`
	check(av.SetAVTransportURI(0, media, meta))
	expect("STOPPED", "OK")
	check(av.Play(0, "1"))
	expect("PLAYING", "OK")
	check(rc.SetVolume(0, "Master", 30))
	v, err := rc.GetVolume(0, "Master")
	if err != nil || v != 30 {
		t.Fatalf("volume %d %v", v, err)
	}
	check(rc.SetMute(0, "Master", true))
	m, err := rc.GetMute(0, "Master")
	if err != nil || !m {
		t.Fatalf("mute %t %v", m, err)
	}
	check(av.Pause(0))
	expect("PAUSED_PLAYBACK", "OK")
	check(av.Seek(0, "REL_TIME", "0:00:03"))
	_, dur, metadata, uri, _, _, _, _, err := av.GetPositionInfo(0)
	check(err)
	if uri != media || metadata != meta || dur != "0:00:04" {
		t.Fatalf("position: %q %q %q", uri, metadata, dur)
	}
	next := server(t, 8000) + "?track=second"
	check(av.SetNextAVTransportURI(0, next, "next metadata"))
	_, _, _, _, nextURI, nextMeta, _, _, _, err := av.GetMediaInfo(0)
	check(err)
	if nextURI != next || nextMeta != "next metadata" {
		t.Fatalf("pending next: %s %s", nextURI, nextMeta)
	}
	check(av.Play(0, "1"))
	expect("STOPPED", "OK")
	_, _, _, uri, _, _, _, _, err = av.GetPositionInfo(0)
	check(err)
	if uri != next {
		t.Fatalf("EOF did not consume next URI: %s", uri)
	}
	check(av.Play(0, "1"))
	expect("PLAYING", "OK")
	check(av.Stop(0))
	expect("STOPPED", "OK")
	var f *soap.SOAPFaultError
	err = av.Record(0)
	if !errors.As(err, &f) || f.Detail.UPnPError.Errorcode != 401 {
		t.Fatalf("unsupported action: %v", err)
	}
	err = av.SetAVTransportURI(0, "file:///not-allowed", "")
	if !errors.As(err, &f) || f.Detail.UPnPError.Errorcode != 714 {
		t.Fatalf("URI validation: %v", err)
	}
	bad := httptest.NewServer(http.NotFoundHandler())
	defer bad.Close()
	check(av.SetAVTransportURI(0, bad.URL, ""))
	check(av.Play(0, "1"))
	expect("STOPPED", "ERROR_OCCURRED")
	check(av.SetAVTransportURI(0, "", ""))
	expect("NO_MEDIA_PRESENT", "OK")
}

package mpvplayer

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/huin/goupnp/dcps/av1"
	"github.com/nchapman/tacet-dmr/internal/renderer"
	wiim "github.com/shumatech/wiimplay/upnp"
)

// The PCM AO is unclocked: this proves sample continuity, not real DAC timing.
// Do not append across AO opens: reopening the output would hide a broken join.
func TestNetworkJoinMatchesSourcePCM(t *testing.T) {
	const split = 13579
	const frames = 32000
	whole := wav(frames)[44:]
	for i := 0; i < frames; i++ {
		binary.LittleEndian.PutUint16(whole[i*2:], uint16((i*73)%29001-14500))
	}
	part := func(pcm []byte) []byte {
		b := wav(len(pcm) / 2)
		copy(b[44:], pcm)
		return b
	}
	first, second := part(whole[:2*split]), part(whole[2*split:])
	media := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var b []byte
		switch r.URL.Path {
		case "/a":
			b = first
		case "/b":
			b = second
		case "/handoff":
			http.Redirect(w, r, "/b", 302)
			return
		default:
			http.NotFound(w, r)
			return
		}
		http.ServeContent(w, r, "part.wav", time.Time{}, bytes.NewReader(b))
	}))
	defer media.Close()
	for _, tc := range []struct {
		name, next, status string
		want               []byte
		index              uint32
	}{
		{"AVTransport", "/handoff", "OK", whole, 2},
		{"PlayQueue", "/handoff", "OK", whole, 2},
		{"missing successor", "/missing", "ERROR_OCCURRED", whole[:2*split], 1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "capture.pcm")
			srv := httptest.NewUnstartedServer(nil)
			d := renderer.New("Capture", "uuid:fixture", srv.Listener.Addr().String(), netip.MustParseAddr("127.0.0.1"), func() (renderer.Engine, error) {
				p, err := OpenNetwork("pcm/", false)
				if err != nil {
					return nil, err
				}
				for _, kv := range [][2]string{{"ao-pcm-file", path}, {"ao-pcm-waveheader", "no"}, {"audio-format", "s16"}, {"audio-channels", "mono"}} {
					if err := p.m.SetPropertyString(kv[0], kv[1]); err != nil {
						p.Close()
						return nil, fmt.Errorf("PCM setup: %w", err)
					}
				}
				return p, nil
			})
			defer d.Close()
			srv.Config.Handler = d
			srv.Start()
			defer srv.Close()
			loc, _ := url.Parse(srv.URL + "/description.xml")
			clients, err := av1.NewAVTransport1ClientsByURL(loc)
			if err != nil || len(clients) != 1 {
				t.Fatalf("AVTransport: %v", err)
			}
			av := clients[0]
			check := func(err error) {
				t.Helper()
				if err != nil {
					t.Fatal(err)
				}
			}
			uris := []string{media.URL + "/a", media.URL + tc.next}
			var queue *wiim.PlayQueue1
			if tc.name == "AVTransport" {
				check(av.SetAVTransportURI(0, uris[0], ""))
				check(av.SetNextAVTransportURI(0, uris[1], ""))
				check(av.Play(0, "1"))
			} else {
				qs, err := wiim.NewPlayQueue1ClientsByURLCtx(t.Context(), loc)
				check(err)
				if len(qs) != 1 {
					t.Fatal("missing PlayQueue")
				}
				queue = qs[0]
				check(queue.CreateQueueCtx(t.Context(), fmt.Sprintf(`<PlayList><ListName>capture</ListName><ListInfo><TrackNumber>2</TrackNumber></ListInfo><Tracks><Track1><URL>%s</URL></Track1><Track2><URL>%s</URL></Track2></Tracks></PlayList>`, uris[0], uris[1])))
				check(queue.PlayQueueWithIndexCtx(t.Context(), "CurrentQueue", 1))
			}
			deadline := time.Now().Add(5 * time.Second)
			for {
				state, status, _, err := av.GetTransportInfo(0)
				check(err)
				if state == "STOPPED" {
					if status != tc.status {
						t.Fatalf("terminal status %s; want %s", status, tc.status)
					}
					break
				}
				if time.Now().After(deadline) {
					t.Fatal("did not reach final EOF")
				}
				time.Sleep(5 * time.Millisecond)
			}
			_, duration, _, uri, pos, _, _, _, err := av.GetPositionInfo(0)
			check(err)
			seconds := len(second[44:]) / 2 / 8000
			if tc.index == 1 {
				seconds = split / 8000
			}
			wantTime := fmt.Sprintf("0:00:%02d", seconds)
			if pos != wantTime || duration != wantTime {
				t.Errorf("terminal position/duration %s/%s, want %s", pos, duration, wantTime)
			}
			if uri != uris[tc.index-1] {
				t.Fatalf("wrong selected URI %s", uri)
			}
			if queue != nil {
				index, _, err := queue.GetQueueIndexCtx(t.Context(), "CurrentQueue")
				check(err)
				if index != tc.index {
					t.Fatalf("queue index %d, want %d", index, tc.index)
				}
			}
			got, err := os.ReadFile(path)
			check(err)
			if !bytes.Equal(got, tc.want) {
				offset := 0
				for offset < min(len(got), len(tc.want)) && got[offset] == tc.want[offset] {
					offset++
				}
				t.Fatalf("capture %d frames, want %d; first mismatch at frame %d (join %d)", len(got)/2, len(tc.want)/2, offset/2, split)
			}
		})
	}
}

package interop_test

import (
	"encoding/xml"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"testing"

	"github.com/nchapman/tacet-dmr/internal/renderer"
	wiim "github.com/shumatech/wiimplay/upnp"
)

// A fixed, independently generated vendor client in addition to the dynamic
// SCPD suite. Its older GetQueueIndex decoder requires CurrentPage, ignores
// newer fields, and does not consult our service description for their types.
func TestGeneratedWiiMQueueClient(t *testing.T) {
	srv := httptest.NewUnstartedServer(nil)
	dev := renderer.New("Fixture", "uuid:test", srv.Listener.Addr().String(), netip.MustParsePrefix("127.0.0.1/32"), 100, nil)
	defer dev.Close()
	srv.Config.Handler = dev
	srv.Start()
	defer srv.Close()
	location, _ := url.Parse(srv.URL + "/description.xml")
	peers, err := wiim.NewPlayQueue1ClientsByURLCtx(t.Context(), location)
	if err != nil || len(peers) != 1 {
		t.Fatalf("discover PlayQueue: %d, %v", len(peers), err)
	}
	p := peers[0]
	inventory := func() wiim.TotalPlayQueueXml {
		t.Helper()
		raw, err := p.BrowseQueueCtx(t.Context(), "TotalQueue")
		if err != nil {
			t.Fatal(err)
		}
		var view wiim.TotalPlayQueueXml
		if err := xml.Unmarshal([]byte(raw), &view); err != nil {
			t.Fatal(err)
		}
		return view
	}
	if inventory().TotalQueue != 0 {
		t.Fatal("nonempty initial inventory")
	}
	err = p.CreateQueueCtx(t.Context(), `<PlayList><ListName>fixture</ListName><ListInfo><TrackNumber>1</TrackNumber></ListInfo><Tracks><Track1><URL>http://127.0.0.1/not-fetched</URL></Track1></Tracks></PlayList>`)
	if err != nil {
		t.Fatal(err)
	}
	view := inventory()
	var list wiim.TotalPlayListXml
	if err := xml.Unmarshal([]byte(view.PlayListInfo.InnerXml), &list); err != nil {
		t.Fatal(err)
	}
	if view.TotalQueue != 1 || view.CurrentPlayList != "fixture" || list.Name != "fixture" || list.ListInfo.TrackNumber != 1 {
		t.Fatalf("incorrect inventory: %+v %+v", view, list)
	}
	index, page, err := p.GetQueueIndexCtx(t.Context(), "CurrentQueue")
	if err != nil || index != 1 || page != 1 {
		t.Fatalf("GetQueueIndex: index=%d page=%d err=%v", index, page, err)
	}
}

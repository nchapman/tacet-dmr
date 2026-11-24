package renderer

import (
	"context"
	"io"
	"net/netip"
	"os"
	"testing"

	ssdp "github.com/koron/go-ssdp"
)

func TestServeControllerSubnet(t *testing.T) {
	for _, tc := range []struct {
		controller string
		valid      bool
	}{{"192.168.68.0/22", true}, {"192.168.68.42", true}, {"", false}, {"0.0.0.0/0", false}, {"192.168.0.0/15", false}, {"192.168.68.42/22", false}, {"::1/128", false}} {
		t.Run(tc.controller, func(t *testing.T) {
			ctx, cancel := context.WithCancel(t.Context())
			cancel()
			err := Serve(ctx, Config{Listen: "127.0.0.1:0", Controller: tc.controller, Name: "test", NoSSDP: true}, nil, io.Discard)
			if (err == nil) != tc.valid {
				t.Fatalf("controller %q: %v", tc.controller, err)
			}
		})
	}
}

// Opt-in real UDP smoke test. Never announce a test renderer on the LAN.
func TestLoopbackDiscovery(t *testing.T) {
	if os.Getenv("TACET_DMR_TEST_SSDP") != "1" {
		t.Skip("set TACET_DMR_TEST_SSDP=1 on a multicast-capable loopback")
	}
	const udn = "uuid:5c4c0432-7970-4405-8a73-a2f4d7eec20d"
	const location = "http://127.0.0.1:12345/description.xml"
	prior := ssdp.Interfaces
	t.Cleanup(func() { ssdp.Interfaces = prior })
	ads, err := startSSDP(netip.MustParseAddr("127.0.0.1"), udn, location)
	if err != nil {
		t.Fatal(err)
	}
	defer ads.close()
	st := serviceType(av)
	found, err := ssdp.Search(st, 1, "127.0.0.1:0", ssdp.TTL(1))
	if err != nil {
		t.Fatal(err)
	}
	for _, r := range found {
		if r.Type == st && r.USN == udn+"::"+st && r.Location == location {
			return
		}
	}
	t.Fatalf("AVTransport M-SEARCH did not find test renderer: %v", found)
}

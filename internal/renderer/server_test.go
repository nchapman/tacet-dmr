package renderer

import (
	"net/netip"
	"os"
	"testing"

	ssdp "github.com/koron/go-ssdp"
)

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

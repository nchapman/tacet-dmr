package renderer

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net"
	"net/netip"
	"os"
	"strings"
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

func TestResolveListen(t *testing.T) {
	cidr := func(s string) net.Addr {
		ip, ipnet, err := net.ParseCIDR(s)
		if err != nil {
			t.Fatal(err)
		}
		return &net.IPNet{IP: ip, Mask: ipnet.Mask}
	}
	ifaces := map[string]struct {
		flags net.Flags
		addrs []net.Addr
	}{
		"en0":  {net.FlagUp | net.FlagMulticast, []net.Addr{cidr("fe80::1/64"), cidr("169.254.10.2/16"), cidr("192.168.68.71/22")}},
		"ts0":  {net.FlagUp, []net.Addr{cidr("100.88.248.12/32")}},
		"ll0":  {net.FlagUp, []net.Addr{cidr("169.254.10.2/16")}},
		"lo0":  {net.FlagUp | net.FlagLoopback, []net.Addr{cidr("::1/128"), cidr("127.0.0.1/8")}},
		"down": {0, []net.Addr{cidr("192.168.1.10/24")}},
		"pub0": {net.FlagUp, []net.Addr{cidr("203.0.113.5/24")}},
		"v6":   {net.FlagUp, []net.Addr{cidr("fd00::1/64")}},
	}
	lookup := func(name string) (net.Flags, []net.Addr, error) {
		v, ok := ifaces[name]
		if !ok {
			return 0, nil, errors.New("no such network interface")
		}
		return v.flags, v.addrs, nil
	}
	for _, tc := range []struct {
		listen string
		want   string
	}{
		{"192.168.68.71:49152", "192.168.68.71:49152"},
		{"100.88.248.12:49152", "100.88.248.12:49152"},
		{"127.0.0.1:0", "127.0.0.1:0"},
		{"en0:49152", "192.168.68.71:49152"},
		{"ts0:49152", "100.88.248.12:49152"},
		{"100.128.0.1:49152", ""},
		{"ll0:49152", "169.254.10.2:49152"},
		{"lo0:0", "127.0.0.1:0"},
		{":49152", ""},
		{"0.0.0.0:49152", ""},
		{"203.0.113.5:49152", ""},
		{"[::1]:49152", ""},
		{"down:49152", ""},
		{"pub0:49152", ""},
		{"v6:49152", ""},
		{"missing0:49152", ""},
		{"en0:abc", ""},
		{"en0:70000", ""},
		{"en0:-1", ""},
	} {
		got, err := resolveListen(tc.listen, lookup)
		if tc.want == "" {
			if err == nil {
				t.Errorf("resolveListen(%q) = %s, want error", tc.listen, got)
			}
			continue
		}
		if err != nil || got.String() != tc.want {
			t.Errorf("resolveListen(%q) = (%s, %v), want %s", tc.listen, got, err, tc.want)
		}
	}
}

func TestServeByLoopbackInterfaceName(t *testing.T) {
	iface, err := interfaceFor(netip.MustParseAddr("127.0.0.1"))
	if err != nil {
		t.Skipf("no loopback interface owning 127.0.0.1: %v", err)
	}
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	var out bytes.Buffer
	if err := Serve(ctx, Config{Listen: iface.Name + ":0", Controller: "127.0.0.1", Name: "IfaceTest", NoSSDP: true}, nil, &out); err != nil {
		t.Fatalf("Serve(%s:0): %v", iface.Name, err)
	}
	if !strings.Contains(out.String(), "TACET_RENDERER_URL=http://127.0.0.1:") {
		t.Fatalf("unexpected startup output: %q", out.String())
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
	ads, err := startSSDP(netip.MustParseAddr("127.0.0.1"), udn, location, "test")
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

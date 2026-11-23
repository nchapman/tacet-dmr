package renderer

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"os"
	"runtime"
	"time"

	"github.com/google/uuid"
	ssdp "github.com/koron/go-ssdp"
)

type Config struct {
	Listen, Controller, Name, UUID string
	NoSSDP                         bool
}

func lan(ip netip.Addr) bool {
	return ip.Is4() && (ip.IsLoopback() || ip.IsPrivate() || ip.IsLinkLocalUnicast())
}
func identity(id string) (string, error) {
	if id != "" {
		u, err := uuid.Parse(id)
		if err != nil {
			return "", fmt.Errorf("invalid renderer UUID: %w", err)
		}
		return "uuid:" + u.String(), nil
	}
	host, err := os.Hostname()
	if err != nil {
		return "", err
	}
	return "uuid:" + uuid.NewSHA1(uuid.NameSpaceDNS, []byte("tacet-dmr:"+host)).String(), nil
}
func interfaceFor(ip netip.Addr) (*net.Interface, error) {
	ifs, err := net.Interfaces()
	if err != nil {
		return nil, err
	}
	for _, i := range ifs {
		addrs, err := i.Addrs()
		if err != nil {
			continue
		}
		for _, a := range addrs {
			p, err := netip.ParsePrefix(a.String())
			if err == nil && p.Addr().Unmap() == ip {
				return &i, nil
			}
		}
	}
	return nil, fmt.Errorf("no interface owns %s", ip)
}

type advertisements []*ssdp.Advertiser

func (ads advertisements) close() {
	for _, ad := range ads {
		_ = ad.Bye()
		_ = ad.Close()
	}
}
func (ads advertisements) alive() error {
	for _, ad := range ads {
		if err := ad.Alive(); err != nil {
			return err
		}
	}
	return nil
}
func startSSDP(ip netip.Addr, udn, location string) (advertisements, error) {
	iface, err := interfaceFor(ip)
	if err != nil {
		return nil, err
	}
	if iface.Flags&net.FlagMulticast == 0 {
		return nil, fmt.Errorf("interface %s does not support multicast; use -no-ssdp for direct URL", iface.Name)
	}
	// go-ssdp has process-wide interface selection. The CLI runs one renderer.
	ssdp.Interfaces = []net.Interface{*iface}
	targets := []string{ssdp.RootDevice, udn, deviceType}
	for _, s := range services {
		targets = append(targets, serviceType(s))
	}
	var ads advertisements
	for _, st := range targets {
		usn := udn
		if st != udn {
			usn += "::" + st
		}
		ad, err := ssdp.Advertise(st, usn, location, runtime.GOOS+"/1 UPnP/1.0 tacet-dmr/0.1", 1800, ssdp.TTL(1))
		if err != nil {
			ads.close()
			return nil, err
		}
		ads = append(ads, ad)
	}
	if err := ads.alive(); err != nil {
		ads.close()
		return nil, err
	}
	return ads, nil
}
func Serve(ctx context.Context, c Config, open Factory, out io.Writer) error {
	ap, err := netip.ParseAddrPort(c.Listen)
	if err != nil || !lan(ap.Addr()) {
		return errors.New("-serve requires a concrete private, link-local or loopback IPv4:port, not a wildcard")
	}
	peer, err := netip.ParseAddr(c.Controller)
	if err != nil || !lan(peer) {
		return errors.New("-controller requires the trusted Tacet server's LAN IPv4 address")
	}
	if c.Name == "" {
		return errors.New("renderer name cannot be empty")
	}
	udn, err := identity(c.UUID)
	if err != nil {
		return err
	}
	ln, err := net.Listen("tcp4", ap.String())
	if err != nil {
		return err
	}
	defer ln.Close()
	host := ln.Addr().String()
	location := "http://" + host + "/description.xml"
	d := New(c.Name, udn, host, peer, open)
	defer d.Close()
	srv := &http.Server{Handler: d, ReadHeaderTimeout: 3 * time.Second, ReadTimeout: 8 * time.Second, WriteTimeout: 10 * time.Second, IdleTimeout: 15 * time.Second, MaxHeaderBytes: 16 << 10}
	defer srv.Close()
	ended := make(chan error, 1)
	go func() { ended <- srv.Serve(ln) }()
	var ads advertisements
	if !c.NoSSDP {
		ads, err = startSSDP(ap.Addr(), udn, location)
		if err != nil {
			return fmt.Errorf("start SSDP: %w", err)
		}
		defer ads.close()
	}
	fmt.Fprintf(out, "%s (%s)\nTACET_RENDERER_URL=%s\nController: %s · SSDP enabled: %t\n", c.Name, udn, location, peer, !c.NoSSDP)
	tick := time.NewTicker(12 * time.Minute)
	defer tick.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil
		case err := <-ended:
			return err
		case <-tick.C:
			if err := ads.alive(); err != nil {
				return fmt.Errorf("refresh SSDP: %w", err)
			}
		}
	}
}

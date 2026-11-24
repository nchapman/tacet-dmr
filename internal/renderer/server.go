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
	"strconv"
	"time"

	"github.com/google/uuid"
	ssdp "github.com/koron/go-ssdp"
	"github.com/nchapman/tacet-dmr/internal/playback"
)

type Config struct {
	Listen, Controller, Name, UUID string
	NoSSDP                         bool
	Exclusive                      bool                          // defaults software volume to 30 when InitialVolume is nil
	InitialVolume                  *int                          // nil: 100 shared, 30 exclusive software, or current device level for hardware
	Volume                         playback.VolumeControl        // nil: mpv handles volume and mute (default)
	ProbeVolume                    func() playback.VolumeControl // optional hotplug probe when Volume is nil
	Log                            io.Writer                     // optional operational log sink (nil disables logging)
}

func (c Config) StartupVolume() int {
	if c.InitialVolume != nil {
		return *c.InitialVolume
	}
	if c.Exclusive {
		return 30
	}
	return 100
}

func lan(ip netip.Addr) bool {
	return ip.Is4() && (ip.IsLoopback() || ip.IsPrivate() || ip.IsLinkLocalUnicast())
}

func systemInterface(name string) (net.Flags, []net.Addr, error) {
	iface, err := net.InterfaceByName(name)
	if err != nil {
		return 0, nil, err
	}
	addrs, err := iface.Addrs()
	if err != nil {
		return 0, nil, err
	}
	return iface.Flags, addrs, nil
}

func resolveListen(listen string, lookup func(string) (net.Flags, []net.Addr, error)) (netip.AddrPort, error) {
	if ap, err := netip.ParseAddrPort(listen); err == nil {
		if !lan(ap.Addr()) {
			return netip.AddrPort{}, errors.New("-serve requires a concrete private, link-local or loopback IPv4:port or interface:port, not a wildcard")
		}
		return ap, nil
	}
	host, portStr, err := net.SplitHostPort(listen)
	if err != nil || host == "" {
		return netip.AddrPort{}, errors.New("-serve requires a concrete private, link-local or loopback IPv4:port or interface:port, not a wildcard")
	}
	if _, err := netip.ParseAddr(host); err == nil {
		return netip.AddrPort{}, errors.New("-serve requires a concrete private, link-local or loopback IPv4:port or interface:port, not a wildcard")
	}
	port, err := strconv.Atoi(portStr)
	if err != nil || port < 0 || port > 65535 {
		return netip.AddrPort{}, fmt.Errorf("-serve port %q must be 0–65535", portStr)
	}
	flags, addrs, err := lookup(host)
	if err != nil {
		return netip.AddrPort{}, fmt.Errorf("-serve interface %q: %w", host, err)
	}
	if flags&net.FlagUp == 0 {
		return netip.AddrPort{}, fmt.Errorf("-serve interface %q is down", host)
	}
	var best netip.Addr
	rank := func(ip netip.Addr) int {
		switch {
		case ip.IsPrivate():
			return 3
		case ip.IsLinkLocalUnicast():
			return 2
		case ip.IsLoopback():
			return 1
		default:
			return 0
		}
	}
	for _, a := range addrs {
		p, err := netip.ParsePrefix(a.String())
		if err != nil {
			continue
		}
		ip := p.Addr().Unmap()
		if !lan(ip) {
			continue
		}
		if !best.IsValid() || rank(ip) > rank(best) {
			best = ip
		}
	}
	if !best.IsValid() {
		return netip.AddrPort{}, fmt.Errorf("-serve interface %q has no private, link-local or loopback IPv4 address", host)
	}
	return netip.AddrPortFrom(best, uint16(port)), nil
}

// A bare IP keeps the original single-controller policy. Require an explicit
// network address and keep the entire prefix inside one LAN address range.
func controllerNetwork(raw string) (netip.Prefix, error) {
	if ip, err := netip.ParseAddr(raw); err == nil && lan(ip) {
		return netip.PrefixFrom(ip, 32), nil
	}
	if p, err := netip.ParsePrefix(raw); err == nil && p == p.Masked() {
		for _, block := range []string{"10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16", "127.0.0.0/8"} {
			local := netip.MustParsePrefix(block)
			if p.Bits() >= local.Bits() && local.Contains(p.Addr()) {
				return p, nil
			}
		}
	}
	return netip.Prefix{}, errors.New("-controller requires a LAN IPv4 address or trusted LAN subnet such as 192.168.1.0/24")
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
	ap, err := resolveListen(c.Listen, systemInterface)
	if err != nil {
		return err
	}
	peers, err := controllerNetwork(c.Controller)
	if err != nil {
		return err
	}
	volume := c.StartupVolume()
	if volume < 0 || volume > 100 {
		return errors.New("initial volume must be 0–100")
	}
	if c.Name == "" {
		return errors.New("renderer name cannot be empty")
	}
	udn, err := identity(c.UUID)
	if err != nil {
		return err
	}
	vol := c.Volume
	if vol == nil && c.ProbeVolume != nil {
		vol = c.ProbeVolume()
	}
	if c.InitialVolume != nil && vol != nil {
		if err := vol.SetVolume(*c.InitialVolume); err != nil {
			return fmt.Errorf("set initial volume: %w", err)
		}
	}
	ln, err := net.Listen("tcp4", ap.String())
	if err != nil {
		return err
	}
	defer ln.Close()
	host := ln.Addr().String()
	location := "http://" + host + "/description.xml"
	d := newDevice(c.Name, udn, host, peers, deviceOptions{
		exclusive:     c.Exclusive,
		initialVolume: c.InitialVolume,
		volume:        vol,
		probeVolume:   c.ProbeVolume,
		log:           c.Log,
	}, open)
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
	volumeBackend := "mpv software"
	if vol != nil {
		volumeBackend = fmt.Sprint(vol)
	} else if c.ProbeVolume != nil {
		volumeBackend = "mpv software (watching for hardware device)"
	}
	fmt.Fprintf(out, "%s (%s)\nTACET_RENDERER_URL=%s\nController: %s · SSDP enabled: %t · Volume: %s\n", c.Name, udn, location, peers, !c.NoSSDP, volumeBackend)
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

package main

import (
	"bufio"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/nchapman/tacet-dmr/internal/mpvplayer"
	"github.com/nchapman/tacet-dmr/internal/playback"
	"github.com/nchapman/tacet-dmr/internal/renderer"
)

func main() {
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	if err := run(ctx, os.Args[1:], os.Stdin, os.Stdout); err != nil && !errors.Is(err, flag.ErrHelp) {
		fmt.Fprintln(os.Stderr, "tacet-dmr:", err)
		os.Exit(1)
	}
}

func run(ctx context.Context, args []string, in io.Reader, out io.Writer) error {
	fs := flag.NewFlagSet("tacet-dmr", flag.ContinueOnError)
	list := fs.Bool("list-devices", false, "list audio output device IDs and exit")
	device := fs.String("device", "", "mpv output device ID (default: automatic)")
	excl := fs.Bool("exclusive", false, "request exclusive audio on a named native device")
	interactive := fs.Bool("interactive", false, "read play/pause/seek/status/stop commands from stdin")
	var server renderer.Config
	fs.StringVar(&server.Listen, "serve", "", "host a UPnP renderer on this LAN IPv4:port")
	fs.StringVar(&server.Controller, "controller", "", "trusted controller IPv4 or LAN CIDR, e.g. 192.168.1.0/24 (required with -serve)")
	fs.StringVar(&server.Name, "name", "Tacet Renderer", "UPnP friendly name")
	fs.StringVar(&server.UUID, "uuid", "", "override stable renderer UUID (default: derived from hostname)")
	fs.BoolVar(&server.NoSSDP, "no-ssdp", false, "disable multicast discovery; use the printed description URL")
	fs.Usage = func() {
		fmt.Fprintln(fs.Output(), "Usage: tacet-dmr [flags] FILE-OR-HTTP-URL ...\n       tacet-dmr -serve LAN-IP:PORT -controller TACET-IP [flags]")
		fs.PrintDefaults()
	}
	if err := fs.Parse(args); err != nil {
		return err
	}
	if server.Listen != "" {
		if *list || *interactive || fs.NArg() != 0 {
			return errors.New("-serve cannot be combined with local tracks, -interactive or -list-devices")
		}
		return renderer.Serve(ctx, server, func() (renderer.Engine, error) { return mpvplayer.OpenNetwork(*device, *excl) }, out)
	}
	if *list && fs.NArg() != 0 {
		return errors.New("-list-devices takes no tracks")
	}
	if !*list && fs.NArg() == 0 {
		fs.Usage()
		return errors.New("no tracks supplied")
	}
	var tracks []string
	for i, arg := range fs.Args() {
		track, err := mediaSource(arg)
		if err != nil {
			return fmt.Errorf("track %d: %w", i+1, err)
		}
		tracks = append(tracks, track)
	}
	p, err := mpvplayer.Open(*device, *excl)
	if err != nil {
		return err
	}
	defer p.Close()
	if *list {
		devices, err := p.Devices()
		if err != nil {
			return err
		}
		for _, d := range devices {
			fmt.Fprintf(out, "%s\t%s\n", d.ID, d.Description)
		}
		return nil
	}
	var commands <-chan string
	if *interactive {
		fmt.Fprintln(out, "Commands: pause, play, seek <duration> (e.g. 1m20s), status, stop")
		lines := make(chan string)
		commands = lines
		go func() {
			defer close(lines)
			scanner := bufio.NewScanner(in)
			for scanner.Scan() {
				select {
				case lines <- scanner.Text():
				case <-ctx.Done():
					return
				}
			}
			if err := scanner.Err(); err != nil {
				fmt.Fprintln(os.Stderr, "tacet-dmr: interactive input disabled:", err)
			}
		}()
	}
	return play(ctx, p, tracks, commands, out)
}

func mediaSource(raw string) (string, error) {
	if raw == "" || strings.ContainsRune(raw, 0) {
		return "", errors.New("empty or invalid source")
	}
	if !filepath.IsAbs(raw) && strings.Contains(raw, "://") {
		u, err := url.Parse(raw)
		if err != nil || u.Hostname() == "" || (u.Scheme != "http" && u.Scheme != "https") {
			return "", errors.New("expected a local file or HTTP(S) URL")
		}
		return raw, nil
	}
	path, err := filepath.Abs(raw)
	if err != nil {
		return "", err
	}
	info, err := os.Stat(path)
	if err != nil {
		return "", err
	}
	if !info.Mode().IsRegular() {
		return "", errors.New("expected a regular audio file")
	}
	return path, nil
}

func play(ctx context.Context, p playback.Player, tracks []string, commands <-chan string, out io.Writer) error {
	if err := p.Prepare(tracks); err != nil {
		return err
	}
	if err := p.Pause(false); err != nil {
		return err
	}
	for finished := 0; finished < len(tracks); {
		select {
		case <-ctx.Done():
			return p.Stop()
		case cmd, ok := <-commands:
			if !ok {
				commands = nil
			} else if strings.TrimSpace(cmd) == "stop" {
				return p.Stop()
			} else if err := control(p, cmd, out); err != nil {
				fmt.Fprintln(out, "control:", err)
			}
		default:
		}
		e, err := p.NextEvent(50 * time.Millisecond)
		if err != nil {
			return fmt.Errorf("track %d: %w", finished+1, err)
		}
		switch e {
		case playback.Stopped:
			return fmt.Errorf("track %d: playback stopped before the queue finished", finished+1)
		case playback.TrackFinished:
			finished++
		case playback.TrackStarted:
			fmt.Fprintf(out, "Loaded track %d/%d\n", finished+1, len(tracks))
		case playback.OutputChanged:
			s := p.Status()
			if s.Output != "" && s.SourceRate > 0 && s.OutputRate > 0 {
				fmt.Fprintf(out, "Output: %s · %d Hz source → %d Hz output\n", s.Output, s.SourceRate, s.OutputRate)
			}
		}
	}
	return nil
}

func control(p playback.Player, line string, out io.Writer) error {
	args := strings.Fields(line)
	if len(args) == 0 {
		return nil
	}
	if len(args) == 2 && args[0] == "seek" {
		pos, err := time.ParseDuration(args[1])
		if err != nil {
			return err
		}
		return p.Seek(pos)
	}
	if len(args) == 1 {
		switch args[0] {
		case "play":
			return p.Pause(false)
		case "pause":
			return p.Pause(true)
		case "status":
			s := p.Status()
			fmt.Fprintf(out, "Track %d · %s / %s · paused=%t · %s\n", s.Track, s.Position.Round(time.Millisecond), s.Duration.Round(time.Millisecond), s.Paused, s.Output)
			return nil
		}
	}
	return errors.New("expected play, pause, seek <duration>, status, or stop")
}

// Loaded into Tacet's own linkplay test package by scripts/test-tacet.sh.
package linkplay_test

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	"tacet/internal/linkplay"
	"tacet/internal/upnp"
)

func TestTacetDMRQueuePeer(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	ip := os.Getenv("DMR_TEST_IP")
	cmd := exec.CommandContext(ctx, os.Getenv("DMR_TEST_BIN"), "-serve", ip+":0", "-controller", ip, "-no-ssdp", "-device", "null/")
	cmd.Env = append(os.Environ(), "DYLD_FALLBACK_LIBRARY_PATH="+os.Getenv("DMR_TEST_LIB"))
	cmd.Stderr = os.Stderr
	out, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = cmd.Process.Signal(os.Interrupt); _ = cmd.Wait() }()
	var location string
	scan := bufio.NewScanner(out)
	for scan.Scan() {
		if loc, ok := strings.CutPrefix(scan.Text(), "TACET_RENDERER_URL="); ok {
			location = loc
			break
		}
	}
	if location == "" {
		t.Fatal("no description URL from renderer")
	}
	base, err := upnp.ConnectByURL(ctx, location)
	if err != nil {
		t.Fatal(err)
	}
	lp, ok := linkplay.Wrap(base).(*linkplay.Renderer)
	if !ok || !lp.Info().SupportsQueue || !lp.Info().SupportsSetNext {
		t.Fatal("Tacet did not recognize native queue + next URI")
	}
	pcm := make([]byte, 44+64000)
	copy(pcm, "RIFF")
	binary.LittleEndian.PutUint32(pcm[4:], uint32(len(pcm)-8))
	copy(pcm[8:], "WAVEfmt ")
	binary.LittleEndian.PutUint32(pcm[16:], 16)
	binary.LittleEndian.PutUint16(pcm[20:], 1)
	binary.LittleEndian.PutUint16(pcm[22:], 1)
	binary.LittleEndian.PutUint32(pcm[24:], 8000)
	binary.LittleEndian.PutUint32(pcm[28:], 16000)
	binary.LittleEndian.PutUint16(pcm[32:], 2)
	binary.LittleEndian.PutUint16(pcm[34:], 16)
	copy(pcm[36:], "data")
	binary.LittleEndian.PutUint32(pcm[40:], 64000)
	media := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.ServeContent(w, r, "fixture.wav", time.Time{}, bytes.NewReader(pcm))
	}))
	defer media.Close()
	items := make([]upnp.MediaItem, 55)
	for i := range items {
		items[i] = upnp.MediaItem{ID: fmt.Sprint(i), URL: fmt.Sprintf("%s/fixture?id=%d&v=2", media.URL, i), Title: "A < B & C", MIME: "audio/wav", Duration: 4 * time.Second, Seekable: true}
	}
	check := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	expect := func(state string, index int, uri string) {
		t.Helper()
		deadline := time.Now().Add(5 * time.Second)
		for {
			st, err := lp.Status(ctx)
			check(err)
			if st.State == state && st.HasQueueIndex && st.QueueIndex == index && st.TrackURI == uri {
				return
			}
			if time.Now().After(deadline) {
				t.Fatalf("want %s [%d] %s, got %+v", state, index, uri, st)
			}
			time.Sleep(20 * time.Millisecond)
		}
	}
	check(lp.ReplaceQueue(ctx, items, 50))
	expect("PLAYING", 50, items[50].URL)
	check(lp.Pause(ctx))
	check(lp.MoveQueueItem(ctx, 50, 0))
	expect("PAUSED_PLAYBACK", 0, items[50].URL)
	check(lp.Next(ctx))
	expect("PAUSED_PLAYBACK", 1, items[0].URL)
	check(lp.Previous(ctx))
	expect("PAUSED_PLAYBACK", 0, items[50].URL)
	check(lp.RemoveQueueRange(ctx, 1, 54))
	expect("PAUSED_PLAYBACK", 0, items[50].URL)
	check(lp.AppendQueue(ctx, items[1:3]))
	check(lp.SetLoopMode(ctx, "all", false))
	check(lp.PlayIndex(ctx, 2))
	expect("PLAYING", 2, items[2].URL)
	check(lp.Seek(ctx, 3*time.Second))
	expect("PLAYING", 0, items[50].URL)
	check(lp.Stop(ctx))
	st, err := lp.Status(ctx)
	check(err)
	if st.State != "STOPPED" {
		t.Fatal(st)
	}
}

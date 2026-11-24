package renderer

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

// Drive playback notifications explicitly so queue boundaries, including repeat
// one on identical URLs, can be tested without timing assumptions.
type queueEngine struct {
	uri                  string
	paused, closed       bool
	active, next, serial int64
	entries              map[int64]string
	events               []playback.Notice
	err                  error
}

func (p *queueEngine) Enqueue(uri string) (int64, error) {
	p.serial++
	p.entries[p.serial] = uri
	if p.active == 0 {
		p.start(p.serial)
	} else {
		p.next = p.serial
	}
	return p.serial, nil
}
func (p *queueEngine) start(id int64) {
	p.active = id
	p.uri = p.entries[id]
	p.events = append(p.events, playback.Notice{Kind: playback.Ready, Entry: id})
}
func (p *queueEngine) eof() {
	p.events = append(p.events, playback.Notice{Kind: playback.TrackFinished, Entry: p.active})
	if p.next != 0 {
		p.start(p.next)
		p.next = 0
	}
}
func (p *queueEngine) Discard(id int64) error {
	delete(p.entries, id)
	if p.next == id {
		p.next = 0
	}
	return nil
}
func (p *queueEngine) Pause(v bool) error      { p.paused = v; return nil }
func (*queueEngine) Stop() error               { return nil }
func (*queueEngine) Seek(time.Duration) error  { return nil }
func (p *queueEngine) Status() playback.Status { return playback.Status{Paused: p.paused} }
func (p *queueEngine) NextNotice(time.Duration) (playback.Notice, error) {
	if p.err != nil {
		return playback.Notice{Entry: p.active}, p.err
	}
	if len(p.events) == 0 {
		return playback.Notice{}, nil
	}
	e := p.events[0]
	p.events = p.events[1:]
	return e, nil
}
func (*queueEngine) SetMute(bool) error  { return nil }
func (*queueEngine) SetVolume(int) error { return nil }
func (p *queueEngine) Close()            { p.closed = true }

type queueRig struct {
	*testing.T
	tpt    *transport
	player *queueEngine
}

func setupQueue(t *testing.T) *queueRig {
	r := &queueRig{T: t}
	r.tpt = newTransport(func() (Engine, error) { r.player = &queueEngine{entries: make(map[int64]string)}; return r.player, nil }, deviceOptions{})
	return r
}
func (r *queueRig) call(s, action string, a fields) fields {
	r.Helper()
	if a == nil {
		a = fields{}
	}
	if s == av {
		a["InstanceID"] = "0"
	}
	out, f := r.tpt.call(s, action, a)
	if f != nil {
		r.Fatalf("%s fault: %+v", action, f)
	}
	return out
}
func ctxXML(name string, uris ...string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "<PlayList><ListName>%s</ListName><ListInfo><TrackNumber>%d</TrackNumber></ListInfo><Tracks>", name, len(uris))
	for i, u := range uris {
		fmt.Fprintf(&b, "<Track%d>%s%s</Track%d>", i+1, tag("URL", u), tag("Metadata", fmt.Sprintf("<didl>%d &amp; more</didl>", i)), i+1)
	}
	return b.String() + "</Tracks></PlayList>"
}
func (r *queueRig) create(urls ...string) {
	r.Helper()
	r.call(pq, "CreateQueue", fields{"QueueContext": ctxXML("tacet", urls...)})
}
func (r *queueRig) jump(n string) {
	r.Helper()
	r.call(pq, "PlayQueueWithIndex", fields{"Index": n, "QueueName": currentQueue})
	r.tpt.refresh()
}
func (r *queueRig) eof() {
	r.Helper()
	r.player.eof()
	r.tpt.refresh()
}
func (r *queueRig) check(index int, uri, state string) {
	r.Helper()
	i := r.call(pq, "GetQueueIndex", fields{"QueueName": currentQueue})["CurrentIndex"]
	pos := r.call(av, "GetPositionInfo", nil)
	st := r.call(av, "GetTransportInfo", nil)
	if i != strconv.Itoa(index) || pos["TrackURI"] != uri || st["CurrentTransportState"] != state {
		r.Fatalf("want [%d] %s %s; got [%s] %v %v", index, uri, state, i, pos, st)
	}
}

func TestQueueEditsKeepSelectedEntryAndEngine(t *testing.T) {
	const a = "https://media.test/a?x=1&y=2"
	const b = "https://media.test/b"
	r := setupQueue(t)
	r.create(a, b, a)
	r.jump("3")
	r.call(av, "Pause", nil)
	player := r.player
	r.call(pq, "AppendTracksInQueue", fields{"QueueContext": ctxXML(currentQueue, b)})
	r.call(pq, "MoveTracksInQueue", fields{"QueueName": currentQueue, "IndexList": "3", "ToIndex": "1"})
	r.check(1, a, "PAUSED_PLAYBACK")
	if meta := r.call(av, "GetPositionInfo", nil)["TrackMetaData"]; meta != "<didl>2 &amp; more</didl>" {
		t.Fatal(meta)
	}
	r.call(pq, "MoveTracksInQueue", fields{"QueueName": "tacet", "IndexList": "4", "ToIndex": "1"})
	r.check(2, a, "PAUSED_PLAYBACK")
	r.call(pq, "RemoveTracksInQueue", fields{"QueueName": currentQueue, "RangStart": "1", "RangEnd": "1", "Action": "0"})
	r.check(1, a, "PAUSED_PLAYBACK")
	if player.closed || r.player != player {
		t.Fatal("queue edit interrupted playback")
	}
	r.call(av, "Next", nil)
	r.check(2, a, "PAUSED_PLAYBACK")
	if !r.player.paused {
		t.Fatal("Next unpaused backend")
	}
	r.call(av, "Previous", nil)
	r.check(1, a, "PAUSED_PLAYBACK")
	r.call(pq, "RemoveTracksInQueue", fields{"QueueName": currentQueue, "RangStart": "1", "RangEnd": "2", "Action": "0"})
	r.check(1, b, "STOPPED")
	r.call(pq, "RemoveTracksInQueue", fields{"QueueName": currentQueue, "RangStart": "1", "RangEnd": "1", "Action": "0"})
	r.check(0, "", "NO_MEDIA_PRESENT")
	r.call(pq, "AppendTracksInQueue", fields{"QueueContext": ctxXML(currentQueue, b)})
	r.check(1, b, "STOPPED")
}

func TestQueueEndAndRepeatModes(t *testing.T) {
	const a = "http://media/a"
	const b = "http://media/b"
	r := setupQueue(t)
	r.create(a, b)
	r.jump("1")
	eof := func() { r.player.eof(); r.tpt.refresh(); r.tpt.refresh() }
	eof()
	r.check(2, b, "PLAYING")
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "1"})
	prior := r.player.active
	eof()
	r.check(2, b, "PLAYING")
	if prior == r.player.active || r.player.uri != b {
		t.Fatal("did not restart current item")
	}
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "0"})
	eof()
	r.check(1, a, "PLAYING")
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "4"})
	eof()
	eof()
	r.check(2, b, "STOPPED")
	r.jump("1")
	r.player.err = errors.New("source failed")
	r.tpt.refresh()
	r.check(1, a, "STOPPED")
	if st := r.call(av, "GetTransportInfo", nil); st["CurrentTransportStatus"] != "ERROR_OCCURRED" {
		t.Fatal(st)
	}
}

func TestInvalidQueueRequestsPreservePlayback(t *testing.T) {
	const a = "https://media/a"
	r := setupQueue(t)
	r.create(a)
	r.jump("1")
	bad := ctxXML("tacet", a, "file:///bad")
	for _, tc := range []struct {
		name string
		args fields
	}{{"CreateQueue", fields{"QueueContext": bad}}, {"AppendTracksInQueue", fields{"QueueContext": ctxXML("wrong", a)}}, {"AppendTracksInQueue", fields{"QueueContext": strings.Replace(ctxXML(currentQueue, a), "Track1>", "Track3>", -1)}}, {"CreateQueue", fields{"QueueContext": strings.Replace(ctxXML("tacet", a), "<TrackNumber>1", "<TrackNumber>20", 1)}}, {"PlayQueueWithIndex", fields{"QueueName": currentQueue, "Index": "0"}}, {"RemoveTracksInQueue", fields{"QueueName": "tacet", "RangStart": "1", "RangEnd": "2", "Action": "0"}}, {"MoveTracksInQueue", fields{"QueueName": currentQueue, "IndexList": "1,2", "ToIndex": "1"}}, {"SetQueueLoopMode", fields{"LoopMode": "6"}}} {
		if _, f := r.tpt.call(pq, tc.name, tc.args); f == nil {
			t.Fatalf("accepted %s", tc.name)
		}
		r.check(1, a, "PLAYING")
	}
	if _, f := r.tpt.call(av, "SetNextAVTransportURI", fields{"InstanceID": "0", "NextURI": a}); f == nil {
		t.Fatal("must not overwrite native queue")
	}
}

func TestGenericNextReplaceClearAndResetNativeQueue(t *testing.T) {
	const a = "http://media/a"
	const b = "http://media/b"
	r := setupQueue(t)
	r.create(a, b)
	r.jump("1")
	r.call(av, "SetAVTransportURI", fields{"CurrentURI": a})
	if _, f := r.tpt.call(pq, "GetQueueIndex", fields{"QueueName": currentQueue}); f == nil {
		t.Fatal("old queue still active")
	}
	for _, v := range []string{b, a, ""} {
		r.call(av, "SetNextAVTransportURI", fields{"NextURI": v})
		media := r.call(av, "GetMediaInfo", nil)
		if media["NextURI"] != v || media["CurrentURI"] != a || media["NrTracks"] != "1" {
			t.Fatal(media)
		}
	}
	if _, f := r.tpt.call(av, "Next", fields{"InstanceID": "0"}); f == nil {
		t.Fatal("Next succeeded after clearing next URI")
	}
}

func TestShuffleLoopModesAndQueueEdits(t *testing.T) {
	const a, b, c, d, e = "http://media/a", "http://media/b", "http://media/c", "http://media/d", "http://media/e"
	r := setupQueue(t)
	// Reverse the tail deterministically so shuffle order is predictable.
	r.tpt.q.shuffle = func(n int, swap func(i, j int)) {
		for i, j := 0, n-1; i < j; i, j = i+1, j-1 {
			swap(i, j)
		}
	}
	r.create(a, b, c, d)
	r.jump("2") // start on b (index 1); tail [a(0), c(2), d(3)] reversed -> order [1(b), 3(d), 2(c), 0(a)]

	// Mode 3: SHUFFLE_NOREPEAT plays every track once in shuffled order, then stops.
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "3"})
	if got := r.call(av, "GetTransportSettings", nil)["PlayMode"]; got != "SHUFFLE_NOREPEAT" {
		t.Fatalf("PlayMode = %q", got)
	}
	if idx := r.call(pq, "GetQueueIndex", fields{"QueueName": currentQueue}); idx["CurrentIndex"] != "2" || idx["PreloadingIndex"] != "4" {
		t.Fatalf("shuffled lookahead mismatch: %+v", idx)
	}
	// Advance b -> d -> c -> a; then Previous goes back to c; then Next returns to a; then EOF stops.
	r.eof()
	r.check(4, d, "PLAYING")
	r.eof()
	r.check(3, c, "PLAYING")
	r.eof()
	r.check(1, a, "PLAYING")
	r.call(av, "Previous", nil)
	r.tpt.refresh()
	r.check(3, c, "PLAYING")
	r.call(av, "Next", nil)
	r.tpt.refresh()
	r.check(1, a, "PLAYING")
	r.eof()
	r.check(1, a, "STOPPED")

	// Mode 2: SHUFFLE (repeat all) wraps after the last shuffled track.
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "2"})
	if got := r.call(av, "GetTransportSettings", nil)["PlayMode"]; got != "SHUFFLE" {
		t.Fatalf("PlayMode = %q", got)
	}
	r.jump("2") // restarts shuffle with b at order[0] -> [1(b), 3(d), 2(c), 0(a)]
	for _, want := range []struct {
		idx int
		uri string
	}{{4, d}, {3, c}, {1, a}, {2, b}} {
		r.eof()
		r.check(want.idx, want.uri, "PLAYING")
	}

	// Mode 5: SHUFFLE_REPEAT_ONE repeats current track at EOF, while manual Next uses shuffle order.
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "5"})
	if got := r.call(av, "GetTransportSettings", nil)["PlayMode"]; got != "SHUFFLE_REPEAT_ONE" {
		t.Fatalf("PlayMode = %q", got)
	}
	if got := r.call(pq, "GetQueueLoopMode", nil)["LoopMode"]; got != "5" {
		t.Fatalf("LoopMode = %q", got)
	}
	r.eof()
	r.check(2, b, "PLAYING")
	r.call(av, "Next", nil)
	r.tpt.refresh()
	r.check(4, d, "PLAYING")

	// Queue edits (Append, Move, Remove) while shuffled keep valid indices and preserve playback.
	r.call(pq, "SetQueueLoopMode", fields{"LoopMode": "3"})
	r.call(pq, "AppendTracksInQueue", fields{"QueueContext": ctxXML(currentQueue, e)})
	r.call(pq, "MoveTracksInQueue", fields{"QueueName": currentQueue, "IndexList": "4", "ToIndex": "1"}) // d moves 4 -> 1
	r.check(1, d, "PLAYING")
	r.call(pq, "RemoveTracksInQueue", fields{"QueueName": currentQueue, "RangStart": "2", "RangEnd": "2", "Action": "0"}) // remove a
	r.check(1, d, "PLAYING")
	// Remaining unplayed tracks after d are e and c (b already played earlier in the pass; a was removed).
	r.eof()
	r.check(4, e, "PLAYING")
	r.eof()
	r.check(3, c, "PLAYING")
	r.eof()
	r.check(3, c, "STOPPED")

	// SetPlayMode round-trips back to GetQueueLoopMode for all 6 LinkPlay modes.
	for mode, code := range loopCodes {
		r.call(av, "SetPlayMode", fields{"NewPlayMode": mode})
		if got := r.call(pq, "GetQueueLoopMode", nil)["LoopMode"]; got != code {
			t.Fatalf("SetPlayMode(%s) -> GetQueueLoopMode = %q, want %q", mode, got, code)
		}
	}
}

package renderer

import (
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type brokenEnqueue struct{ *queueEngine }

func (p brokenEnqueue) Enqueue(uri string) (int64, error) {
	id, err := p.queueEngine.Enqueue(uri)
	if id > 1 {
		return 0, errors.New("inserted but unknown ID")
	}
	return id, err
}

func TestUnknownLookaheadOwnershipRetiresSession(t *testing.T) {
	p := &queueEngine{entries: make(map[int64]string)}
	r := setupQueue(t)
	r.tpt.open = func() (Engine, error) { return brokenEnqueue{p}, nil }
	r.create("http://media/a", "http://media/b")
	_, f := r.tpt.call(pq, "PlayQueueWithIndex", fields{"Index": "1", "QueueName": currentQueue})
	if f == nil || !p.closed {
		t.Fatal("kept session with an unowned entry")
	}
}

type seekSpy struct {
	*queueEngine
	called bool
}

func (p *seekSpy) Seek(time.Duration) error { p.called = true; return nil }
func TestPendingHandoffDoesNotSeekUnreportedSuccessor(t *testing.T) {
	r := setupQueue(t)
	r.create("http://media/a", "http://media/b")
	r.jump("1")
	p := &seekSpy{queueEngine: r.player}
	r.tpt.p = p
	p.events = append(p.events, playback.Notice{Kind: playback.TrackFinished, Entry: p.active}, playback.Notice{Kind: playback.TrackStarted, Entry: p.next})
	r.tpt.refresh()
	actions := r.call(av, "GetCurrentTransportActions", nil)["Actions"]
	_, f := r.tpt.call(av, "Seek", fields{"InstanceID": "0", "Unit": "REL_TIME", "Target": "0:00:04"})
	if strings.Contains(actions, "Seek") || f == nil || f.code != 701 || p.called {
		t.Fatalf("actions %s; fault %v; forwarded %v", actions, f, p.called)
	}
}

func TestPendingHandoffHonorsPauseStopAndEdits(t *testing.T) {
	const a = "http://media.test/a"
	const b = "http://media.test/b"
	for _, action := range []string{"Pause", "Stop", "RemoveTracksInQueue"} {
		t.Run(action, func(t *testing.T) {
			r := setupQueue(t)
			r.create(a, b, a)
			r.jump("1")
			p := r.player
			// EOF has arrived but the old successor has not signalled playable yet.
			old := p.next
			p.events = append(p.events, playback.Notice{Entry: p.active, Kind: playback.TrackFinished})
			r.tpt.refresh()
			r.check(1, a, "TRANSITIONING")
			switch action {
			case "Pause":
				r.call(av, action, nil)
				p.start(old)
				r.tpt.refresh()
				r.check(2, b, "PAUSED_PLAYBACK")
				if !p.paused {
					t.Fatal("handoff unpaused backend")
				}
			case "Stop":
				r.call(av, action, nil)
				p.start(old)
				r.tpt.refresh()
				r.check(1, a, "STOPPED")
				if !p.closed {
					t.Fatal("stop retained queued audio")
				}
			default:
				r.call(pq, action, fields{"QueueName": currentQueue, "RangStart": "2", "RangEnd": "2", "Action": "0"})
				p.events = append(p.events, playback.Notice{Entry: old, Kind: playback.Ready}, playback.Notice{Entry: old, Kind: playback.Stopped})
				r.tpt.refresh()
				r.check(1, a, "TRANSITIONING")
				if p.entries[p.next] != a {
					t.Fatal("did not stage replacement")
				}
				p.start(p.next)
				r.tpt.refresh()
				r.check(2, a, "PLAYING")
				if meta := r.call(av, "GetPositionInfo", nil)["TrackMetaData"]; meta != "<didl>2 &amp; more</didl>" {
					t.Fatal(meta)
				}
			}
		})
	}
}

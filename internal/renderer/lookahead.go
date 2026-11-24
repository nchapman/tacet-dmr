package renderer

import (
	"errors"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type stagedEntry struct {
	id  int64 // zero with nonempty uri means staging failed; do not retry on polls
	uri string
}

func (t *transport) fail() { t.release(); t.status = "ERROR_OCCURRED" }

// Re-evaluate the successor after every successful control operation. Edits that
// only move the current entry or change the next entry's metadata need no audio
// command. Queue indices and metadata always come from the authoritative list.
func (t *transport) syncNext() error {
	want := t.q.nextTrack().uri
	if want == t.next.uri {
		return nil
	}
	if t.next.id != 0 {
		if err := t.p.Discard(t.next.id); err != nil {
			return err
		}
	}
	t.next = stagedEntry{uri: want}
	if want != "" {
		id, err := t.p.Enqueue(want)
		if err != nil && !errors.Is(err, playback.ErrNotQueued) {
			return err
		}
		t.next.id = id
	}
	if t.ended && t.next.id == 0 {
		t.finish()
	}
	return nil
}

func (t *transport) finish() {
	t.pos = t.duration
	if t.next.uri != "" {
		t.logf("error: next track %s failed to open", sanitizeURI(t.next.uri))
		t.fail()
	} else {
		t.logf("queue finished")
		t.release()
	}
}

func (t *transport) refresh() {
	t.syncVolume(false)
	// Bound work under the service lock even for pathological, tiny repeat loops.
	for n := 0; t.p != nil && n < 128; n++ {
		ev, err := t.p.NextNotice(0)
		if ev.Kind == playback.NoEvent && err == nil {
			if !t.ended {
				s := t.p.Status()
				t.pos = s.Position
				if s.Duration > 0 {
					t.duration = s.Duration
				}
			}
			return
		}
		if ev.Entry != 0 && ev.Entry != t.active && ev.Entry != t.next.id {
			continue
		} // discarded lookahead or consumed entry
		if err != nil {
			if ev.Entry != 0 && ev.Entry == t.next.id && !t.ended {
				t.logf("error: next track %s failed: %v", sanitizeURI(t.next.uri), err)
				if t.p.Discard(t.next.id) != nil {
					t.fail()
					return
				}
				t.next.id = 0
				continue
			}
			t.logf("error: playback failed: %v", err)
			t.fail()
			return
		}
		switch ev.Kind {
		case playback.OutputChanged:
			t.logOutput()
		case playback.Ready:
			// Opening a URL (and even FILE_LOADED) is not proof that it plays.
			// A failed successor must not become the reported current item.
			if ev.Entry == t.next.id && ev.Entry != 0 {
				i, ok := t.q.step(1, true)
				if !ok || t.p.Discard(t.active) != nil {
					t.fail()
					return
				}
				t.active = ev.Entry
				t.next = stagedEntry{}
				t.q.index = i
				t.pos = 0
				t.duration = 0
				t.ended = false
				if err := t.syncNext(); err != nil {
					t.logf("error: stage next track: %v", err)
					t.fail()
					return
				}
			}
			if t.active != 0 && t.loggedEntry != t.active {
				t.loggedEntry = t.active
				num, nr := t.q.trackNumbers()
				t.logf("play: track %d/%d %s", num, nr, sanitizeURI(t.q.current().uri))
			}
			t.logOutput()
			if t.state != "PAUSED_PLAYBACK" {
				t.state = "PLAYING"
			}
		case playback.TrackFinished:
			if ev.Entry != t.active {
				t.fail()
				return
			}
			t.ended = true
			if ev.Duration > 0 {
				t.duration = ev.Duration
			}
			t.pos = t.duration
			if t.next.id == 0 {
				t.finish()
				return
			}
			if t.state != "PAUSED_PLAYBACK" {
				t.state = "TRANSITIONING"
			}
		case playback.Stopped:
			t.logf("playback stopped")
			t.release()
			return
		}
	}
}

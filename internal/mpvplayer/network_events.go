package mpvplayer

import (
	"fmt"
	"time"

	"github.com/nchapman/tacet-dmr/internal/playback"
)

type result struct {
	notice playback.Notice
	err    error
}
type networkEvents struct {
	updates    chan result
	stop, done chan struct{}
}

// An unclocked output (or a late controller poll) can pass EOF before
// Status is sampled. Service the unload hook independently of SOAP/monitor
// polling, so we can retain final duration without stalling a join for a poll.
func (p *Player) startNetworkEvents() error {
	if err := p.m.HookAdd(0, "on_unload", 0); err != nil {
		return fmt.Errorf("register unload hook: %w", err)
	}
	q := &networkEvents{updates: make(chan result, 64), stop: make(chan struct{}), done: make(chan struct{})}
	p.events = q
	go func() {
		defer close(q.done)
		for {
			select {
			case <-q.stop:
				return
			default:
			}
			n, err := p.readNotice(-time.Second)
			if err == nil && (n.Kind == playback.Ignored || n.Kind == playback.NoEvent) {
				continue
			}
			select {
			case q.updates <- result{n, err}:
			case <-q.stop:
				return
			}
		}
	}()
	return nil
}
func (q *networkEvents) next(timeout time.Duration) (playback.Notice, error) {
	select {
	case r := <-q.updates:
		return r.notice, r.err
	default:
	}
	if timeout != 0 {
		var deadline <-chan time.Time
		if timeout > 0 {
			timer := time.NewTimer(timeout)
			defer timer.Stop()
			deadline = timer.C
		}
		select {
		case r := <-q.updates:
			return r.notice, r.err
		case <-deadline:
		case <-q.done:
		}
	}
	return playback.Notice{}, nil
}

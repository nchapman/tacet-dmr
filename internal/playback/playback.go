// Package playback defines the audio boundary. No protocol or player-library
// types cross it. A session has one owner, which serializes commands and events.
package playback

import (
	"errors"
	"time"
)

// ErrNotQueued means enqueue was rejected without changing the playlist. Other
// enqueue errors leave ownership uncertain and the session must be retired.
var ErrNotQueued = errors.New("entry was not queued")

// ErrUnsupported reports that a volume controller does not expose a given
// control (for example, a DAC with hardware volume but no hardware mute).
var ErrUnsupported = errors.New("control not supported")

// VolumeControl overrides the playback engine's default software volume and
// mute controls for a specific output target (for example, macOS CoreAudio
// hardware device controls during exclusive playback).
//
// Mute and SetMute may return ErrUnsupported, in which case the caller falls
// back to the playback engine's software mute while keeping hardware volume.
type VolumeControl interface {
	Volume() (int, error)
	SetVolume(int) error
	Mute() (bool, error)
	SetMute(bool) error
}

type Device struct{ ID, Description string }

type Status struct {
	Track                  int // one-based; zero when idle
	Paused                 bool
	Position, Duration     time.Duration
	Output                 string // active output driver, not proof of a hardware lock
	SourceRate, OutputRate int    // Hz as reported by the engine; zero when unknown
}

type Event int

const (
	NoEvent Event = iota
	TrackStarted
	Ready // playback restarted after initial buffering or a seek
	OutputChanged
	TrackFinished // natural EOF only
	Stopped
	Ignored // an engine notification was consumed (distinct from no event available)
)

// Notice identifies an engine entry, not a URL or a mutable queue position.
// Zero is reserved for session-wide events.
type Notice struct {
	Kind     Event
	Entry    int64
	Duration time.Duration // final known duration at natural EOF; zero if unknown
}

type Player interface {
	// Prepare replaces the queue and leaves playback paused. Opening media is
	// asynchronous: NextEvent reports media and output failures.
	Prepare(tracks []string) error
	Pause(bool) error
	Seek(time.Duration) error
	Stop() error
	Status() Status
	NextEvent(timeout time.Duration) (Event, error)
}

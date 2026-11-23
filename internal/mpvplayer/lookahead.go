package mpvplayer

import (
	"errors"
	"fmt"
	"strconv"

	"github.com/nchapman/tacet-dmr/internal/playback"

	"github.com/gen2brain/go-mpv"
)

// Only the renderer adds/removes entries, and network playlists cannot expand.
// Playing an entry changes its position as current but not its playlist index
// or ID. Resolve ID -> index at removal time; URLs need not be unique.
func (p *Player) entryIDs() ([]int64, error) {
	v, err := p.m.GetProperty("playlist", mpv.FormatNode)
	if err != nil {
		return nil, err
	}
	entries, ok := v.([]any)
	if !ok {
		return nil, errors.New("invalid mpv playlist")
	}
	ids := make([]int64, len(entries))
	for i, entry := range entries {
		m, _ := entry.(map[string]any)
		id, ok := m["id"].(int64)
		if !ok || id <= 0 {
			return nil, errors.New("missing mpv entry ID")
		}
		ids[i] = id
	}
	return ids, nil
}
func (p *Player) Enqueue(uri string) (int64, error) {
	// Ubuntu 26.04 ships mpv 0.41. It and 0.42 both accept append-play;
	// the newer append+play flag spelling is not understood by 0.41.
	v, err := p.m.CommandRet([]string{"loadfile", uri, "append-play"})
	if err != nil {
		return 0, fmt.Errorf("%w: %w", playback.ErrNotQueued, err)
	}
	result, _ := v.(map[string]any)
	id, ok := result["playlist_entry_id"].(int64)
	if !ok || id <= 0 {
		return 0, errors.New("loadfile did not return an entry ID")
	}
	return id, nil
}
func (p *Player) Discard(id int64) error {
	ids, err := p.entryIDs()
	if err != nil {
		return err
	}
	for i, v := range ids {
		if v == id {
			return p.m.Command([]string{"playlist-remove", strconv.Itoa(i)})
		}
	}
	return errors.New("mpv entry no longer in playlist")
}

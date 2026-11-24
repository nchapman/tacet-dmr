package renderer

import (
	"encoding/xml"
	"errors"
	"math/rand/v2"
	"slices"
	"strconv"
	"strings"
)

const (
	maxTracks        = 10000
	maxQueueBytes    = 32 << 20 // retained URIs and DIDL, independent of request size
	currentQueue     = "CurrentQueue"
	normal           = "NORMAL"
	repeatOne        = "REPEAT_ONE"
	repeatAll        = "REPEAT_ALL"
	shuffleRepeatAll = "SHUFFLE"
	shuffleNoRepeat  = "SHUFFLE_NOREPEAT"
	shuffleRepeatOne = "SHUFFLE_REPEAT_ONE"
)

// LinkPlay LoopMode <-> UPnP AVTransport CurrentPlayMode mapping:
// 0: repeat all, 1: repeat one, 2: shuffle + repeat all,
// 3: shuffle (no repeat), 4: normal, 5: shuffle + repeat one.
var loopCodes = map[string]string{
	repeatAll:        "0",
	repeatOne:        "1",
	shuffleRepeatAll: "2",
	shuffleNoRepeat:  "3",
	normal:           "4",
	shuffleRepeatOne: "5",
}

type track struct{ uri, metadata string }
type playlist struct {
	name    string // empty for ordinary AVTransport URI + optional next URI
	items   []track
	index   int // zero-based internally; only the SOAP boundary uses one-based indices
	mode    string
	order   []int // permutation of [0..len(items)-1] when shuffle is active
	shuffle func(n int, swap func(i, j int))
}

func (q *playlist) isShuffle() bool {
	return q.mode == shuffleRepeatAll || q.mode == shuffleNoRepeat || q.mode == shuffleRepeatOne
}
func (q *playlist) isRepeatAll() bool {
	return q.mode == repeatAll || q.mode == shuffleRepeatAll
}
func (q *playlist) isRepeatOne() bool {
	return q.mode == repeatOne || q.mode == shuffleRepeatOne
}
func (q *playlist) doShuffle(n int, swap func(i, j int)) {
	if q.shuffle != nil {
		q.shuffle(n, swap)
		return
	}
	rand.Shuffle(n, swap)
}
func (q *playlist) rebuildOrder() {
	if !q.isShuffle() || len(q.items) == 0 {
		q.order = nil
		return
	}
	order := make([]int, 0, len(q.items))
	if q.index >= 0 && q.index < len(q.items) {
		order = append(order, q.index)
	}
	for i := range q.items {
		if i != q.index {
			order = append(order, i)
		}
	}
	if len(order) > 2 {
		q.doShuffle(len(order)-1, func(i, j int) { order[i+1], order[j+1] = order[j+1], order[i+1] })
	}
	q.order = order
}
func (q *playlist) setMode(mode string) {
	wasShuffle := q.isShuffle()
	q.mode = mode
	if !q.isShuffle() {
		q.order = nil
		return
	}
	if !wasShuffle || len(q.order) != len(q.items) {
		q.rebuildOrder()
	}
}
func (q *playlist) ensureOrder() {
	if q.isShuffle() && len(q.order) != len(q.items) {
		q.rebuildOrder()
	}
}

func (q *playlist) current() track {
	if len(q.items) == 0 {
		return track{}
	}
	return q.items[q.index]
}
func (q *playlist) matches(name string) bool {
	return q.name != "" && (name == currentQueue || name == q.name)
}
func (q *playlist) step(delta int, eof bool) (int, bool) {
	if len(q.items) == 0 {
		return 0, false
	}
	if eof && q.isRepeatOne() {
		return q.index, true
	}
	if !q.isShuffle() {
		i := q.index + delta
		if i >= 0 && i < len(q.items) {
			return i, true
		}
		if q.isRepeatAll() {
			return (i + len(q.items)) % len(q.items), true
		}
		return 0, false
	}
	q.ensureOrder()
	pos := slices.Index(q.order, q.index)
	if pos < 0 {
		return 0, false
	}
	next := pos + delta
	if next >= 0 && next < len(q.order) {
		return q.order[next], true
	}
	if q.isRepeatAll() {
		return q.order[(next+len(q.order))%len(q.order)], true
	}
	return 0, false
}
func (q *playlist) appendItems(items []track) {
	oldLen := len(q.items)
	q.items = append(q.items, items...)
	if !q.isShuffle() {
		return
	}
	if oldLen == 0 || len(q.order) != oldLen {
		q.rebuildOrder()
		return
	}
	for i := oldLen; i < len(q.items); i++ {
		q.order = append(q.order, i)
	}
	pos := slices.Index(q.order, q.index)
	if pos >= 0 && pos+2 < len(q.order) {
		tail := q.order[pos+1:]
		q.doShuffle(len(tail), func(i, j int) { tail[i], tail[j] = tail[j], tail[i] })
	}
}
func (q *playlist) remove(lo, hi int) (removedCurrent bool) { // inclusive
	removedCurrent = lo <= q.index && q.index <= hi
	q.items = slices.Delete(q.items, lo, hi+1)
	switch {
	case removedCurrent:
		q.index = max(0, min(lo, len(q.items)-1))
	case hi < q.index:
		q.index -= hi - lo + 1
	}
	if q.isShuffle() {
		if len(q.items) == 0 {
			q.order = nil
		} else {
			n := 0
			span := hi - lo + 1
			for _, idx := range q.order {
				if idx >= lo && idx <= hi {
					continue
				}
				if idx > hi {
					idx -= span
				}
				q.order[n] = idx
				n++
			}
			q.order = q.order[:n]
		}
	}
	return removedCurrent
}
func (q *playlist) move(from, to int) {
	item := q.items[from]
	switch {
	case from < to:
		copy(q.items[from:to], q.items[from+1:to+1])
	case from > to:
		copy(q.items[to+1:from+1], q.items[to:from])
	}
	q.items[to] = item
	remap := func(idx int) int {
		switch {
		case idx == from:
			return to
		case from < idx && idx <= to:
			return idx - 1
		case to <= idx && idx < from:
			return idx + 1
		default:
			return idx
		}
	}
	q.index = remap(q.index)
	if q.isShuffle() {
		for i, idx := range q.order {
			q.order[i] = remap(idx)
		}
	}
}
func parseIndex(raw string, n int) (int, bool) {
	v, err := strconv.Atoi(raw)
	if err != nil || v < 1 || v > n {
		return 0, false
	}
	return v - 1, true
}
func (q *playlist) trackNumbers() (current, total int) {
	if len(q.items) == 0 {
		return 0, 0
	}
	if q.name == "" {
		return 1, 1
	}
	return q.index + 1, len(q.items)
}
func (q *playlist) nextTrack() track {
	if i, ok := q.step(1, true); ok {
		return q.items[i]
	}
	return track{}
}
func trackBytes(items []track) int {
	n := 0
	for _, v := range items {
		n += len(v.uri) + len(v.metadata)
	}
	return n
}

// Decode incrementally: a small XML document can still contain a large number
// of empty elements. Reject it before allocating an entry for each element.
type wireTracks []track

func (w *wireTracks) UnmarshalXML(d *xml.Decoder, start xml.StartElement) error {
	for {
		tok, err := d.Token()
		if err != nil {
			return err
		}
		switch v := tok.(type) {
		case xml.StartElement:
			if len(*w) >= maxTracks || v.Name.Local != "Track"+strconv.Itoa(len(*w)+1) {
				return errors.New("invalid track number")
			}
			var item struct {
				URL      string `xml:"URL"`
				Metadata string `xml:"Metadata"`
			}
			if err := d.DecodeElement(&item, &v); err != nil {
				return err
			}
			if !validURI(item.URL) {
				return errors.New("invalid track URL")
			}
			*w = append(*w, track{item.URL, item.Metadata})
		case xml.EndElement:
			if v.Name == start.Name {
				return nil
			}
		}
	}
}

// QueueContext is XML escaped inside SOAP; Metadata is escaped DIDL inside
// QueueContext. Decode each layer once and keep DIDL opaque for round trips.
func parseContext(raw string) (string, []track, *fault) {
	// Track1, Track2, ... are element names rather than a repeated <Track> tag.
	var body struct {
		XMLName xml.Name   `xml:"PlayList"`
		Name    string     `xml:"ListName"`
		Count   int        `xml:"ListInfo>TrackNumber"`
		Tracks  wireTracks `xml:"Tracks"`
	}
	if xml.Unmarshal([]byte(raw), &body) != nil || strings.TrimSpace(body.Name) == "" || len(body.Name) > 256 || body.Count == 0 || body.Count != len(body.Tracks) || body.Count > maxTracks {
		return "", nil, invalid
	}
	return body.Name, body.Tracks, nil
}

func (t *transport) queueAction(name string, a fields) (fields, *fault) {
	q := &t.q
	switch name {
	case "BrowseQueue":
		return q.browse(a["QueueName"])
	case "GetQueueLoopMode":
		return fields{"LoopMode": loopCodes[q.mode]}, nil
	case "CreateQueue", "AppendTracksInQueue":
		list, items, f := parseContext(a["QueueContext"])
		if f != nil {
			return nil, f
		}
		if name == "CreateQueue" {
			if list == totalQueue {
				return nil, invalid
			} // reserved inventory view
			t.q = playlist{name: list, items: items, mode: normal, shuffle: q.shuffle}
			t.resetTrack()
			return nil, nil
		}
		if !q.matches(list) {
			return nil, invalid
		}
		if len(q.items)+len(items) > maxTracks || trackBytes(q.items)+trackBytes(items) > maxQueueBytes {
			return nil, &fault{601, "Queue capacity exceeded"}
		}
		empty := len(q.items) == 0
		q.appendItems(items)
		if empty {
			t.resetTrack()
		}
		return nil, nil
	case "SetQueueLoopMode":
		if q.name == "" {
			return nil, invalid
		}
		for mode, code := range loopCodes {
			if a["LoopMode"] == code {
				q.setMode(mode)
				return nil, nil
			}
		}
		return nil, invalid
	}
	if !q.matches(a["QueueName"]) {
		return nil, invalid
	}
	index := func(key string) (int, bool) { return parseIndex(a[key], len(q.items)) }
	switch name {
	case "GetQueueIndex":
		n := 0
		if len(q.items) > 0 {
			n = q.index + 1
		}
		preloading := 0
		if i, ok := q.step(1, true); ok && t.next.id != 0 {
			preloading = i + 1
		}
		// Our queue occupies one page.
		page := "0"
		if n > 0 {
			page = "1"
		}
		return fields{"CurrentIndex": strconv.Itoa(n), "PreloadingIndex": strconv.Itoa(preloading), "CurrentPage": page, "TrackNums": strconv.Itoa(len(q.items))}, nil
	case "PlayQueueWithIndex":
		i, ok := index("Index")
		if !ok {
			return nil, invalid
		}
		if q.isShuffle() {
			q.index = i
			q.rebuildOrder()
		}
		return nil, t.selectTrack(i, "PLAYING")
	case "RemoveTracksInQueue":
		lo, ok := index("RangStart")
		hi, ok2 := index("RangEnd")
		if !ok || !ok2 || lo > hi || a["Action"] != "0" {
			return nil, invalid
		}
		if q.remove(lo, hi) {
			t.resetTrack()
		}
		return nil, nil
	case "MoveTracksInQueue":
		from, ok := index("IndexList")
		to, ok2 := index("ToIndex")
		if !ok || !ok2 {
			return nil, invalid
		}
		q.move(from, to)
		return nil, nil
	}
	return nil, &fault{401, "Invalid Action"}
}

package renderer

import (
	"io"
	"slices"
	"strconv"
	"strings"
)

const totalQueue = "TotalQueue"

// snapshotForBrowse validates QueueName and copies only the lightweight track
// slice header under Device.mu so large BrowseQueue XML serialization runs
// unlocked and streams directly to the HTTP response.
func (q *playlist) snapshotForBrowse(name string) (listName string, items []track, isTotal bool, f *fault) {
	if name == totalQueue {
		return q.name, q.items, true, nil
	}
	if !q.matches(name) {
		return "", nil, false, invalid
	}
	return q.name, slices.Clone(q.items), false, nil
}

func writeBrowse(w io.Writer, listName string, items []track, isTotal bool) error {
	if isTotal {
		count := "0"
		if listName != "" {
			count = "1"
		}
		if _, err := io.WriteString(w, "<PlayQueue>"); err != nil {
			return err
		}
		if err := writeTag(w, "TotalQueue", count); err != nil {
			return err
		}
		if _, err := io.WriteString(w, "<CurrentPlayList>"); err != nil {
			return err
		}
		if err := writeTag(w, "Name", listName); err != nil {
			return err
		}
		if _, err := io.WriteString(w, "</CurrentPlayList><PlayListInfo>"); err != nil {
			return err
		}
		if listName != "" {
			if _, err := io.WriteString(w, "<PlayList1>"); err != nil {
				return err
			}
			if err := writeTag(w, "Name", listName); err != nil {
				return err
			}
			if _, err := io.WriteString(w, "<ListInfo>"); err != nil {
				return err
			}
			if err := writeTag(w, "TrackNumber", strconv.Itoa(len(items))); err != nil {
				return err
			}
			if _, err := io.WriteString(w, "</ListInfo></PlayList1>"); err != nil {
				return err
			}
		}
		_, err := io.WriteString(w, "</PlayListInfo></PlayQueue>")
		return err
	}
	if _, err := io.WriteString(w, "<PlayList>"); err != nil {
		return err
	}
	if err := writeTag(w, "ListName", listName); err != nil {
		return err
	}
	if _, err := io.WriteString(w, "<ListInfo>"); err != nil {
		return err
	}
	if err := writeTag(w, "TrackNumber", strconv.Itoa(len(items))); err != nil {
		return err
	}
	if _, err := io.WriteString(w, "</ListInfo><Tracks>"); err != nil {
		return err
	}
	var numBuf [32]byte
	for i, item := range items {
		num := string(strconv.AppendInt(numBuf[:0], int64(i+1), 10))
		if _, err := io.WriteString(w, "<Track"); err != nil {
			return err
		}
		if _, err := io.WriteString(w, num); err != nil {
			return err
		}
		if _, err := io.WriteString(w, ">"); err != nil {
			return err
		}
		if err := writeTag(w, "URL", item.uri); err != nil {
			return err
		}
		if err := writeTag(w, "Metadata", item.metadata); err != nil {
			return err
		}
		if _, err := io.WriteString(w, "</Track"); err != nil {
			return err
		}
		if _, err := io.WriteString(w, num); err != nil {
			return err
		}
		if _, err := io.WriteString(w, ">"); err != nil {
			return err
		}
	}
	_, err := io.WriteString(w, "</Tracks></PlayList>")
	return err
}

// Browse exposes only the named Wiimu queue, not the generic AVTransport URI.
// The TotalQueue view is an inventory; never include track URLs/DIDL there.
func (q *playlist) browse(name string) (fields, *fault) {
	listName, items, isTotal, f := q.snapshotForBrowse(name)
	if f != nil {
		return nil, f
	}
	var b strings.Builder
	if !isTotal {
		b.Grow(trackBytes(items) + len(items)*64 + 128)
	}
	_ = writeBrowse(&b, listName, items, isTotal)
	return fields{"QueueContext": b.String()}, nil
}

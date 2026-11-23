package renderer

import (
	"fmt"
	"strconv"
	"strings"
)

const totalQueue = "TotalQueue"

// Browse exposes only the named Wiimu queue, not the generic AVTransport URI.
// The TotalQueue view is an inventory; never include track URLs/DIDL there.
func (q *playlist) browse(name string) (fields, *fault) {
	var b strings.Builder
	if name == totalQueue {
		b.WriteString("<PlayQueue>")
		count := "0"
		if q.name != "" {
			count = "1"
		}
		b.WriteString(tag("TotalQueue", count) + "<CurrentPlayList>" + tag("Name", q.name) + "</CurrentPlayList><PlayListInfo>")
		if q.name != "" {
			b.WriteString("<PlayList1>" + tag("Name", q.name) + "<ListInfo>" + tag("TrackNumber", strconv.Itoa(len(q.items))) + "</ListInfo></PlayList1>")
		}
		b.WriteString("</PlayListInfo></PlayQueue>")
	} else {
		if !q.matches(name) {
			return nil, invalid
		}
		b.WriteString("<PlayList>" + tag("ListName", q.name) + "<ListInfo>" + tag("TrackNumber", strconv.Itoa(len(q.items))) + "</ListInfo><Tracks>")
		for i, item := range q.items {
			fmt.Fprintf(&b, "<Track%d>", i+1)
			b.WriteString(tag("URL", item.uri))
			b.WriteString(tag("Metadata", item.metadata))
			fmt.Fprintf(&b, "</Track%d>", i+1)
		}
		b.WriteString("</Tracks></PlayList>")
	}
	return fields{"QueueContext": b.String()}, nil
}

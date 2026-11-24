package renderer

import (
	"bufio"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"net/netip"
	"strings"
	"sync"
	"time"
)

const soapNS = "http://schemas.xmlsoap.org/soap/envelope/"

type Device struct {
	host   string
	peers  netip.Prefix
	mux    *http.ServeMux
	mu     sync.Mutex // owns the transport, its libmpv handle, and subscription registry
	t      *transport
	last   eventState
	subs   map[string]*subscription
	client *http.Client
	wg     sync.WaitGroup
	done   chan struct{}
	wake   chan struct{}
	closed bool
}

func New(name, udn, host string, peers netip.Prefix, initialVolume int, open Factory) *Device {
	return newDevice(name, udn, host, peers, deviceOptions{initialVolume: &initialVolume}, open)
}

func newDevice(name, udn, host string, peers netip.Prefix, opts deviceOptions, open Factory) *Device {
	t := newTransport(open, opts)
	d := &Device{host: host, peers: peers, mux: http.NewServeMux(), t: t, last: t.eventState(), subs: make(map[string]*subscription), done: make(chan struct{}), wake: make(chan struct{}, 1), client: &http.Client{Timeout: 2 * time.Second, Transport: &http.Transport{DisableKeepAlives: true}, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}}
	document := func(path, body string) {
		d.mux.HandleFunc("GET "+path, func(w http.ResponseWriter, _ *http.Request) {
			w.Header().Set("Content-Type", `text/xml; charset="utf-8"`)
			io.WriteString(w, body)
		})
	}
	document("/description.xml", deviceXML(name, udn))
	for _, s := range services {
		document("/"+s+"/scpd.xml", scpdXML(s))
		d.mux.HandleFunc("POST /"+s+"/control", func(w http.ResponseWriter, r *http.Request) { d.control(w, r, s) })
		if s == pq {
			continue
		} // Queue index is polled; only AVTransport and RenderingControl emit LastChange.
		for _, m := range []string{"SUBSCRIBE", "UNSUBSCRIBE"} {
			d.mux.HandleFunc(m+" /"+s+"/events", func(w http.ResponseWriter, r *http.Request) {
				d.subscription(w, r, s)
			})
		}
	}
	d.wg.Add(1)
	go d.monitor()
	return d
}
func (d *Device) nudge() {
	select {
	case d.wake <- struct{}{}:
	default:
	}
}
func (d *Device) nextWakeLocked(now time.Time, hasRC bool) time.Duration {
	if d.t.p != nil {
		return 100 * time.Millisecond
	}
	var wait time.Duration
	schedule := func(until time.Time, floor time.Duration) {
		delta := until.Sub(now)
		if delta < floor {
			delta = floor
		}
		if wait == 0 || delta < wait {
			wait = delta
		}
	}
	for _, s := range d.subs {
		schedule(s.expires, 10*time.Millisecond)
	}
	if d.t.volOffline || (d.t.vol == nil && d.t.probeVol != nil) {
		schedule(d.t.nextProbe, 50*time.Millisecond)
	} else if hasRC && d.t.vol != nil {
		schedule(d.t.nextSync, 50*time.Millisecond)
	}
	return wait
}
func (d *Device) monitor() {
	defer d.wg.Done()
	var timer *time.Timer
	stopTimer := func() {
		if timer != nil {
			timer.Stop()
		}
	}
	defer stopTimer()
	for {
		d.mu.Lock()
		if d.closed {
			d.mu.Unlock()
			return
		}
		now := time.Now()
		hasRC := d.hasRCSubs()
		d.t.refreshWith(hasRC)
		d.publish()
		for _, s := range d.subs {
			if !now.Before(s.expires) {
				d.remove(s)
			}
		}
		wait := d.nextWakeLocked(now, d.hasRCSubs())
		d.mu.Unlock()

		if wait == 0 {
			select {
			case <-d.done:
				return
			case <-d.wake:
			}
			continue
		}
		if timer == nil {
			timer = time.NewTimer(wait)
		} else {
			timer.Reset(wait)
		}
		select {
		case <-d.done:
			return
		case <-d.wake:
			if !timer.Stop() {
				select {
				case <-timer.C:
				default:
				}
			}
		case <-timer.C:
		}
	}
}
func (d *Device) Close() {
	d.mu.Lock()
	if !d.closed {
		d.closed = true
		close(d.done)
		for _, s := range d.subs {
			d.remove(s)
		}
		d.t.release()
	}
	d.mu.Unlock()
	d.wg.Wait()
}
func requestPeer(r *http.Request) netip.Addr {
	ap, _ := netip.ParseAddrPort(r.RemoteAddr)
	return ap.Addr().Unmap() // invalid on parse error; never matches a prefix
}
func validHost(got, want string) bool {
	if got == want {
		return true
	}
	if bare, ok := strings.CutSuffix(want, ":80"); ok && got == bare {
		return true
	}
	return false
}

func (d *Device) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if !d.peers.Contains(requestPeer(r)) || !validHost(r.Host, d.host) || r.Header.Get("Origin") != "" {
		http.Error(w, "not an allowed controller", http.StatusForbidden)
		return
	}
	d.mux.ServeHTTP(w, r)
}

type wireAction struct {
	XMLName xml.Name
	Args    []struct {
		XMLName xml.Name
		Value   string     `xml:",chardata"`
		Nested  []struct{} `xml:",any"`
	} `xml:",any"`
}
type envelope struct {
	XMLName xml.Name `xml:"http://schemas.xmlsoap.org/soap/envelope/ Envelope"`
	Bodies  []struct {
		Actions []wireAction `xml:",any"`
	} `xml:"http://schemas.xmlsoap.org/soap/envelope/ Body"`
}

func soap(w http.ResponseWriter, code int, body string) {
	w.Header().Set("Content-Type", `text/xml; charset="utf-8"`)
	w.WriteHeader(code)
	fmt.Fprintf(w, `<?xml version="1.0"?><s:Envelope xmlns:s="%s" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>%s</s:Body></s:Envelope>`, soapNS, body)
}
func soapFault(w http.ResponseWriter, f *fault) {
	soap(w, 500, fmt.Sprintf(`<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>%d</errorCode><errorDescription>%s</errorDescription></UPnPError></detail></s:Fault>`, f.code, escape(f.message)))
}
func (d *Device) control(w http.ResponseWriter, r *http.Request, s string) {
	limit := int64(256 << 10)
	if s == pq {
		limit = 4 << 20
	} // Nested DIDL/XML is escaped twice in a batch.
	data, err := io.ReadAll(http.MaxBytesReader(w, r.Body, limit))
	if err != nil {
		http.Error(w, "invalid or excessive SOAP body", 413)
		return
	}
	var env envelope
	if xml.Unmarshal(data, &env) != nil || len(env.Bodies) != 1 || len(env.Bodies[0].Actions) != 1 {
		soapFault(w, invalid)
		return
	}
	req := env.Bodies[0].Actions[0]
	ns, name := serviceType(s), req.XMLName.Local
	if req.XMLName.Space != ns || strings.Trim(r.Header.Get("SOAPAction"), `"`) != ns+"#"+name {
		soapFault(w, &fault{401, "Invalid Action"})
		return
	}
	var spec *action
	for i := range actions[s] {
		if actions[s][i].name == name {
			spec = &actions[s][i]
			break
		}
	}
	if spec == nil {
		soapFault(w, &fault{401, "Invalid Action"})
		return
	}
	args := fields{}
	for _, v := range req.Args {
		if _, dup := args[v.XMLName.Local]; dup || len(v.Nested) != 0 {
			soapFault(w, invalid)
			return
		}
		args[v.XMLName.Local] = v.Value
	}
	in := strings.Fields(spec.in)
	if len(in) != len(args) {
		soapFault(w, invalid)
		return
	}
	for _, f := range in {
		key, _, _ := strings.Cut(f, ":")
		if _, ok := args[key]; !ok {
			soapFault(w, invalid)
			return
		}
	}
	d.mu.Lock()
	if d.closed {
		d.mu.Unlock()
		w.WriteHeader(503)
		return
	}
	d.t.refreshWith(d.hasRCSubs())
	if s == pq && name == "BrowseQueue" {
		// Snapshot the queue slice under lock and stream XML outside d.mu to
		// avoid multi-hundred-MiB intermediate copies on large queues.
		listName, items, isTotal, f := d.t.q.snapshotForBrowse(args["QueueName"])
		d.publish()
		d.mu.Unlock()
		if f != nil {
			soapFault(w, f)
			return
		}
		w.Header().Set("Content-Type", `text/xml; charset="utf-8"`)
		w.WriteHeader(200)
		bw := bufio.NewWriterSize(w, 32<<10)
		fmt.Fprintf(bw, `<?xml version="1.0"?><s:Envelope xmlns:s="%s" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:BrowseQueueResponse xmlns:u="%s"><QueueContext>`, soapNS, ns)
		// QueueContext is an XML string argument inside the SOAP envelope, so
		// escapeWriter XML-escapes the streamed <PlayList>/<PlayQueue> document
		// directly into bw without materializing an intermediate string.
		_ = writeBrowse(escapeWriter{bw}, listName, items, isTotal)
		_, _ = io.WriteString(bw, `</QueueContext></u:BrowseQueueResponse></s:Body></s:Envelope>`)
		_ = bw.Flush()
		return
	}
	values, f := d.t.call(s, name, args)
	d.publish()
	needWake := d.t.p != nil || d.t.volOffline || (d.t.vol == nil && d.t.probeVol != nil)
	d.mu.Unlock()
	if needWake {
		d.nudge()
	}
	if f != nil {
		soapFault(w, f)
		return
	}
	w.Header().Set("Content-Type", `text/xml; charset="utf-8"`)
	w.WriteHeader(200)
	bw := bufio.NewWriterSize(w, 4<<10)
	fmt.Fprintf(bw, `<?xml version="1.0"?><s:Envelope xmlns:s="%s" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:%sResponse xmlns:u="%s">`, soapNS, name, ns)
	for _, arg := range strings.Fields(spec.out) {
		key, _, _ := strings.Cut(arg, ":")
		_ = writeTag(bw, key, values[key])
	}
	fmt.Fprintf(bw, `</u:%sResponse></s:Body></s:Envelope>`, name)
	_ = bw.Flush()
}

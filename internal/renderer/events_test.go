package renderer

import (
	"encoding/xml"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"strings"
	"testing"
	"time"
)

func TestGENALifecycle(t *testing.T) {
	type message struct {
		header      http.Header
		method, xml string
	}
	notified := make(chan message, 8)
	cb := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var prop struct {
			Change string `xml:"property>LastChange"`
		}
		err := xml.NewDecoder(r.Body).Decode(&prop)
		if err != nil {
			t.Error(err)
		}
		notified <- message{r.Header.Clone(), r.Method, prop.Change}
	}))
	defer cb.Close()
	srv := httptest.NewUnstartedServer(nil)
	d := New("Desk", "uuid:test", srv.Listener.Addr().String(), netip.MustParsePrefix("127.0.0.0/24"), nil)
	defer d.Close()
	srv.Config.Handler = d
	srv.Start()
	defer srv.Close()
	request := func(method, path string, headers map[string]string, payload string) http.Header {
		t.Helper()
		r, err := http.NewRequest(method, srv.URL+path, strings.NewReader(payload))
		if err != nil {
			t.Fatal(err)
		}
		for k, v := range headers {
			r.Header.Set(k, v)
		}
		res, err := srv.Client().Do(r)
		if err != nil {
			t.Fatal(err)
		}
		res.Body.Close()
		return http.Header{"Status": {res.Status}, "Sid": {res.Header.Get("SID")}, "Timeout": {res.Header.Get("Timeout")}}
	}
	path := "/RenderingControl/events"
	r := request("SUBSCRIBE", path, map[string]string{"CALLBACK": "<" + cb.URL + ">", "NT": "upnp:event", "TIMEOUT": "Second-60"}, "")
	sid := r.Get("SID")
	if !strings.HasPrefix(sid, "uuid:") || r.Get("Timeout") != "Second-60" {
		t.Fatal(r)
	}
	expect := func(seq, text string) {
		t.Helper()
		select {
		case m := <-notified:
			if m.method != "NOTIFY" || m.header.Get("SID") != sid || m.header.Get("SEQ") != seq || m.header.Get("NTS") != "upnp:propchange" || !strings.Contains(m.xml, text) {
				t.Fatalf("notification: %+v", m)
			}
		case <-time.After(2 * time.Second):
			t.Fatal("missing NOTIFY")
		}
	}
	expect("0", `<Volume channel="Master" val="100"`)
	// Another allowed controller can read the description, but cannot target
	// the first controller's callback or claim its subscription by SID.
	for _, tc := range []struct {
		method, path string
		headers      map[string]string
		code         int
	}{
		{"GET", "/description.xml", nil, 200},
		{"SUBSCRIBE", path, map[string]string{"SID": sid}, 412},
		{"UNSUBSCRIBE", path, map[string]string{"SID": sid}, 412},
		{"SUBSCRIBE", path, map[string]string{"NT": "upnp:event", "CALLBACK": "<" + cb.URL + ">"}, 412},
	} {
		r := httptest.NewRequest(tc.method, srv.URL+tc.path, nil)
		r.RemoteAddr = "127.0.0.2:1234"
		for k, v := range tc.headers {
			r.Header.Set(k, v)
		}
		w := httptest.NewRecorder()
		d.ServeHTTP(w, r)
		if w.Code != tc.code {
			t.Fatalf("second peer %s %s: %d", tc.method, tc.path, w.Code)
		}
	}
	r = request("SUBSCRIBE", path, map[string]string{"SID": sid, "TIMEOUT": "Second-80"}, "")
	if r.Get("SID") != sid || r.Get("Timeout") != "Second-80" {
		t.Fatal(r)
	}
	ns := serviceType(rc)
	r = request("POST", "/RenderingControl/control", map[string]string{"SOAPAction": ns + "#SetVolume"}, `<s:Envelope xmlns:s="`+soapNS+`"><s:Body><u:SetVolume xmlns:u="`+ns+`"><InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>17</DesiredVolume></u:SetVolume></s:Body></s:Envelope>`)
	if r.Get("Status") != "200 OK" {
		t.Fatal(r)
	}
	expect("1", `<Volume channel="Master" val="17"`)
	if r = request("UNSUBSCRIBE", path, map[string]string{"SID": sid}, ""); r.Get("Status") != "200 OK" {
		t.Fatal(r)
	}
	if r = request("SUBSCRIBE", path, map[string]string{"SID": sid}, ""); !strings.HasPrefix(r.Get("Status"), "412") {
		t.Fatal(r)
	}
	if r = request("SUBSCRIBE", path, map[string]string{"CALLBACK": "<http://192.0.2.1/>", "NT": "upnp:event"}, ""); !strings.HasPrefix(r.Get("Status"), "412") {
		t.Fatal(r)
	}
}

func TestDeviceAcceptsOnlyConfiguredControllerAndHost(t *testing.T) {
	d := New("Desk", "uuid:test", "192.168.1.12:8088", netip.MustParsePrefix("192.168.1.42/32"), nil)
	defer d.Close()
	for _, tc := range []struct {
		remote, host, origin string
		code                 int
	}{{"192.168.1.42:3456", d.host, "", 200}, {"192.168.1.15:3456", d.host, "", 403}, {"192.168.1.42:3456", "wrong.example", "", 403}, {"192.168.1.42:3456", d.host, "http://other.example", 403}} {
		r := httptest.NewRequest("GET", "http://"+tc.host+"/description.xml", nil)
		r.RemoteAddr = tc.remote
		r.Header.Set("Origin", tc.origin)
		w := httptest.NewRecorder()
		d.ServeHTTP(w, r)
		if w.Code != tc.code {
			t.Errorf("%+v: %d", tc, w.Code)
		}
	}
}

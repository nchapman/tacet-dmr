//! Zero-copy streaming `BrowseQueue` XML serializer.

use std::fmt::{self, Write as _};

use crate::renderer::queue::{Playlist, TOTAL_QUEUE, Track};
use crate::renderer::schema::{EscapeWriter, Fault, PQ, service_type, write_tag};

pub struct BrowseSnapshot {
    pub list_name: String,
    pub total_count: usize,
    pub items: Vec<Track>,
    pub is_total: bool,
}

pub fn snapshot_for_browse(q: &Playlist, name: &str) -> Result<BrowseSnapshot, Fault> {
    if name == TOTAL_QUEUE {
        return Ok(BrowseSnapshot {
            list_name: q.name.clone(),
            total_count: q.items.len(),
            items: Vec::new(),
            is_total: true,
        });
    }
    if !q.matches(name) {
        return Err(Fault::INVALID_ARGS);
    }
    Ok(BrowseSnapshot {
        list_name: q.name.clone(),
        total_count: q.items.len(),
        items: q.items.clone(),
        is_total: false,
    })
}

pub fn write_browse<W: fmt::Write + ?Sized>(w: &mut W, snap: &BrowseSnapshot) -> fmt::Result {
    if snap.is_total {
        let count = if snap.list_name.is_empty() { "0" } else { "1" };
        w.write_str("<PlayQueue>")?;
        write_tag(w, "TotalQueue", count)?;
        w.write_str("<CurrentPlayList>")?;
        write_tag(w, "Name", &snap.list_name)?;
        w.write_str("</CurrentPlayList><PlayListInfo>")?;
        if !snap.list_name.is_empty() {
            w.write_str("<PlayList1>")?;
            write_tag(w, "Name", &snap.list_name)?;
            w.write_str("<ListInfo>")?;
            write!(w, "<TrackNumber>{}</TrackNumber>", snap.total_count)?;
            w.write_str("</ListInfo></PlayList1>")?;
        }
        return w.write_str("</PlayListInfo></PlayQueue>");
    }

    w.write_str("<PlayList>")?;
    write_tag(w, "ListName", &snap.list_name)?;
    w.write_str("<ListInfo>")?;
    write!(w, "<TrackNumber>{}</TrackNumber>", snap.items.len())?;
    w.write_str("</ListInfo><Tracks>")?;
    for (i, item) in snap.items.iter().enumerate() {
        let n = i + 1;
        write!(w, "<Track{n}>")?;
        write_tag(w, "URL", &item.uri)?;
        write_tag(w, "Metadata", &item.metadata)?;
        write!(w, "</Track{n}>")?;
    }
    w.write_str("</Tracks></PlayList>")
}

/// Format the complete SOAP `BrowseQueueResponse` with XML-escaped `QueueContext`.
pub fn format_browse_soap(snap: &BrowseSnapshot) -> String {
    let est = if snap.is_total {
        512
    } else {
        snap.items
            .iter()
            .map(|t| t.uri.len() + t.metadata.len() + 96)
            .sum::<usize>()
            + 512
    };
    let mut out = String::with_capacity(est);
    let _ = write!(
        out,
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:BrowseQueueResponse xmlns:u="{}"><QueueContext>"#,
        service_type(PQ)
    );
    let _ = write_browse(&mut EscapeWriter(&mut out), snap);
    out.push_str("</QueueContext></u:BrowseQueueResponse></s:Body></s:Envelope>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::queue::{Track, parse_context};
    use quick_xml::Reader;
    use quick_xml::events::Event;

    #[test]
    fn browse_soap_roundtrips_special_xml_chars_in_url_and_metadata() {
        let snap = BrowseSnapshot {
            list_name: "Mix & Match".into(),
            total_count: 2,
            is_total: false,
            items: vec![
                Track {
                    uri: "http://10.0.0.1:8080/track?id=1&fmt=flac".into(),
                    metadata: r#"<DIDL-Lite><item id="1"><dc:title>A &amp; B &lt;Live&gt;</dc:title></item></DIDL-Lite>"#.into(),
                },
                Track {
                    uri: "http://10.0.0.1:8080/track?id=2&fmt=alac".into(),
                    metadata: String::new(),
                },
            ],
        };

        let soap = format_browse_soap(&snap);
        // Extract <QueueContext> text from the outer SOAP envelope using a real XML parser.
        let mut reader = Reader::from_str(&soap);
        let mut in_qc = false;
        let mut qc = String::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Start(e) if e.local_name().as_ref() == b"QueueContext" => in_qc = true,
                Event::End(e) if e.local_name().as_ref() == b"QueueContext" => break,
                Event::Text(t) if in_qc => qc.push_str(&t.unescape().unwrap()),
                Event::Eof => break,
                _ => {}
            }
        }

        // Now parse the unescaped inner <PlayList> XML back through parse_context!
        let (list_name, tracks) = parse_context(&qc).unwrap();
        assert_eq!(list_name, "Mix & Match");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].uri, snap.items[0].uri);
        assert_eq!(tracks[0].metadata, snap.items[0].metadata);
        assert_eq!(tracks[1].uri, snap.items[1].uri);
    }
}

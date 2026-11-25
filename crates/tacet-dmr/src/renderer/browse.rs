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

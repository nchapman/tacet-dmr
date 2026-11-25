//! Seekable HTTP/HTTPS and local-file `MediaSource` for `symphonia`.
//!
//! Keeps an in-memory prefix cache (so format probing and container header
//! re-reads never trigger extra HTTP roundtrips) and opens a fresh
//! `Range: bytes=<pos>-` stream when seeking outside the buffered region.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use symphonia::core::io::MediaSource;

const PREFIX_CACHE_MAX: usize = 256 * 1024;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Opened media stream together with container hints (MIME type and extension).
pub struct OpenedSource {
    pub source: Box<dyn MediaSource>,
    pub content_type: Option<String>,
    pub extension: Option<String>,
}

/// Open an `http://`, `https://`, `file://`, or local filesystem path.
pub fn open_media(url: &str, cancel: Arc<AtomicBool>) -> io::Result<OpenedSource> {
    if let Some(rest) = url.strip_prefix("file://") {
        let file = File::open(rest)?;
        return Ok(OpenedSource {
            source: Box::new(file),
            content_type: None,
            extension: extension_from_url(rest),
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let http = HttpMediaSource::open(url, cancel)?;
        let content_type = http.content_type.clone();
        return Ok(OpenedSource {
            source: Box::new(http),
            content_type,
            extension: extension_from_url(url),
        });
    }
    let file = File::open(url)?;
    Ok(OpenedSource {
        source: Box::new(file),
        content_type: None,
        extension: extension_from_url(url),
    })
}

fn extension_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or(path);
    let (_, ext) = last.rsplit_once('.')?;
    let ext = ext.trim().to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 8 {
        None
    } else {
        Some(ext)
    }
}

struct HttpMediaSource {
    url: String,
    agent: ureq::Agent,
    cancel: Arc<AtomicBool>,
    reader: Option<Box<dyn Read + Send + Sync>>,
    stream_pos: u64,
    pos: u64,
    len: Option<u64>,
    seekable: bool,
    content_type: Option<String>,
    prefix: Vec<u8>,
}

impl HttpMediaSource {
    fn open(url: &str, cancel: Arc<AtomicBool>) -> io::Result<Self> {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
            .timeout_recv_response(Some(HTTP_READ_TIMEOUT))
            .timeout_per_read(Some(HTTP_READ_TIMEOUT))
            .build();
        let agent: ureq::Agent = config.into();

        let mut src = Self {
            url: url.to_string(),
            agent,
            cancel,
            reader: None,
            stream_pos: 0,
            pos: 0,
            len: None,
            seekable: false,
            content_type: None,
            prefix: Vec::new(),
        };
        src.open_at(0, true)?;
        Ok(src)
    }

    fn open_at(&mut self, offset: u64, initial: bool) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"));
        }
        let mut req = self.agent.get(&self.url);
        if offset > 0 {
            req = req.header("Range", format!("bytes={offset}-"));
        }
        let resp = req
            .call()
            .map_err(|e| io::Error::other(format!("http get {}: {e}", self.url)))?;
        let status = resp.status().as_u16();
        if offset > 0 && status != 206 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("server ignored Range request (status {status})"),
            ));
        }

        if initial {
            self.content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty());

            self.len = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            let accepts_bytes = resp
                .headers()
                .get("accept-ranges")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
            // Most HTTP servers (including Go net/http and Tacet) support Range
            // when Content-Length is known even if Accept-Ranges is omitted.
            self.seekable = accepts_bytes || self.len.is_some();
        }

        self.reader = Some(Box::new(resp.into_body().into_reader()));
        self.stream_pos = offset;
        Ok(())
    }

    fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"))
        } else {
            Ok(())
        }
    }
}

impl Read for HttpMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check_cancel()?;
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(len) = self.len
            && self.pos >= len
        {
            return Ok(0);
        }

        // Serve from prefix cache when reading inside cached header bytes.
        if (self.pos as usize) < self.prefix.len() {
            let start = self.pos as usize;
            let avail = self.prefix.len() - start;
            let n = avail.min(buf.len());
            buf[..n].copy_from_slice(&self.prefix[start..start + n]);
            self.pos += n as u64;
            return Ok(n);
        }

        // Realign underlying stream if needed.
        if self.reader.is_none() || self.stream_pos != self.pos {
            // If we only need to skip a small forward distance (<= 32 KiB), drain
            // the current connection instead of opening a new HTTP request.
            if let Some(reader) = self.reader.as_mut()
                && self.pos > self.stream_pos
                && self.pos - self.stream_pos <= 32 * 1024
            {
                let mut skip = (self.pos - self.stream_pos) as usize;
                let mut scratch = [0u8; 4096];
                while skip > 0 {
                    if self.cancel.load(Ordering::Relaxed) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "aborted"));
                    }
                    let want = skip.min(scratch.len());
                    let r = reader.read(&mut scratch[..want])?;
                    if r == 0 {
                        return Ok(0);
                    }
                    if self.stream_pos == self.prefix.len() as u64
                        && self.prefix.len() < PREFIX_CACHE_MAX
                    {
                        let take = r.min(PREFIX_CACHE_MAX - self.prefix.len());
                        self.prefix.extend_from_slice(&scratch[..take]);
                    }
                    self.stream_pos += r as u64;
                    skip -= r;
                }
            } else {
                self.open_at(self.pos, false)?;
            }
        }

        let r = self.reader.as_mut().unwrap().read(buf)?;
        if r > 0 {
            if self.stream_pos == self.prefix.len() as u64 && self.prefix.len() < PREFIX_CACHE_MAX {
                let take = r.min(PREFIX_CACHE_MAX - self.prefix.len());
                self.prefix.extend_from_slice(&buf[..take]);
            }
            self.stream_pos += r as u64;
            self.pos += r as u64;
        }
        Ok(r)
    }
}

impl Seek for HttpMediaSource {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        self.check_cancel()?;
        let target: i64 = match style {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => {
                let len = self.len.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "unknown content length")
                })?;
                len as i64 + d
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of stream",
            ));
        }
        let target = target as u64;
        if let Some(len) = self.len
            && target > len
        {
            self.pos = len;
            return Ok(self.pos);
        }
        self.pos = target;
        Ok(self.pos)
    }
}

impl MediaSource for HttpMediaSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    /// Minimal single-purpose HTTP/1.1 range server for deterministic source tests.
    fn spawn_range_server(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let body = body.clone();
                let hits = Arc::clone(&hits_bg);
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut range_start: Option<usize> = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("range: bytes=")
                            && let Some((s, _)) = rest.trim().split_once('-')
                        {
                            range_start = s.parse().ok();
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    let mut w = stream;
                    match range_start {
                        Some(start) if start < body.len() => {
                            let slice = &body[start..];
                            let hdr = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                slice.len(),
                                start,
                                body.len() - 1,
                                body.len()
                            );
                            let _ = w.write_all(hdr.as_bytes());
                            let _ = w.write_all(slice);
                        }
                        Some(_) => {
                            let _ = w.write_all(
                                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                        None => {
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = w.write_all(hdr.as_bytes());
                            let _ = w.write_all(&body);
                        }
                    }
                });
            }
        });
        (format!("http://{addr}/track.bin"), hits)
    }

    #[test]
    fn http_source_prefix_cache_range_seek_and_eof_clamp() {
        // 300 KiB payload (> 256 KiB prefix cache so far seeks exercise Range requests).
        let payload: Vec<u8> = (0..(300 * 1024)).map(|i| (i % 251) as u8).collect();
        let (url, hits) = spawn_range_server(payload.clone());
        let cancel = Arc::new(AtomicBool::new(false));

        let mut src = open_media(&url, Arc::clone(&cancel)).unwrap().source;
        assert_eq!(src.byte_len(), Some(payload.len() as u64));
        assert!(src.is_seekable());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Read first 16 bytes, seek back to 0: served from prefix cache with no new HTTP request.
        let mut buf = [0u8; 16];
        src.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &payload[..16]);
        src.seek(SeekFrom::Start(0)).unwrap();
        src.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &payload[..16]);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Seek beyond prefix cache -> Range request.
        let far = 280 * 1024;
        src.seek(SeekFrom::Start(far as u64)).unwrap();
        src.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &payload[far..far + 16]);
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // Seek past EOF clamps to len and next read returns Ok(0) without issuing a 416 Range request!
        let pos = src
            .seek(SeekFrom::Start((payload.len() + 999) as u64))
            .unwrap();
        assert_eq!(pos, payload.len() as u64);
        assert_eq!(src.read(&mut buf).unwrap(), 0);
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // Cancellation returns Interrupted cleanly on both seek and read.
        cancel.store(true, Ordering::Release);
        assert_eq!(
            src.seek(SeekFrom::Start(0)).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            src.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}

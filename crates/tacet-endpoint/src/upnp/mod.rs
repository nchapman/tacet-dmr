//! Minimal UPnP AV / DLNA `MediaRenderer:1` compatibility layer.
//!
//! Exposes `AVTransport:1`, `RenderingControl:1`, and `ConnectionManager:1`
//! as a separate UPnP device (`urn:schemas-upnp-org:device:MediaRenderer:1`)
//! alongside the primary OpenHome `Source:1` device, sharing the same
//! `tacet-audio` output engine and volume controller.

pub mod events;
pub mod schema;

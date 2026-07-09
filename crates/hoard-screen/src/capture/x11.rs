//! Real X11 window capture (`feature = "runtime"`).
//!
//! Enumerates top-levels via `_NET_CLIENT_LIST` and grabs each frame with
//! `GetImage` on the target window. On a composited X server (picom, the GNOME/
//! KDE X sessions, etc.) the window keeps its full off-screen contents, so this
//! captures even occluded windows; without a compositor only the visible part
//! is valid. Pixels come back as 32bpp BGRX on little-endian TrueColor servers
//! and are swizzled to RGBA.
//!
//! A faster path (XComposite `NameWindowPixmap` + XDamage to only re-read on
//! change, XShm to avoid the round-trip copy) slots in behind the same
//! [`Source`] without touching anything above.

use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, ImageFormat, Window,
};
use x11rb::rust_connection::RustConnection;

use super::{CaptureBackend, CaptureError, Result, WindowId, WindowInfo};
use crate::source::{Frame, Source};

fn backend_err<E: std::fmt::Display>(e: E) -> CaptureError {
    CaptureError::Backend(e.to_string())
}

pub struct X11Capture {
    conn: Arc<RustConnection>,
    root: Window,
}

impl X11Capture {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).map_err(backend_err)?;
        let root = conn.setup().roots[screen_num].root;
        Ok(Self { conn: Arc::new(conn), root })
    }

    fn atom(&self, name: &str) -> Result<u32> {
        Ok(self
            .conn
            .intern_atom(false, name.as_bytes())
            .map_err(backend_err)?
            .reply()
            .map_err(backend_err)?
            .atom)
    }

    fn window_title(&self, w: Window) -> String {
        // Prefer _NET_WM_NAME (UTF-8), fall back to WM_NAME.
        if let Ok(net) = self.atom("_NET_WM_NAME") {
            if let Ok(utf8) = self.atom("UTF8_STRING") {
                if let Ok(cookie) = self.conn.get_property(false, w, net, utf8, 0, 1024) {
                    if let Ok(reply) = cookie.reply() {
                        if !reply.value.is_empty() {
                            return String::from_utf8_lossy(&reply.value).into_owned();
                        }
                    }
                }
            }
        }
        if let Ok(cookie) =
            self.conn
                .get_property(false, w, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
        {
            if let Ok(reply) = cookie.reply() {
                return String::from_utf8_lossy(&reply.value).into_owned();
            }
        }
        String::new()
    }

    fn window_class(&self, w: Window) -> String {
        if let Ok(cookie) =
            self.conn
                .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        {
            if let Ok(reply) = cookie.reply() {
                // WM_CLASS is "instance\0class\0"; take the class part.
                let parts: Vec<&[u8]> = reply.value.split(|&b| b == 0).collect();
                if let Some(class) = parts.get(1).filter(|s| !s.is_empty()) {
                    return String::from_utf8_lossy(class).into_owned();
                }
                if let Some(inst) = parts.first().filter(|s| !s.is_empty()) {
                    return String::from_utf8_lossy(inst).into_owned();
                }
            }
        }
        String::new()
    }
}

impl CaptureBackend for X11Capture {
    fn name(&self) -> &'static str {
        "linux/x11-getimage"
    }

    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let net_client_list = self.atom("_NET_CLIENT_LIST")?;
        let reply = self
            .conn
            .get_property(false, self.root, net_client_list, AtomEnum::WINDOW, 0, 4096)
            .map_err(backend_err)?
            .reply()
            .map_err(backend_err)?;
        let windows = reply
            .value32()
            .ok_or_else(|| CaptureError::Backend("_NET_CLIENT_LIST not 32-bit".into()))?;

        let mut out = Vec::new();
        for w in windows {
            let title = self.window_title(w);
            let app = self.window_class(w);
            out.push(WindowInfo {
                id: format!("0x{w:x}"),
                title,
                app,
                protected: false,
            });
        }
        Ok(out)
    }

    fn capture(&self, id: &WindowId) -> Result<Box<dyn Source>> {
        let window = parse_window_id(id)?;
        // Validate it exists and grab initial size.
        let geo = self
            .conn
            .get_geometry(window)
            .map_err(backend_err)?
            .reply()
            .map_err(|_| CaptureError::NotFound(id.clone()))?;
        Ok(Box::new(X11WindowSource {
            conn: Arc::clone(&self.conn),
            window,
            id: id.clone(),
            last: None,
            _seed: (geo.width, geo.height),
        }))
    }
}

fn parse_window_id(id: &WindowId) -> Result<Window> {
    let s = id.trim();
    let parsed = if let Some(hex) = s.strip_prefix("0x") {
        u32::from_str_radix(hex, 16)
    } else {
        s.parse::<u32>()
    };
    parsed.map_err(|_| CaptureError::Backend(format!("bad window id: {id}")))
}

/// A live X11 window source. Re-reads geometry each frame so the panel tracks
/// the real window being resized.
struct X11WindowSource {
    conn: Arc<RustConnection>,
    window: Window,
    id: String,
    last: Option<Frame>,
    _seed: (u16, u16),
}

impl Source for X11WindowSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn acquire(&mut self) -> Option<Frame> {
        let geo = self.conn.get_geometry(self.window).ok()?.reply().ok()?;
        let (w, h) = (geo.width, geo.height);
        if w == 0 || h == 0 {
            return self.last.clone();
        }
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.window, 0, 0, w, h, !0u32)
            .ok()?
            .reply()
            .ok()?;

        let (wu, hu) = (w as usize, h as usize);
        let expected = wu * hu * 4;
        if img.data.len() < expected {
            // Not 32bpp (unusual visual); keep the previous frame.
            return self.last.clone();
        }
        let mut rgba = vec![0u8; expected];
        // ZPixmap 32bpp, little-endian TrueColor: bytes are [B, G, R, X].
        for (src, dst) in img.data.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 0xff;
        }
        let frame = Frame::new(w as u32, h as u32, rgba);
        self.last = Some(frame.clone());
        Some(frame)
    }
}

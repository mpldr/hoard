//! Sniper-scope / magnifier widget: a lens (circle or square) that shows the
//! screen region under itself magnified. Like [`crate::crosshair`] it is a
//! procedural [`Source`] riding the engine → CPU-compositor path, but it is
//! *live*: every tick it re-grabs the screen under the panel and emits a frame
//! captured at `panel_size / zoom`, which the compositor then stretches over
//! the panel box — the stretch IS the magnification, reusing the existing
//! bilinear scaler.
//!
//! Where the pixels come from is [`crate::capture::screen`]'s per-OS screen
//! grab. The overlay's own windows must be excluded from that grab or the lens
//! would recursively magnify itself — on Windows the runtime flips
//! `WDA_EXCLUDEFROMCAPTURE` on while a scope panel exists.
//!
//! The lens needs to know *where* the panel currently is, which a plain
//! [`Source`] never did: [`Source::set_viewport`] (a default-no-op hook) is
//! fed by [`Engine::tick`](crate::engine::Engine::tick) with the panel's rect
//! and target monitor right before each `acquire`.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::monitors::MonitorInfo;
use crate::scene::Rect;
use crate::source::{Frame, Source};

/// Lens outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScopeShape {
    /// Elliptical lens inscribed in the panel box (a circle when the box is
    /// square) — the classic sniper look.
    #[default]
    Circle,
    /// The whole panel box.
    Square,
}

/// Everything that defines a scope's look. All fields default so the desktop
/// can send just `{"kind":"scope"}`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopeSpec {
    #[serde(default)]
    pub shape: ScopeShape,
    /// Magnification: the captured region is `panel_size / zoom`. Clamped to
    /// 1..=8 at use; 1 shows the region unmagnified.
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    /// Dark rim around the lens edge so it reads as a scope.
    #[serde(default = "default_border")]
    pub border: bool,
}

impl Default for ScopeSpec {
    fn default() -> Self {
        Self {
            shape: ScopeShape::Circle,
            zoom: default_zoom(),
            border: default_border(),
        }
    }
}

fn default_zoom() -> f32 {
    2.0
}
fn default_border() -> bool {
    true
}

/// Screen-grab function: `(x, y, w, h)` in virtual-desktop pixels → RGBA
/// frame. Injectable so the mask/zoom geometry is unit-testable headless.
pub type Grabber = fn(i32, i32, u32, u32) -> Option<Frame>;

/// Live magnifier source. Re-grabs the screen under its viewport each acquire,
/// throttled to ~30 fps so an otherwise-idle overlay doesn't spin the CPU
/// compositor at the message-loop rate.
pub struct ScopeSource {
    id: String,
    spec: ScopeSpec,
    grab: Grabber,
    viewport: Option<(Rect, u32)>,
    /// Monitor origins, cached on first use (the overlay process is restarted
    /// on display changes anyway).
    mons: Option<Vec<MonitorInfo>>,
    last: Option<Instant>,
}

/// Minimum interval between grabs (~30 fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

impl ScopeSource {
    pub fn new(id: impl Into<String>, spec: &ScopeSpec) -> Self {
        Self::with_grabber(id, spec, crate::capture::screen::grab)
    }

    pub fn with_grabber(id: impl Into<String>, spec: &ScopeSpec, grab: Grabber) -> Self {
        Self {
            id: id.into(),
            spec: *spec,
            grab,
            viewport: None,
            mons: None,
            last: None,
        }
    }

    fn monitor_origin(&mut self, mon_id: u32) -> (i32, i32) {
        let mons = self.mons.get_or_insert_with(crate::monitors::list_monitors);
        mons.iter()
            .find(|m| m.id == mon_id)
            .map(|m| (m.x, m.y))
            .unwrap_or((0, 0))
    }
}

impl Source for ScopeSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_viewport(&mut self, rect: Rect, monitor: u32) {
        self.viewport = Some((rect, monitor));
    }

    fn acquire(&mut self) -> Option<Frame> {
        if self.last.is_some_and(|t| t.elapsed() < FRAME_INTERVAL) {
            return None;
        }
        let (rect, mon_id) = self.viewport?;
        self.last = Some(Instant::now());

        let r = rect.normalized();
        let zoom = self.spec.zoom.clamp(1.0, 8.0) as f64;
        let cap_w = ((r.w / zoom).round() as u32).clamp(8, 4096);
        let cap_h = ((r.h / zoom).round() as u32).clamp(8, 4096);
        let (ox, oy) = self.monitor_origin(mon_id);
        let cx = ox + (r.x + r.w / 2.0) as i32;
        let cy = oy + (r.y + r.h / 2.0) as i32;

        let mut frame = (self.grab)(
            cx - (cap_w / 2) as i32,
            cy - (cap_h / 2) as i32,
            cap_w,
            cap_h,
        )
        // No grab on this platform / it failed: a dim glass placeholder so the
        // lens still shows where it is instead of vanishing.
        .unwrap_or_else(|| Frame::solid(cap_w, cap_h, [20, 20, 24, 200]));

        apply_lens(&mut frame, &self.spec);
        Some(frame)
    }
}

/// Mask the frame to the lens shape and draw the rim, in place. Geometry is in
/// normalized coordinates so the mask stretches with the frame onto the panel
/// box (a circle lens on a square box, an ellipse on a wide one).
fn apply_lens(frame: &mut Frame, spec: &ScopeSpec) {
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 {
        return;
    }
    let buf = match std::sync::Arc::get_mut(&mut frame.rgba) {
        Some(b) => b,
        None => return, // freshly built frames are uniquely owned
    };
    // Edge widths in normalized units, sized off the frame so the rim stays a
    // consistent on-panel thickness (~2px of frame ≈ 2*zoom px on screen is too
    // fat; the frame is panel/zoom so 2px frame == 2px panel after stretch...
    // exactly what we want).
    let aa = 1.5 / w.min(h) as f32;
    let rim_w = 2.5 / w.min(h) as f32;
    let rim = spec.border;

    for y in 0..h {
        for x in 0..w {
            // Normalized offset from centre, -0.5..0.5 on each axis.
            let nx = (x as f32 + 0.5) / w as f32 - 0.5;
            let ny = (y as f32 + 0.5) / h as f32 - 0.5;
            // Signed distance to the lens edge (negative inside).
            let d = match spec.shape {
                ScopeShape::Circle => (nx * nx + ny * ny).sqrt() - 0.5,
                ScopeShape::Square => nx.abs().max(ny.abs()) - 0.5,
            };
            let i = ((y * w + x) * 4) as usize;
            // Outside → transparent (with AA); rim band → dark ring.
            let cover = ((-d) / aa).clamp(0.0, 1.0);
            if cover < 1.0 {
                let a = (buf[i + 3] as f32 * cover) as u8;
                if cover <= 0.0 {
                    buf[i] = 0;
                    buf[i + 1] = 0;
                    buf[i + 2] = 0;
                }
                buf[i + 3] = a;
            }
            if rim && d > -rim_w && cover > 0.0 {
                let t = (cover * 230.0) as u8;
                buf[i] = 12;
                buf[i + 1] = 12;
                buf[i + 2] = 14;
                buf[i + 3] = buf[i + 3].max(t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(x: i32, y: i32, w: u32, h: u32) -> Option<Frame> {
        // Encode the requested origin in the first pixel so tests can assert
        // capture geometry; fill the rest opaque white.
        let mut f = vec![255u8; (w * h * 4) as usize];
        f[0] = (x & 0xff) as u8;
        f[1] = (y & 0xff) as u8;
        Some(Frame::new(w, h, f))
    }

    fn scope(spec: ScopeSpec) -> ScopeSource {
        let mut s = ScopeSource::with_grabber("s", &spec, grid);
        s.set_viewport(Rect::new(100.0, 100.0, 200.0, 200.0), 0);
        s
    }

    #[test]
    fn zoom_shrinks_the_captured_region() {
        let f = scope(ScopeSpec {
            zoom: 2.0,
            ..Default::default()
        })
        .acquire()
        .unwrap();
        // 200x200 panel at zoom 2 → 100x100 capture.
        assert_eq!((f.width, f.height), (100, 100));

        let f = scope(ScopeSpec {
            zoom: 1.0,
            shape: ScopeShape::Square,
            ..Default::default()
        })
        .acquire()
        .unwrap();
        assert_eq!((f.width, f.height), (200, 200));
    }

    #[test]
    fn circle_masks_corners_keeps_centre() {
        let f = scope(ScopeSpec {
            zoom: 2.0,
            border: false,
            ..Default::default()
        })
        .acquire()
        .unwrap();
        assert_eq!(f.pixel(2, 2)[3], 0, "corner transparent");
        assert_eq!(f.pixel(50, 50)[3], 255, "centre opaque");
    }

    #[test]
    fn square_keeps_corners_and_rim_is_dark() {
        let f = scope(ScopeSpec {
            zoom: 2.0,
            shape: ScopeShape::Square,
            border: true,
            ..Default::default()
        })
        .acquire()
        .unwrap();
        assert!(f.pixel(1, 1)[3] > 0, "corner kept on square");
        let rim = f.pixel(1, 50);
        assert!(rim[0] < 30, "left rim dark: {rim:?}");
        assert_eq!(f.pixel(50, 50), [255, 255, 255, 255], "centre untouched");
    }

    #[test]
    fn without_viewport_no_frame_with_it_throttled() {
        let mut s = ScopeSource::with_grabber("s", &ScopeSpec::default(), grid);
        assert!(s.acquire().is_none(), "no viewport yet");
        s.set_viewport(Rect::new(0.0, 0.0, 100.0, 100.0), 0);
        assert!(s.acquire().is_some());
        assert!(s.acquire().is_none(), "throttled immediately after");
    }
}

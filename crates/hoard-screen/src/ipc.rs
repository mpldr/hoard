//! IPC with the desktop app. The Tauri process owns the editor and the asset
//! cache; this overlay process is launched by it and fed layout updates.
//!
//! Wire format: newline-delimited JSON, one [`Message`] per line, on stdin.
//! Trivial, dependency-free, and easy to drive from Tauri's `Command` sidecar
//! API (or a unix socket later). The overlay replies (nav state, picked window
//! ids) the same way on stdout.

use serde::{Deserialize, Serialize};

use crate::scene::Scene;

/// Desktop -> overlay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Replace the whole layout.
    SetScene { scene: Scene },
    /// Force a mode (e.g. the desktop's own "edit overlay" button); the global
    /// Ctrl+O hotkey toggles it locally too.
    SetEditor { editor: bool },
    /// Ask the overlay to shut down cleanly.
    Quit,
}

/// Parse one newline-stripped line into a [`Message`]. Blank lines yield `None`
/// so the reader can skip keepalives without erroring.
pub fn parse_line(line: &str) -> Result<Option<Message>, serde_json::Error> {
    let t = line.trim();
    if t.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(t).map(Some)
}

/// Serialise a message as a single line (no trailing newline).
pub fn to_line<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Crop, Panel, Rect, ScaleMode, SourceRef};

    #[test]
    fn blank_lines_are_skipped() {
        assert_eq!(parse_line("   ").unwrap(), None);
    }

    #[test]
    fn round_trips_set_scene() {
        let scene = Scene {
            panels: vec![Panel {
                id: "v1".into(),
                source: SourceRef::Window { id: "0xabc".into() },
                rect: Rect::new(10.0, 10.0, 640.0, 360.0),
                crop: Crop {
                    top: 0.1,
                    right: 0.0,
                    bottom: 0.1,
                    left: 0.0,
                },
                scale: ScaleMode::Fill,
                z: 2,
                monitor: Default::default(),
                passthrough: true,
                compat: false,
                passthrough_radius: 90.0,
            }],
        };
        let msg = Message::SetScene { scene };
        let line = to_line(&msg).unwrap();
        assert_eq!(parse_line(&line).unwrap(), Some(msg));
    }

    #[test]
    fn defaults_fill_in_optional_panel_fields() {
        // crop/scale/z omitted -> defaults.
        let line = r#"{"type":"set_scene","scene":{"panels":[
            {"id":"n","source":{"kind":"note","text":"hi"},
             "rect":{"x":0,"y":0,"w":100,"h":50}}]}}"#;
        let Message::SetScene { scene } = parse_line(line).unwrap().unwrap() else {
            panic!("expected set_scene");
        };
        assert_eq!(scene.panels[0].scale, ScaleMode::Fill);
        assert_eq!(scene.panels[0].z, 0);
        assert_eq!(scene.panels[0].crop, Crop::NONE);
    }

    #[test]
    fn parses_quit() {
        assert_eq!(
            parse_line(r#"{"type":"quit"}"#).unwrap(),
            Some(Message::Quit)
        );
    }
}

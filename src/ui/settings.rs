//! What survives a restart, as one JSON string in eframe's Storage. No serde derive here, and every
//! field falls back on its own: a settings file from another version must not lose the rest.

use super::grid::{self, Profiles};
use crate::library::CAMERAS;
use serde_json::{Value, json};
use std::path::PathBuf;

pub const KEY: &str = "settings";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Grid,
    Center,
    Driving,
    Custom,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::Grid, Layout::Center, Layout::Driving, Layout::Custom];

    pub fn name(self) -> &'static str {
        match self {
            Layout::Grid => "grid",
            Layout::Center => "center",
            Layout::Driving => "driving",
            Layout::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// km/h, else mph.
    pub metric: bool,
    pub layout: Layout,
    /// "map" or "sat"; Phase 3 reads it.
    pub map_mode: String,
    pub last_folder: Option<PathBuf>,
    pub library_open: bool,
    pub lanes_open: bool,
    /// Saved as `{ name: { camera: [x, y, w, h] | null } }`; never empty.
    pub custom_layouts: Profiles,
    /// A key of `custom_layouts`.
    pub custom_active: String,
    /// Camera indexes back to front, last moved on top.
    // shortcut: one stacking order for all saved layouts, the saved format has no room for one per layout; split it if overlapping layouts fight.
    pub custom_z: [usize; 6],
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            metric: true,
            layout: Layout::Grid,
            map_mode: "map".to_owned(),
            last_folder: None,
            library_open: true,
            lanes_open: true,
            custom_layouts: grid::default_profiles(),
            custom_active: "my layout".to_owned(),
            custom_z: [0, 1, 2, 3, 4, 5],
        }
    }
}

impl Settings {
    pub fn to_json(&self) -> String {
        json!({
            "units": if self.metric { "metric" } else { "imperial" },
            "layout": self.layout.name(),
            "map_mode": self.map_mode,
            // shortcut: a non-UTF-8 last_folder is forgotten on restart, store the OsStr units if anyone hits it.
            "last_folder": self.last_folder.as_ref().map(|p| p.to_string_lossy()),
            "library_open": self.library_open,
            "lanes_open": self.lanes_open,
            "custom_layouts": grid::profiles_to_json(&self.custom_layouts),
            "custom_active": self.custom_active,
            "custom_z": self.custom_z.map(|c| CAMERAS[c]),
        })
        .to_string()
    }

    pub fn from_json(s: &str) -> Settings {
        let d = Settings::default();
        let Ok(v) = serde_json::from_str::<Value>(s) else { return d };
        let custom_layouts = grid::profiles_from_json(&v["custom_layouts"]);
        let active = v["custom_active"].as_str().unwrap_or(&d.custom_active);
        let custom_active = if custom_layouts.contains_key(active) { active } else { custom_layouts.keys().next().map_or(active, String::as_str) };
        let custom_active = custom_active.to_owned();
        // Raising what's listed keeps it a permutation, whatever the file says.
        let mut custom_z = d.custom_z;
        for name in v["custom_z"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(i) = CAMERAS.iter().position(|c| *c == name) {
                grid::raise(&mut custom_z, i);
            }
        }
        Settings {
            metric: v["units"].as_str() != Some("imperial"),
            layout: Layout::ALL.into_iter().find(|l| v["layout"].as_str() == Some(l.name())).unwrap_or(d.layout),
            map_mode: v["map_mode"].as_str().map_or(d.map_mode, str::to_owned),
            last_folder: v["last_folder"].as_str().map(PathBuf::from),
            library_open: v["library_open"].as_bool().unwrap_or(d.library_open),
            lanes_open: v["lanes_open"].as_bool().unwrap_or(d.lanes_open),
            custom_layouts,
            custom_active,
            custom_z,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut custom_layouts = grid::default_profiles();
        custom_layouts.insert("layout 2".to_owned(), [Some([1.0, 2.0, 3.0, 4.0]), None, None, None, None, None]);
        let s = Settings {
            metric: false,
            layout: Layout::Driving,
            map_mode: "sat".to_owned(),
            last_folder: Some(PathBuf::from(r"E:\TeslaCam")),
            library_open: false,
            lanes_open: false,
            custom_layouts,
            custom_active: "layout 2".to_owned(),
            custom_z: [5, 4, 3, 2, 1, 0],
        };
        assert_eq!(Settings::from_json(&s.to_json()), s);
        assert_eq!(Settings::from_json(&Settings::default().to_json()), Settings::default());
    }

    #[test]
    fn bad_input_falls_back_per_field() {
        assert_eq!(Settings::from_json("not json"), Settings::default());
        assert_eq!(Settings::from_json("{}"), Settings::default());
        let s = Settings::from_json(
            r#"{"units":"imperial","layout":"nope","library_open":"x","custom_layouts":[],"custom_active":3,"custom_z":["back","bumper",3,"back"]}"#,
        );
        assert!(!s.metric);
        assert_eq!(s.layout, Layout::Grid);
        assert!(s.library_open);
        assert_eq!(s.custom_layouts, grid::default_profiles());
        assert_eq!(s.custom_active, "my layout");
        assert_eq!(s.custom_z, [0, 2, 3, 4, 5, 1]);
    }

    #[test]
    fn the_active_layout_always_exists() {
        let s = Settings::from_json(r#"{"custom_layouts":{"only":{"front":[0,0,4,4]}},"custom_active":"gone"}"#);
        assert_eq!(s.custom_active, "only");
    }
}

//! What survives a restart, as one JSON string in eframe's Storage. No serde derive here, and every
//! field falls back on its own: a settings file from another version must not lose the rest.

use super::grid::{self, Profiles};
use crate::library::CAMERAS;
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
    /// "map" or "sat".
    pub map_mode: String,
    /// Heading-up, else north-up.
    pub map_heading: bool,
    pub last_folder: Option<PathBuf>,
    /// Where the last export went.
    pub export_dir: Option<PathBuf>,
    pub library_open: bool,
    pub lanes_open: bool,
    /// Saved as `{ name: { camera: [x, y, w, h] | null } }`; never empty.
    pub custom_layouts: Profiles,
    /// A key of `custom_layouts`.
    pub custom_active: String,
    /// Camera indexes back to front, last moved on top.
    // shortcut: one stacking order for all saved layouts, the saved format has no room for one per layout; split it if overlapping layouts fight.
    pub custom_z: [usize; 6],
    /// The newest version the update dialog has opened for; it opens by itself once per version.
    pub update_seen: String,
    /// Where it happened, per event folder name: (from, to) as wall-clock `library::stamp` seconds, so
    /// a mark survives the event's segments changing. A point has from == to. Saved as `{ name: [from, to] }`.
    // shortcut: one mark per event, and the name is the only key (two cars saving in the same second would share one); keep a list or add the path if that's ever hit.
    pub marks: BTreeMap<String, (f64, f64)>,
    /// Where imported footage is kept on this PC; None until the user chooses to import.
    pub library_dir: Option<PathBuf>,
    /// Delete files from the drive once their copy is verified.
    pub import_clear: bool,
    /// Import without asking when a Tesla drive is plugged in.
    pub import_auto: bool,
    /// The welcome has been shown (or skipped); it never comes back by itself.
    pub onboarded: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            metric: true,
            layout: Layout::Grid,
            map_mode: "map".to_owned(),
            map_heading: false,
            last_folder: None,
            export_dir: None,
            library_open: true,
            lanes_open: true,
            custom_layouts: grid::default_profiles(),
            custom_active: "my layout".to_owned(),
            custom_z: [0, 1, 2, 3, 4, 5],
            update_seen: String::new(),
            marks: BTreeMap::new(),
            library_dir: None,
            import_clear: false,
            import_auto: false,
            onboarded: false,
        }
    }
}

impl Settings {
    /// Marks `name` at `mark`; the same mark again clears it, like In and Out.
    pub fn toggle_mark(&mut self, name: &str, mark: (f64, f64)) {
        let same = |m: &(f64, f64)| (m.0 - mark.0).abs() < 1e-3 && (m.1 - mark.1).abs() < 1e-3;
        if self.marks.get(name).is_some_and(same) {
            self.marks.remove(name);
        } else {
            self.marks.insert(name.to_owned(), mark);
        }
    }

    pub fn to_json(&self) -> String {
        json!({
            "units": if self.metric { "metric" } else { "imperial" },
            "layout": self.layout.name(),
            "map_mode": self.map_mode,
            "map_heading": self.map_heading,
            // shortcut: a non-UTF-8 last_folder is forgotten on restart, store the OsStr units if anyone hits it.
            "last_folder": self.last_folder.as_ref().map(|p| p.to_string_lossy()),
            "export_dir": self.export_dir.as_ref().map(|p| p.to_string_lossy()),
            "library_open": self.library_open,
            "lanes_open": self.lanes_open,
            "custom_layouts": grid::profiles_to_json(&self.custom_layouts),
            "custom_active": self.custom_active,
            "custom_z": self.custom_z.map(|c| CAMERAS[c]),
            "update_seen": self.update_seen,
            "library_dir": self.library_dir.as_ref().map(|p| p.to_string_lossy()),
            "import_clear": self.import_clear,
            "import_auto": self.import_auto,
            "onboarded": self.onboarded,
            "marks": self.marks.iter().map(|(k, &(a, b))| (k.clone(), json!([a, b]))).collect::<serde_json::Map<_, _>>(),
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
        let mark = |m: &Value| Some((m[0].as_f64()?, m[1].as_f64()?)).filter(|(a, b)| a.is_finite() && b >= a);
        let marks = v["marks"].as_object().into_iter().flatten().filter_map(|(k, m)| Some((k.clone(), mark(m)?))).collect();
        Settings {
            metric: v["units"].as_str() != Some("imperial"),
            layout: Layout::ALL.into_iter().find(|l| v["layout"].as_str() == Some(l.name())).unwrap_or(d.layout),
            map_mode: v["map_mode"].as_str().map_or(d.map_mode, str::to_owned),
            map_heading: v["map_heading"].as_bool().unwrap_or(d.map_heading),
            last_folder: v["last_folder"].as_str().map(PathBuf::from),
            export_dir: v["export_dir"].as_str().map(PathBuf::from),
            library_open: v["library_open"].as_bool().unwrap_or(d.library_open),
            lanes_open: v["lanes_open"].as_bool().unwrap_or(d.lanes_open),
            custom_layouts,
            custom_active,
            custom_z,
            update_seen: v["update_seen"].as_str().unwrap_or_default().to_owned(),
            marks,
            library_dir: v["library_dir"].as_str().map(PathBuf::from),
            import_clear: v["import_clear"].as_bool().unwrap_or(d.import_clear),
            import_auto: v["import_auto"].as_bool().unwrap_or(d.import_auto),
            onboarded: v["onboarded"].as_bool().unwrap_or(d.onboarded),
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
            map_heading: true,
            last_folder: Some(PathBuf::from(r"E:\TeslaCam")),
            export_dir: Some(PathBuf::from(r"E:\Exports")),
            library_open: false,
            lanes_open: false,
            custom_layouts,
            custom_active: "layout 2".to_owned(),
            custom_z: [5, 4, 3, 2, 1, 0],
            update_seen: "2.2.0".to_owned(),
            marks: BTreeMap::from([("2026-03-14_15-31-00".to_owned(), (1000.5, 1012.0)), ("point".to_owned(), (7.0, 7.0))]),
            library_dir: Some(PathBuf::from(r"C:\Videos\TeslaCam")),
            import_clear: true,
            import_auto: true,
            onboarded: true,
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
    fn marks_toggle_and_bad_ones_are_dropped() {
        let mut s = Settings::default();
        s.toggle_mark("a", (10.0, 12.0));
        s.toggle_mark("a", (11.0, 11.0));
        assert_eq!(s.marks["a"], (11.0, 11.0), "another place replaces it");
        s.toggle_mark("a", (11.0, 11.0));
        assert!(s.marks.is_empty(), "the same place again clears it");
        let s = Settings::from_json(r#"{"marks":{"ok":[5,6.5],"backwards":[9,2],"short":[1],"text":"x","nan":[null,1]}}"#);
        assert_eq!(s.marks, BTreeMap::from([("ok".to_owned(), (5.0, 6.5))]));
    }

    #[test]
    fn the_active_layout_always_exists() {
        let s = Settings::from_json(r#"{"custom_layouts":{"only":{"front":[0,0,4,4]}},"custom_active":"gone"}"#);
        assert_eq!(s.custom_active, "only");
    }
}

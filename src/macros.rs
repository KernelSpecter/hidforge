//! Macros: the recorded step list, the recorder, and on-disk persistence.
//!
//! A macro is a flat list of steps with explicit delays, which is what the OEM
//! tools call "Key in macro". Recording captures real hardware input through Raw
//! Input (so it sees side buttons and every key), timestamps it with QPC, and
//! converts the gaps into `Delay` steps.

use serde::{Deserialize, Serialize};

use crate::clock;
use crate::inject::Btn;
use crate::rawinput::Signal;

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Step {
    Button {
        btn: u8,
        down: bool,
    },
    /// Replayed by scancode where we have one: games reading DirectInput often
    /// ignore virtual-key-only events.
    Key {
        vk: u16,
        scan: u16,
        down: bool,
    },
    Wheel {
        delta: i32,
    },
    Move {
        dx: i32,
        dy: i32,
    },
    Delay {
        ms: f64,
    },
}

impl Step {
    pub fn label(&self) -> String {
        match *self {
            Step::Button { btn, down } => format!(
                "{} {}",
                btn_of(btn).name(),
                if down { "down" } else { "up" }
            ),
            Step::Key { vk, scan, down } => format!(
                "key {} sc{scan:#04x} vk{vk:#04x}",
                if down { "down" } else { "up  " }
            ),
            Step::Wheel { delta } => format!("wheel {delta:+}"),
            Step::Move { dx, dy } => format!("move {dx:+},{dy:+}"),
            Step::Delay { ms } => format!("delay {ms:.1} ms"),
        }
    }
}

pub fn btn_of(i: u8) -> Btn {
    match i {
        1 => Btn::Left,
        2 => Btn::Right,
        3 => Btn::Middle,
        4 => Btn::X1,
        _ => Btn::X2,
    }
}

/// How many times a macro runs once triggered. Names taken from the OEM tools,
/// because "hold vs toggle" told the user nothing about the behaviour.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum Cycle {
    UntilReleased,
    UntilClickedAgain,
    Times(u32),
}

impl Cycle {
    pub fn label(self) -> &'static str {
        match self {
            Cycle::UntilReleased => "Cycle until the key is released",
            Cycle::UntilClickedAgain => "Cycle until the key is clicked again",
            Cycle::Times(_) => "Specified cycle times",
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Macro {
    pub name: String,
    pub steps: Vec<Step>,
    pub cycle: Cycle,
    /// Scale every recorded delay. 1.0 = as recorded.
    pub speed: f64,
}

impl Macro {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
            cycle: Cycle::Times(1),
            speed: 1.0,
        }
    }

    /// Total wall time of one pass, after the speed scale.
    pub fn duration_ms(&self) -> f64 {
        self.steps
            .iter()
            .map(|s| match s {
                Step::Delay { ms } => ms / self.speed.max(0.01),
                _ => 0.0,
            })
            .sum()
    }
}

/// What a discovered button does when pressed.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Action {
    None,
    /// Repeat-fire a mouse button at the configured rate.
    RapidFire {
        btn: u8,
    },
    /// Run a macro by index.
    Macro {
        index: usize,
    },
}

impl Action {
    pub fn label(&self, macros: &[Macro]) -> String {
        match self {
            Action::None => "— not assigned —".into(),
            Action::RapidFire { btn } => format!("Rapid fire: {}", btn_of(*btn).name()),
            Action::Macro { index } => macros
                .get(*index)
                .map(|m| format!("Macro: {}", m.name))
                .unwrap_or_else(|| "Macro: <missing>".into()),
        }
    }
}

/// A button the user has taught us about, plus what it should do.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Slot {
    /// Display name. Defaults to the signal's own label; user-editable.
    pub name: String,
    pub signal: Signal,
    pub action: Action,
    /// How the trigger behaves. Lives here rather than on the macro because it
    /// describes the *button*, and the same macro can be driven both ways.
    #[serde(default = "default_cycle")]
    pub cycle: Cycle,
}

fn default_cycle() -> Cycle {
    Cycle::UntilReleased
}

// ------------------------------------------------------------------ recorder

pub struct Recorder {
    pub steps: Vec<Step>,
    last: Option<i64>,
    /// Ignore everything until the user has released whatever they clicked to
    /// press "Record" -- otherwise every macro starts with a stray mouse-up.
    armed: bool,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            last: None,
            armed: false,
        }
    }

    /// Feed a captured signal. Returns true if it was recorded.
    pub fn feed(&mut self, signal: &Signal, down: bool, qpc: i64) -> bool {
        // The first thing we see is usually the release of the click that started
        // recording. Wait for a fresh press before we start.
        if !self.armed {
            if !down {
                return false;
            }
            self.armed = true;
        }

        let step = match *signal {
            Signal::MouseButton { index, .. } if (1..=5).contains(&index) => {
                Step::Button { btn: index, down }
            }
            Signal::Key { vk, scancode, .. } => Step::Key {
                vk,
                scan: scancode,
                down,
            },
            Signal::Wheel { up, .. } if down => Step::Wheel {
                delta: if up { 120 } else { -120 },
            },
            // A vendor-page bit has no replayable meaning: we know a bit flipped,
            // not what the device intended by it. Recording it would produce a
            // step we cannot reproduce, so it is skipped deliberately.
            _ => return false,
        };

        if let Some(prev) = self.last {
            let gap = clock::ticks_to_ms(qpc - prev);
            if gap >= 1.0 {
                self.steps.push(Step::Delay { ms: gap });
            }
        }
        self.last = Some(qpc);
        self.steps.push(step);
        true
    }
}

// --------------------------------------------------------------- persistence

/// One profile: a complete set of buttons, macros and rates. Switching is
/// manual -- no foreground-window sniffing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub macros: Vec<Macro>,
    #[serde(default)]
    pub slots: Vec<Slot>,
    #[serde(default = "default_cps")]
    pub cps: f64,
    #[serde(default = "default_hold")]
    pub hold_ms: f64,
    #[serde(default)]
    pub jitter_pct: f64,
}

fn default_name() -> String {
    "Default".into()
}
fn default_cps() -> f64 {
    20.0
}
fn default_hold() -> f64 {
    1.0
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: default_name(),
            macros: Vec::new(),
            slots: Vec::new(),
            cps: default_cps(),
            hold_ms: default_hold(),
            jitter_pct: 0.0,
        }
    }
}

impl Profile {
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }
}

/// What lives on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    pub profiles: Vec<Profile>,
    pub active: usize,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            profiles: vec![Profile::named("Default")],
            active: 0,
        }
    }
}

pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&base)
        .join("hidforge")
        .join("config.json")
}

impl Store {
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(config_path()) else {
            return Self::default();
        };
        // Try the current layout, then the older single-profile file, so an
        // upgrade never silently discards someone's bindings.
        if let Ok(mut s) = serde_json::from_str::<Store>(&text) {
            if s.profiles.is_empty() {
                s.profiles.push(Profile::named("Default"));
            }
            s.active = s.active.min(s.profiles.len() - 1);
            return s;
        }
        if let Ok(p) = serde_json::from_str::<Profile>(&text) {
            return Self {
                profiles: vec![p],
                active: 0,
            };
        }
        Self::default()
    }

    /// Best-effort: a macro tool should not fall over because a directory is
    /// read-only, but the UI does surface the error.
    pub fn save(&self) -> std::io::Result<()> {
        let p = config_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(p, json)
    }
}

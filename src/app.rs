use std::collections::{HashMap, HashSet};

use egui::{
    Align, Color32, CornerRadius, FontId, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::clock;
use crate::engine::{Engine, Job, Mode, NB, Opts, Snap, bucket_ns};
use crate::inject::Btn;
use crate::macros::{Action, Cycle, Macro, Profile, Recorder, Slot, Step, Store, btn_of};
use crate::rawinput::{RawInput, Signal};
use crate::theme as t;

const SQ: CornerRadius = CornerRadius::ZERO;

/// Full-width selectable row. egui 0.35 removed the `SelectableLabel` widget and
/// `ui.selectable_label` cannot be given an explicit size, so this is a button
/// styled to read as a list row.
fn sel_row(ui: &mut Ui, selected: bool, text: RichText, width: f32) -> bool {
    let b = egui::Button::new(text.color(if selected { t::LIVE } else { t::INK }))
        .fill(if selected {
            t::PANEL
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0,
            if selected {
                t::LIVE
            } else {
                Color32::TRANSPARENT
            },
        ))
        .corner_radius(SQ);
    ui.add_sized(vec2(width, 22.0), b).clicked()
}

#[derive(PartialEq, Copy, Clone)]
enum Tab {
    Buttons,
    Macros,
    Scope,
    CpsTest,
    Devices,
}

impl Tab {
    const ALL: [(Tab, &'static str); 5] = [
        (Tab::Buttons, "BUTTONS"),
        (Tab::Macros, "MACROS"),
        (Tab::Scope, "SCOPE"),
        (Tab::CpsTest, "CPS TEST"),
        (Tab::Devices, "DEVICES"),
    ];
}

enum Test {
    Idle,
    Running { start: i64, end: i64 },
    Done { start: i64, end: i64 },
}

pub struct App {
    engine: Engine,
    raw: RawInput,
    /// Working copy of the active profile. Committed back into `store` on save
    /// and on switch, which keeps every widget binding to a plain field instead
    /// of indexing through the store on every frame.
    cfg: Profile,
    store: Store,
    tab: Tab,

    /// True while waiting for the user to press a button to learn.
    learning: bool,
    learn_log: Vec<(Signal, bool)>,
    /// Signals currently held. Swallows keyboard auto-repeat, which otherwise
    /// toggles a trigger dozens of times per physical press.
    pressed: HashSet<Signal>,
    /// slot index -> is its toggle currently on.
    toggled: HashMap<usize, bool>,
    /// slot index that is driving the engine right now, for hold-mode release.
    holding: Option<usize>,

    sel_slot: Option<usize>,
    sel_macro: Option<usize>,
    recorder: Option<Recorder>,
    rec_into: Option<usize>,

    test: Test,
    test_secs: f64,
    test_clicks: Vec<i64>,
    bench_target: f64,
    status: String,
    /// Window hidden to the tray; macros keep running.
    hidden: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        t::apply(&cc.egui_ctx);
        crate::tray::start(cc.egui_ctx.clone());
        let store = Store::load();
        let cfg = store.profiles[store.active.min(store.profiles.len() - 1)].clone();
        Self {
            engine: Engine::start(),
            raw: RawInput::start(cc.egui_ctx.clone()),
            cfg,
            store,
            tab: Tab::Buttons,
            learning: false,
            learn_log: Vec::new(),
            pressed: Default::default(),
            toggled: Default::default(),
            holding: None,
            sel_slot: None,
            sel_macro: None,
            recorder: None,
            rec_into: None,
            test: Test::Idle,
            test_secs: 10.0,
            test_clicks: Vec::new(),
            bench_target: 1000.0,
            status: String::new(),
            hidden: false,
        }
    }

    fn save(&mut self) {
        let i = self
            .store
            .active
            .min(self.store.profiles.len().saturating_sub(1));
        if let Some(slot) = self.store.profiles.get_mut(i) {
            *slot = self.cfg.clone();
        }
        match self.store.save() {
            Ok(()) => self.status = "saved".into(),
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    /// Commit the working copy, then make `i` active.
    fn switch_profile(&mut self, i: usize) {
        if i >= self.store.profiles.len() || i == self.store.active {
            return;
        }
        // A macro from the old profile must not keep running under the new one.
        self.stop();
        self.save();
        self.store.active = i;
        self.cfg = self.store.profiles[i].clone();
        self.sel_slot = None;
        self.sel_macro = None;
        self.pressed.clear();
        self.status = format!("profile: {}", self.cfg.name);
        let _ = self.store.save();
    }

    fn click_job(&self, btn: Btn) -> Job {
        let mut j = Job::click(btn, self.cfg.cps, self.cfg.hold_ms);
        j.jitter_pct = self.cfg.jitter_pct;
        j
    }

    fn macro_job(&self, m: &Macro) -> Job {
        Job {
            mode: Mode::Sequence,
            steps: m.steps.clone(),
            speed: m.speed,
            ..Job::click(Btn::Left, 1.0, 0.0)
        }
    }

    /// Start whatever a slot's action says, with the given repeat limit.
    fn fire(&mut self, slot: usize, limit: Option<u64>) {
        let Some(s) = self.cfg.slots.get(slot).cloned() else {
            return;
        };
        let job = match s.action {
            Action::None => return,
            Action::RapidFire { btn } => Job {
                limit,
                ..self.click_job(btn_of(btn))
            },
            Action::Macro { index } => {
                let Some(m) = self.cfg.macros.get(index).cloned() else {
                    return;
                };
                Job {
                    limit,
                    ..self.macro_job(&m)
                }
            }
        };
        self.engine.run(job, Opts::default());
    }

    fn stop(&mut self) {
        self.engine.stop();
        self.holding = None;
        self.toggled.clear();
    }

    fn running(&self) -> bool {
        self.engine
            .tel
            .running
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Capture is registered only when something needs it -- a 1000 Hz mouse is
    /// not free to listen to, and idle should cost nothing.
    fn wanted_capture(&self) -> bool {
        // `self.running()` is load-bearing for the emergency stop: Esc is read
        // from captured input, so if a macro is running we MUST be listening or
        // there would be no way to stop it. Previously a macro started from the
        // Play button with no buttons assigned ran with capture off, and Esc did
        // nothing.
        self.running()
            || self.learning
            || self.recorder.is_some()
            || !self.cfg.slots.is_empty()
            || matches!(self.test, Test::Running { .. })
            || self.tab == Tab::Devices
    }

    fn handle_input(&mut self) {
        let events = self.raw.drain();

        // Digitizers are excluded upstream, but a vendor page can still stream.
        // Drop HID-bit candidates from any device flooding this batch, or brushing
        // a touch surface auto-commits a garbage binding.
        let mut churn: HashMap<i64, usize> = HashMap::new();
        if self.learning {
            for e in &events {
                if matches!(e.signal, Signal::HidBit { .. }) {
                    *churn.entry(e.signal.device()).or_insert(0) += 1;
                }
            }
        }

        for e in events {
            // Swallow keyboard auto-repeat. A stray UP is deliberately NOT
            // filtered: if we missed the DOWN, stopping is the safe failure.
            if e.down && !self.pressed.insert(e.signal.clone()) {
                continue;
            }
            if !e.down {
                self.pressed.remove(&e.signal);
            }

            // Panic key, independent of any binding.
            if e.down
                && let Signal::Key { vk, .. } = e.signal
                && vk == 0x1B
            {
                self.stop();
                if self.recorder.is_some() {
                    self.finish_recording();
                }
                self.learning = false;
                self.status = "stopped (Esc)".into();
                continue;
            }

            if self.learning {
                if matches!(e.signal, Signal::HidBit { .. })
                    && churn.get(&e.signal.device()).copied().unwrap_or(0) > 6
                {
                    continue;
                }
                self.learn_log.push((e.signal.clone(), e.down));
                if self.learn_log.len() > 24 {
                    self.learn_log.remove(0);
                }
                // Commit on release, and only if we saw its press -- confirms a
                // button rather than an axis twitching.
                if !e.down && self.learn_log.iter().any(|(s, d)| *d && *s == e.signal) {
                    self.add_slot(e.signal.clone());
                    self.learning = false;
                }
                continue;
            }

            if let Some(rec) = self.recorder.as_mut() {
                rec.feed(&e.signal, e.down, e.qpc);
                continue;
            }

            if matches!(self.test, Test::Running { .. })
                && e.down
                && matches!(e.signal, Signal::MouseButton { index: 1, .. })
            {
                self.test_clicks.push(e.qpc);
            }

            // Dispatch to any slot bound to this signal.
            let hits: Vec<usize> = self
                .cfg
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.signal == e.signal)
                .map(|(i, _)| i)
                .collect();
            for i in hits {
                let cycle = self.cfg.slots[i].cycle;
                match cycle {
                    Cycle::UntilReleased => {
                        if e.down {
                            self.holding = Some(i);
                            self.fire(i, None);
                        } else if self.holding == Some(i) {
                            self.holding = None;
                            self.engine.stop();
                        }
                    }
                    Cycle::UntilClickedAgain => {
                        if e.down {
                            let on = *self.toggled.get(&i).unwrap_or(&false);
                            if on {
                                self.toggled.insert(i, false);
                                self.engine.stop();
                            } else {
                                self.toggled.insert(i, true);
                                self.fire(i, None);
                            }
                        }
                    }
                    Cycle::Times(n) => {
                        if e.down {
                            self.fire(i, Some(n.max(1) as u64));
                        }
                    }
                }
            }
        }

        if let Test::Running { start, end } = self.test
            && clock::qpc() >= end
        {
            self.test = Test::Done { start, end };
        }
    }

    fn add_slot(&mut self, signal: Signal) {
        if self.cfg.slots.iter().any(|s| s.signal == signal) {
            self.status = format!("{} is already in the list", signal.label());
            return;
        }
        self.cfg.slots.push(Slot {
            name: signal.label(),
            signal,
            action: Action::None,
            cycle: Cycle::UntilReleased,
        });
        self.sel_slot = Some(self.cfg.slots.len() - 1);
        self.status = "button learned — now assign it an action".into();
        self.save();
    }

    /// Tray requests, plus close-to-tray. Closing hides the window rather than
    /// quitting, so armed macros keep working with nothing on screen.
    fn handle_tray(&mut self, ctx: &egui::Context) {
        use egui::ViewportCommand;
        use std::sync::atomic::Ordering;

        if crate::tray::WANT_QUIT.swap(false, Ordering::Relaxed) {
            self.stop();
            self.save();
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        if crate::tray::WANT_STOP.swap(false, Ordering::Relaxed) {
            self.stop();
            self.status = "stopped from tray".into();
        }
        if crate::tray::WANT_SHOW.swap(false, Ordering::Relaxed) {
            self.hidden = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            // Only hide if there is actually a tray icon to get back from --
            // otherwise the window would vanish with no way to reopen it.
            if crate::tray::ACTIVE.load(Ordering::Relaxed) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                self.hidden = true;
                self.save();
            } else {
                self.save();
            }
        }
    }

    fn finish_recording(&mut self) {
        if let (Some(rec), Some(idx)) = (self.recorder.take(), self.rec_into.take()) {
            let n = rec.steps.len();
            if let Some(m) = self.cfg.macros.get_mut(idx) {
                m.steps = rec.steps;
            }
            self.status = format!("recorded {n} steps");
            self.save();
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        let c = t::BG;
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            1.0,
        ]
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.handle_input();
        self.handle_tray(ui.ctx());
        let snap = self.engine.tel.snapshot();

        self.header(ui, &snap);
        self.profile_bar(ui);
        self.rail(ui);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(t::BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ui, |ui| match self.tab {
                Tab::Buttons => self.buttons_tab(ui),
                Tab::Macros => self.macros_tab(ui),
                Tab::Scope => self.scope(ui, &snap),
                Tab::CpsTest => self.cps_test(ui, &snap),
                Tab::Devices => self.devices(ui),
            });

        // Sync capture AFTER the UI runs. Doing it first meant a click on "Learn"
        // only applied on the next frame -- which never came, because the event
        // that would cause it was the one we were not yet listening for.
        let want = self.wanted_capture();
        self.raw.set_capture(want);

        if snap.running
            || self.learning
            || self.recorder.is_some()
            || matches!(self.test, Test::Running { .. })
        {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut Ui, snap: &Snap) {
        egui::Panel::top("hdr")
            .exact_size(78.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(t::PANEL)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 10,
                        bottom: 8,
                    }),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.horizontal(|ui| {
                            let live = snap.running;
                            let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                            ui.painter()
                                .rect_filled(r, SQ, if live { t::LIVE } else { t::FAINT });
                            ui.label(
                                RichText::new(if live { "LIVE" } else { "IDLE" })
                                    .font(FontId::monospace(12.0))
                                    .color(if live { t::LIVE } else { t::DIM }),
                            );
                        });
                        ui.label(
                            RichText::new("HIDFORGE")
                                .font(FontId::monospace(17.0))
                                .color(t::INK),
                        );
                    });

                    ui.add_space(24.0);
                    t::readout(
                        ui,
                        "achieved",
                        &format!("{:.0}", snap.cps(snap.mean_ms)),
                        "cps",
                        if snap.running { t::LIVE } else { t::DIM },
                        24.0,
                    );
                    ui.add_space(16.0);
                    t::readout(
                        ui,
                        "mean period",
                        &format!("{:.3}", snap.mean_ms),
                        "ms",
                        t::INK,
                        24.0,
                    );
                    ui.add_space(16.0);
                    t::readout(
                        ui,
                        "spin cost",
                        &format!("{:.0}", snap.spin_pct),
                        "% core",
                        if snap.spin_pct > 60.0 {
                            t::WARN
                        } else {
                            t::GOOD
                        },
                        24.0,
                    );

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let live = snap.running;
                        let b = egui::Button::new(
                            RichText::new(if live { "STOP" } else { "TEST CLICK" })
                                .font(FontId::monospace(13.0))
                                .color(if live { t::BG } else { t::LIVE }),
                        )
                        .fill(if live { t::LIVE } else { t::PANEL })
                        .stroke(Stroke::new(1.0, t::LIVE))
                        .corner_radius(SQ);
                        if ui.add_sized(vec2(112.0, 34.0), b).clicked() {
                            if live {
                                self.stop();
                            } else {
                                let j = self.click_job(Btn::Left);
                                self.engine.run(j, Opts::default());
                            }
                        }
                        if !self.status.is_empty() {
                            ui.label(RichText::new(&self.status).size(10.5).color(t::DIM));
                        }
                    });
                });
            });
    }

    fn profile_bar(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("profiles")
            .exact_size(46.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(t::PANEL)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 14,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t::caption("profile"));
                    ui.add_space(6.0);
                    let mut go: Option<usize> = None;
                    for (i, p) in self.store.profiles.iter().enumerate() {
                        let on = i == self.store.active;
                        let b = egui::Button::new(
                            RichText::new(&p.name)
                                .font(FontId::monospace(11.5))
                                .color(if on { t::BG } else { t::DIM }),
                        )
                        .fill(if on { t::LIVE } else { Color32::TRANSPARENT })
                        .stroke(Stroke::new(1.0, if on { t::LIVE } else { t::RULE_SOFT }))
                        .corner_radius(SQ);
                        if ui.add(b).clicked() {
                            go = Some(i);
                        }
                    }
                    if let Some(i) = go {
                        self.switch_profile(i);
                    }

                    // Deliberately a flat left-to-right layout with no nested
                    // right_to_left and no TextEdit: that combination inside a
                    // fixed-height panel oscillated its width negotiation every
                    // frame, which pushed idle CPU to ~50 %. Renaming lives in the
                    // BUTTONS tab instead.
                    ui.add_space(12.0);
                    if ui.small_button("+ new").clicked() {
                        let n = self.store.profiles.len() + 1;
                        self.save();
                        self.store
                            .profiles
                            .push(Profile::named(format!("Profile {n}")));
                        let last = self.store.profiles.len() - 1;
                        self.store.active = last;
                        self.cfg = self.store.profiles[last].clone();
                        self.sel_slot = None;
                        self.sel_macro = None;
                        let _ = self.store.save();
                    }
                    if self.store.profiles.len() > 1 && ui.small_button("delete").clicked() {
                        let i = self.store.active;
                        self.store.profiles.remove(i);
                        self.store.active = 0;
                        self.cfg = self.store.profiles[0].clone();
                        self.sel_slot = None;
                        self.sel_macro = None;
                        let _ = self.store.save();
                        self.status = "profile deleted".into();
                    }
                });
            });
    }

    fn rail(&mut self, ui: &mut Ui) {
        egui::Panel::left("rail")
            .exact_size(210.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(t::BG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ui, |ui| {
                for (tab, name) in Tab::ALL {
                    let on = self.tab == tab;
                    let b = egui::Button::new(
                        RichText::new(name)
                            .font(FontId::monospace(11.5))
                            .color(if on { t::LIVE } else { t::DIM }),
                    )
                    .fill(if on { t::PANEL } else { Color32::TRANSPARENT })
                    .stroke(Stroke::new(1.0, if on { t::LIVE } else { t::RULE_SOFT }))
                    .corner_radius(SQ);
                    let w = ui.available_width();
                    if ui.add_sized(vec2(w, 26.0), b).clicked() {
                        self.tab = tab;
                    }
                }

                t::section(ui, "click rate");
                ui.add(
                    egui::Slider::new(&mut self.cfg.cps, 1.0..=20_000.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{v:.0} cps")),
                );
                ui.label(
                    RichText::new(format!("period {:.3} ms", 1000.0 / self.cfg.cps.max(0.01)))
                        .size(10.0)
                        .color(t::FAINT),
                );
                ui.add_space(4.0);
                ui.label(t::caption("hold"));
                ui.add(
                    egui::Slider::new(&mut self.cfg.hold_ms, 0.0..=50.0)
                        .custom_formatter(|v, _| format!("{v:.2} ms")),
                );
                ui.add_space(4.0);
                ui.label(t::caption("jitter"));
                ui.add(
                    egui::Slider::new(&mut self.cfg.jitter_pct, 0.0..=50.0)
                        .custom_formatter(|v, _| format!("{v:.0} %")),
                );

                ui.add_space(10.0);
                t::rule(ui);
                ui.add_space(6.0);
                if ui.button("Save settings").clicked() {
                    self.save();
                }
                ui.label(
                    RichText::new("Esc always stops everything")
                        .size(10.0)
                        .color(t::FAINT),
                );
            });
    }

    // ---------------------------------------------------------- buttons tab

    fn buttons_tab(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("profile name").size(10.5).color(t::DIM));
            if ui
                .add(egui::TextEdit::singleline(&mut self.cfg.name).desired_width(160.0))
                .lost_focus()
            {
                self.save();
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(t::caption("assigned buttons"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.learning {
                    if ui.small_button("cancel").clicked() {
                        self.learning = false;
                    }
                    ui.label(
                        RichText::new("press and release a button…")
                            .font(FontId::monospace(11.5))
                            .color(t::LIVE),
                    );
                } else if ui.button("Learn a button…").clicked() {
                    self.learning = true;
                    self.learn_log.clear();
                }
            });
        });
        ui.add_space(2.0);
        t::rule(ui);
        ui.add_space(6.0);

        if self.cfg.slots.is_empty() {
            ui.label(
                RichText::new(
                    "No buttons yet. Click \"Learn a button\", then press the button you want to \
                     use — mouse or keyboard. Whatever it actually emits gets captured, so side \
                     buttons work even though they arrive on odd channels.",
                )
                .size(11.0)
                .color(t::FAINT),
            );
        }

        let mut remove: Option<usize> = None;
        let macros = self.cfg.macros.clone();
        for i in 0..self.cfg.slots.len() {
            let sel = self.sel_slot == Some(i);
            let (name, sig, act) = {
                let s = &self.cfg.slots[i];
                (s.name.clone(), s.signal.label(), s.action.label(&macros))
            };
            let bg = if sel { t::PANEL } else { Color32::TRANSPARENT };
            egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{:>2}", i + 1))
                                .font(FontId::monospace(12.0))
                                .color(t::FAINT),
                        );
                        if sel_row(
                            ui,
                            sel,
                            RichText::new(&name).font(FontId::monospace(12.0)),
                            170.0,
                        ) {
                            self.sel_slot = Some(i);
                        }
                        ui.label(RichText::new(sig).size(10.5).color(t::FAINT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("remove").clicked() {
                                remove = Some(i);
                            }
                            ui.label(RichText::new(act).font(FontId::monospace(11.5)).color(
                                if matches!(self.cfg.slots[i].action, Action::None) {
                                    t::FAINT
                                } else {
                                    t::LIVE
                                },
                            ));
                        });
                    });
                });
        }
        if let Some(i) = remove {
            self.cfg.slots.remove(i);
            self.sel_slot = None;
            self.save();
        }

        // Assignment editor for the selected row. A panel rather than a dropdown
        // per row: it keeps the list scannable and the options fully visible.
        if let Some(i) = self.sel_slot.filter(|i| *i < self.cfg.slots.len()) {
            ui.add_space(10.0);
            t::section(ui, "assignment");
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("name").size(11.0).color(t::DIM));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.cfg.slots[i].name)
                            .desired_width(180.0),
                    )
                    .lost_focus()
                {
                    changed = true;
                }
            });

            ui.add_space(6.0);
            ui.label(t::caption("action"));
            let cur = self.cfg.slots[i].action.clone();
            if ui.radio(matches!(cur, Action::None), "Nothing").clicked() {
                self.cfg.slots[i].action = Action::None;
                changed = true;
            }
            ui.horizontal(|ui| {
                let on = matches!(cur, Action::RapidFire { .. });
                if ui.radio(on, "Rapid fire").clicked() {
                    self.cfg.slots[i].action = Action::RapidFire { btn: 1 };
                    changed = true;
                }
                if on {
                    for b in 1u8..=5 {
                        let is = matches!(cur, Action::RapidFire { btn } if btn == b);
                        if ui
                            .selectable_label(is, RichText::new(btn_of(b).name()).size(11.0))
                            .clicked()
                        {
                            self.cfg.slots[i].action = Action::RapidFire { btn: b };
                            changed = true;
                        }
                    }
                }
            });
            let macro_on = matches!(cur, Action::Macro { .. });
            if ui.radio(macro_on, "Run a macro").clicked() && !macro_on {
                self.cfg.slots[i].action = Action::Macro { index: 0 };
                changed = true;
            }
            if macro_on {
                if self.cfg.macros.is_empty() {
                    ui.label(
                        RichText::new("no macros yet — record one in the MACROS tab")
                            .size(10.5)
                            .color(t::WARN),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    for (mi, m) in self.cfg.macros.iter().enumerate() {
                        let is = matches!(cur, Action::Macro { index } if index == mi);
                        if ui
                            .selectable_label(is, RichText::new(&m.name).size(11.0))
                            .clicked()
                        {
                            self.cfg.slots[i].action = Action::Macro { index: mi };
                            changed = true;
                        }
                    }
                });
            }

            ui.add_space(8.0);
            ui.label(t::caption("cycle"));
            let cyc = self.cfg.slots[i].cycle;
            for c in [
                Cycle::UntilReleased,
                Cycle::UntilClickedAgain,
                Cycle::Times(0),
            ] {
                let on = std::mem::discriminant(&cyc) == std::mem::discriminant(&c);
                if ui.radio(on, RichText::new(c.label()).size(11.0)).clicked() {
                    self.cfg.slots[i].cycle = match c {
                        Cycle::Times(_) => Cycle::Times(50),
                        other => other,
                    };
                    changed = true;
                }
            }
            if let Cycle::Times(n) = self.cfg.slots[i].cycle {
                let mut v = n;
                if ui
                    .add(
                        egui::DragValue::new(&mut v)
                            .range(1..=1_000_000)
                            .prefix("x "),
                    )
                    .changed()
                {
                    self.cfg.slots[i].cycle = Cycle::Times(v);
                    changed = true;
                }
            }

            if changed {
                self.save();
            }

            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "The original button still works as well — this adds an action rather than \
                     replacing it. Replacing a button needs a low-level hook, which only works for \
                     the five standard mouse buttons.",
                )
                .size(10.5)
                .color(t::FAINT),
            );
        }
    }

    // ----------------------------------------------------------- macros tab

    fn macros_tab(&mut self, ui: &mut Ui) {
        let recording = self.recorder.is_some();
        let total_w = ui.available_width();

        ui.horizontal_top(|ui| {
            // ---- macro list
            ui.vertical(|ui| {
                ui.set_width((total_w * 0.30).min(250.0));
                ui.label(t::caption("macro list"));
                ui.add_space(3.0);
                t::rule(ui);
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("mlist")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for i in 0..self.cfg.macros.len() {
                            let sel = self.sel_macro == Some(i);
                            let label = format!(
                                "{}  ({} steps)",
                                self.cfg.macros[i].name,
                                self.cfg.macros[i].steps.len()
                            );
                            let w = ui.available_width();
                            if sel_row(
                                ui,
                                sel,
                                RichText::new(label).font(FontId::monospace(11.0)),
                                w,
                            ) {
                                self.sel_macro = Some(i);
                            }
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.small_button("+ new").clicked() {
                        let n = self.cfg.macros.len() + 1;
                        self.cfg.macros.push(Macro::new(format!("Macro {n}")));
                        self.sel_macro = Some(self.cfg.macros.len() - 1);
                        self.save();
                    }
                    if ui.small_button("delete").clicked()
                        && let Some(i) = self.sel_macro
                        && i < self.cfg.macros.len()
                    {
                        self.cfg.macros.remove(i);
                        // Any slot pointing at a shifted index would now
                        // run the wrong macro, so unassign them.
                        for s in self.cfg.slots.iter_mut() {
                            if let Action::Macro { index } = s.action {
                                if index == i {
                                    s.action = Action::None;
                                } else if index > i {
                                    s.action = Action::Macro { index: index - 1 };
                                }
                            }
                        }
                        self.sel_macro = None;
                        self.save();
                    }
                });
            });

            ui.add_space(14.0);

            // ---- steps of the selected macro
            ui.vertical(|ui| {
                ui.set_width((total_w * 0.34).min(300.0));
                ui.label(t::caption("key in macro"));
                ui.add_space(3.0);
                t::rule(ui);
                ui.add_space(4.0);

                let Some(mi) = self.sel_macro.filter(|i| *i < self.cfg.macros.len()) else {
                    ui.label(
                        RichText::new("select or create a macro")
                            .size(11.0)
                            .color(t::FAINT),
                    );
                    return;
                };

                let live: Vec<Step> = self
                    .recorder
                    .as_ref()
                    .filter(|_| self.rec_into == Some(mi))
                    .map(|r| r.steps.clone())
                    .unwrap_or_else(|| self.cfg.macros[mi].steps.clone());

                let mut del: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .id_salt("steps")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        if live.is_empty() {
                            ui.label(
                                RichText::new("empty — press Record and do the actions")
                                    .size(10.5)
                                    .color(t::FAINT),
                            );
                        }
                        for (i, s) in live.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("{:>3}", i + 1))
                                        .font(FontId::monospace(10.0))
                                        .color(t::FAINT),
                                );
                                let c = if matches!(s, Step::Delay { .. }) {
                                    t::DIM
                                } else {
                                    t::INK
                                };
                                ui.label(
                                    RichText::new(s.label())
                                        .font(FontId::monospace(11.0))
                                        .color(c),
                                );
                                if !recording {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.small_button("x").clicked() {
                                            del = Some(i);
                                        }
                                    });
                                }
                            });
                        }
                    });
                if let Some(i) = del {
                    self.cfg.macros[mi].steps.remove(i);
                    self.save();
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "{} steps · {:.0} ms per pass",
                        live.len(),
                        self.cfg.macros[mi].duration_ms()
                    ))
                    .size(10.5)
                    .color(t::FAINT),
                );
            });

            ui.add_space(14.0);

            // ---- controls
            ui.vertical(|ui| {
                ui.label(t::caption("record & play"));
                ui.add_space(3.0);
                t::rule(ui);
                ui.add_space(6.0);

                let Some(mi) = self.sel_macro.filter(|i| *i < self.cfg.macros.len()) else {
                    return;
                };

                ui.horizontal(|ui| {
                    ui.label(RichText::new("name").size(11.0).color(t::DIM));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.cfg.macros[mi].name)
                                .desired_width(150.0),
                        )
                        .lost_focus()
                    {
                        self.save();
                    }
                });

                ui.add_space(8.0);
                if recording {
                    let b = egui::Button::new(
                        RichText::new("STOP RECORDING")
                            .font(FontId::monospace(12.0))
                            .color(t::BG),
                    )
                    .fill(t::LIVE)
                    .corner_radius(SQ);
                    if ui.add_sized(vec2(170.0, 30.0), b).clicked() {
                        self.finish_recording();
                    }
                    ui.label(
                        RichText::new("recording — every key and button is captured")
                            .size(10.5)
                            .color(t::LIVE),
                    );
                } else {
                    let b = egui::Button::new(
                        RichText::new("RECORD")
                            .font(FontId::monospace(12.0))
                            .color(t::LIVE),
                    )
                    .stroke(Stroke::new(1.0, t::LIVE))
                    .corner_radius(SQ);
                    if ui.add_sized(vec2(170.0, 30.0), b).clicked() {
                        self.recorder = Some(Recorder::new());
                        self.rec_into = Some(mi);
                        self.status = "recording…".into();
                    }
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Play once").clicked() {
                        let m = self.cfg.macros[mi].clone();
                        let job = Job {
                            limit: Some(1),
                            ..self.macro_job(&m)
                        };
                        self.engine.run(job, Opts::default());
                    }
                    if ui.button("Stop").clicked() {
                        self.stop();
                    }
                });

                ui.add_space(8.0);
                ui.label(t::caption("speed"));
                let mut sp = self.cfg.macros[mi].speed;
                if ui
                    .add(
                        egui::Slider::new(&mut sp, 0.1..=10.0)
                            .logarithmic(true)
                            .custom_formatter(|v, _| format!("{v:.2}x")),
                    )
                    .changed()
                {
                    self.cfg.macros[mi].speed = sp;
                }

                ui.add_space(10.0);
                ui.label(
                    RichText::new(
                        "Recording captures real hardware input, so side buttons and any key are \
                         included. Vendor-page bits are skipped: we can see the bit flip but not \
                         what the device meant by it, so it could not be replayed faithfully.",
                    )
                    .size(10.0)
                    .color(t::FAINT),
                );
            });
        });
    }

    // ---------------------------------------------------------------- scope

    fn scope(&mut self, ui: &mut Ui, snap: &Snap) {
        let target = 1000.0 / self.cfg.cps.max(0.01);
        ui.label(t::caption("period distribution"));
        ui.add_space(4.0);
        hist(ui, snap, target);
        ui.add_space(14.0);
        ui.columns(2, |c| {
            c[0].label(t::caption("timing"));
            c[0].add_space(3.0);
            t::stat(
                &mut c[0],
                "target period",
                format!("{target:.3} ms"),
                t::DIM,
            );
            t::stat(
                &mut c[0],
                "mean (exact)",
                format!("{:.3} ms", snap.mean_ms),
                t::INK,
            );
            t::stat(&mut c[0], "p95", format!("{:.3} ms", snap.p95_ms), t::INK);
            t::stat(&mut c[0], "p99", format!("{:.3} ms", snap.p99_ms), t::INK);
            t::stat(
                &mut c[0],
                "worst",
                format!("{:.3} ms", snap.max_ms),
                if snap.max_ms > target * 3.0 && snap.total > 20 {
                    t::WARN
                } else {
                    t::INK
                },
            );
            c[1].label(t::caption("rate"));
            c[1].add_space(3.0);
            t::stat(
                &mut c[1],
                "requested",
                format!("{:.0} cps", self.cfg.cps),
                t::DIM,
            );
            t::stat(
                &mut c[1],
                "achieved",
                format!("{:.0} cps", snap.cps(snap.mean_ms)),
                t::LIVE,
            );
            t::stat(&mut c[1], "events", format!("{}", snap.events), t::INK);
            t::stat(
                &mut c[1],
                "spin margin",
                format!("{:.3} ms", snap.margin_ms),
                t::INK,
            );
            t::stat(
                &mut c[1],
                "spin cost",
                format!("{:.0} % of a core", snap.spin_pct),
                if snap.spin_pct > 60.0 {
                    t::WARN
                } else {
                    t::GOOD
                },
            );
        });
    }

    // ------------------------------------------------------------- cps test

    fn cps_test(&mut self, ui: &mut Ui, snap: &Snap) {
        ui.label(t::caption("your clicking"));
        ui.add_space(4.0);
        let elapsed = match self.test {
            Test::Running { start, .. } => clock::ticks_to_ms(clock::qpc() - start) / 1000.0,
            Test::Done { start, end } => clock::ticks_to_ms(end - start) / 1000.0,
            Test::Idle => 0.0,
        };
        let running = matches!(self.test, Test::Running { .. });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !running,
                    egui::Button::new(RichText::new("start").font(FontId::monospace(13.0)))
                        .corner_radius(SQ)
                        .min_size(vec2(84.0, 30.0)),
                )
                .clicked()
            {
                let now = clock::qpc();
                self.test_clicks.clear();
                self.test = Test::Running {
                    start: now,
                    end: now + clock::ms_to_ticks(self.test_secs * 1000.0),
                };
            }
            ui.add(
                egui::Slider::new(&mut self.test_secs, 1.0..=30.0)
                    .custom_formatter(|v, _| format!("{v:.0} s")),
            );
            if running {
                ui.label(
                    RichText::new(format!("{:.1} s left", (self.test_secs - elapsed).max(0.0)))
                        .font(FontId::monospace(12.0))
                        .color(t::LIVE),
                );
            }
        });
        ui.label(
            RichText::new(
                "Counts real left-clicks off the hardware. Our own synthetic clicks come back with \
                 a null device handle and are discarded, so an autoclicker cannot inflate this.",
            )
            .size(10.5)
            .color(t::FAINT),
        );

        ui.add_space(10.0);
        let n = self.test_clicks.len();
        let dur = if elapsed > 0.05 { elapsed } else { 1.0 };
        let mut gaps: Vec<f64> = self
            .test_clicks
            .windows(2)
            .map(|w| clock::ticks_to_ms(w[1] - w[0]))
            .collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let gmin = gaps.first().copied().unwrap_or(0.0);
        let win = clock::ms_to_ticks(1000.0);
        let mut best = 0usize;
        for (i, &a) in self.test_clicks.iter().enumerate() {
            best = best.max(
                self.test_clicks[i..]
                    .iter()
                    .take_while(|&&b| b - a <= win)
                    .count(),
            );
        }
        ui.horizontal(|ui| {
            t::readout(
                ui,
                "average",
                &format!("{:.1}", n as f64 / dur),
                "cps",
                t::INK,
                28.0,
            );
            ui.add_space(20.0);
            t::readout(ui, "best second", &format!("{best}"), "cps", t::LIVE, 28.0);
            ui.add_space(20.0);
            t::readout(ui, "clicks", &format!("{n}"), "", t::INK, 28.0);
            ui.add_space(20.0);
            t::readout(ui, "min gap", &format!("{gmin:.2}"), "ms", t::INK, 28.0);
        });
        if gmin > 0.0 {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "Smallest gap {gmin:.2} ms — your firmware's debounce floor plus poll interval. \
                     Software can raise that to kill a chattering switch, never lower it."
                ))
                .size(10.5)
                .color(t::FAINT),
            );
        }

        ui.add_space(14.0);
        t::rule(ui);
        ui.add_space(8.0);
        ui.label(t::caption("engine benchmark"));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("run 3000").font(FontId::monospace(13.0)))
                        .corner_radius(SQ)
                        .min_size(vec2(96.0, 30.0)),
                )
                .clicked()
            {
                let j = Job {
                    mode: Mode::Benchmark,
                    limit: Some(3000),
                    ..Job::click(Btn::Left, self.bench_target, 0.0)
                };
                self.engine.run(j, Opts::default());
            }
            ui.add(
                egui::Slider::new(&mut self.bench_target, 10.0..=20_000.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("target {v:.0} cps")),
            );
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            t::readout(
                ui,
                "sustained",
                &format!("{:.0}", snap.cps(snap.mean_ms)),
                "cps",
                t::LIVE,
                24.0,
            );
            ui.add_space(18.0);
            t::readout(
                ui,
                "worst stall",
                &format!("{:.2}", snap.max_ms),
                "ms",
                t::INK,
                24.0,
            );
            ui.add_space(18.0);
            t::readout(
                ui,
                "cpu",
                &format!("{:.0}", snap.spin_pct),
                "% core",
                if snap.spin_pct > 60.0 {
                    t::WARN
                } else {
                    t::GOOD
                },
                24.0,
            );
        });
    }

    // -------------------------------------------------------------- devices

    fn devices(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(t::caption("attached input devices"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("rescan").clicked() {
                    self.raw.devices = crate::rawinput::enumerate();
                }
                ui.label(
                    RichText::new(if self.raw.capturing() {
                        "capture on"
                    } else {
                        "capture off"
                    })
                    .size(10.0)
                    .color(if self.raw.capturing() {
                        t::LIVE
                    } else {
                        t::FAINT
                    }),
                );
            });
        });
        ui.add_space(6.0);
        // reg_ok in the thousands means registration is looping -- that bug cost
        // days, so the counter stays visible.
        ui.label(
            RichText::new(crate::rawinput::diag())
                .font(FontId::monospace(10.0))
                .color(t::DIM),
        );
        ui.add_space(6.0);
        t::rule(ui);
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Buttons that log nothing here send nothing to Windows at all — DPI, LED and \
                 rapid-fire buttons are executed inside the mouse's own firmware, so no third-party \
                 software can bind them.",
            )
            .size(10.5)
            .color(t::FAINT),
        );
        ui.add_space(8.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            if !self.learn_log.is_empty() {
                ui.label(t::caption("live signal log"));
                ui.add_space(3.0);
                for (s, down) in self.learn_log.iter().rev().take(12) {
                    ui.label(
                        RichText::new(format!(
                            "{:<30} {}",
                            s.label(),
                            if *down { "DOWN" } else { "UP" }
                        ))
                        .font(FontId::monospace(11.0))
                        .color(if *down { t::LIVE } else { t::DIM }),
                    );
                }
                ui.add_space(10.0);
            }
            let devs = self.raw.devices.clone();
            for d in devs.iter().filter(|d| d.kind == "Mouse") {
                device_row(ui, d);
            }
            ui.add_space(8.0);
            ui.label(t::caption("other collections"));
            ui.add_space(3.0);
            for d in devs.iter().filter(|d| d.kind != "Mouse") {
                device_row(ui, d);
            }
        });
    }
}

fn device_row(ui: &mut Ui, d: &crate::rawinput::Device) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{:<22}", d.label()))
                .font(FontId::monospace(11.5))
                .color(t::INK),
        );
        let detail = match d.kind {
            "Mouse" => format!("{} buttons", d.buttons),
            "Keyboard" => format!("{} keys", d.buttons),
            _ => format!("page {:#06x} usage {:#06x}", d.usage_page, d.usage),
        };
        ui.label(
            RichText::new(detail)
                .font(FontId::monospace(11.0))
                .color(t::DIM),
        );
    });
    ui.label(
        RichText::new(d.path.chars().take(78).collect::<String>())
            .size(9.5)
            .color(t::FAINT),
    );
    ui.add_space(3.0);
}

/// Log-scaled period histogram, drawn by hand: full control, no extra crate.
fn hist(ui: &mut Ui, snap: &Snap, target_ms: f64) {
    let h = 150.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, SQ, t::SUNK);
    p.rect_stroke(rect, SQ, Stroke::new(1.0, t::RULE_SOFT), StrokeKind::Inside);

    let lo = 10_000.0f64;
    let hi = 100_000_000.0f64;
    let frac_of = |ns: f64| ((ns / lo).ln() / (hi / lo).ln()) as f32;
    for (ns, lbl) in [
        (10_000.0, "10us"),
        (100_000.0, "100us"),
        (1_000_000.0, "1ms"),
        (10_000_000.0, "10ms"),
        (100_000_000.0, "100ms"),
    ] {
        let x = rect.left() + frac_of(ns) * rect.width();
        p.vline(x, rect.y_range(), Stroke::new(1.0, t::RULE_SOFT));
        p.text(
            pos2(x + 3.0, rect.bottom() - 13.0),
            egui::Align2::LEFT_TOP,
            lbl,
            FontId::monospace(9.0),
            t::FAINT,
        );
    }
    let maxc = snap.hist.iter().copied().max().unwrap_or(1).max(1) as f32;
    let bw = rect.width() / NB as f32;
    for i in 0..NB {
        let c = snap.hist[i];
        if c == 0 {
            continue;
        }
        let bh = ((c as f32 / maxc).sqrt() * (h - 20.0)).max(1.0);
        let x = rect.left() + i as f32 * bw;
        let colour = if bucket_ns(i) > target_ms * 1e6 * 2.0 {
            t::WARN
        } else {
            t::LIVE
        };
        p.rect_filled(
            Rect::from_min_size(pos2(x, rect.bottom() - bh), vec2(bw.max(1.0), bh)),
            SQ,
            colour,
        );
    }
    let tx = rect.left() + frac_of(target_ms * 1e6) * rect.width();
    p.vline(tx, rect.y_range(), Stroke::new(1.0, t::INK));
    if snap.total == 0 {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no samples yet",
            FontId::monospace(11.0),
            t::FAINT,
        );
    }
}

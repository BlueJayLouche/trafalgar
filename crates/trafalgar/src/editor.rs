// Vizia editor: four track columns side by side. Each column = an XY performance
// pad (X = pitch/probability, Y = density) with a live euclidean step display,
// plus that track's mode/hold buttons and param sliders. A 30fps timer drives
// redraws so the playheads animate.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use nih_plug::prelude::{Editor, Param};
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg;
use nih_plug_vizia::widgets::param_base::ParamWidgetBase;
use nih_plug_vizia::widgets::{ParamButton, ParamSlider};
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};

use crate::{euclid, rotated, OscSettings, Shared, TrafalgarParams, NUM_TRACKS, PITCH_RANGE, STEPS};

#[derive(Lens)]
struct Data {
    params: Arc<TrafalgarParams>,
    settings_open: bool,
    /// Mirror of `shared.osc_in_status`, refreshed by the redraw timer so the
    /// settings panel can show a live bind status (0 off, 1 listening, 2 failed).
    osc_in_status: u8,
}

enum AppEvent {
    ToggleSettings,
    SetOscInStatus(u8),
}

impl Model for Data {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|e, _| match e {
            AppEvent::ToggleSettings => self.settings_open = !self.settings_open,
            AppEvent::SetOscInStatus(s) => self.osc_in_status = *s,
        });
    }
}

/// Editable buffer for the OSC settings popout. Write-through: any edit updates
/// the persisted `OscSettings` and flags `osc_dirty` so the audio thread rebuilds.
#[derive(Lens)]
struct SettingsData {
    #[lens(ignore)]
    settings: Arc<std::sync::RwLock<OscSettings>>,
    #[lens(ignore)]
    shared: Arc<Shared>,
    enabled: bool,
    host: String,
    port: String,
    in_enabled: bool,
    in_port: String,
    in_lan: bool,
}

enum SettingsEvent {
    ToggleEnabled,
    SetHost(String),
    SetPort(String),
    ToggleInEnabled,
    SetInPort(String),
    ToggleInLan,
}

impl Model for SettingsData {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|e, _| {
            // Which side changed — so we only rebuild the affected socket.
            let mut out_changed = false;
            let mut in_changed = false;
            match e {
                SettingsEvent::ToggleEnabled => { self.enabled = !self.enabled; out_changed = true; }
                SettingsEvent::SetHost(h) => { self.host = h.clone(); out_changed = true; }
                SettingsEvent::SetPort(p) => { self.port = p.clone(); out_changed = true; }
                SettingsEvent::ToggleInEnabled => { self.in_enabled = !self.in_enabled; in_changed = true; }
                SettingsEvent::SetInPort(p) => { self.in_port = p.clone(); in_changed = true; }
                SettingsEvent::ToggleInLan => { self.in_lan = !self.in_lan; in_changed = true; }
            }
            let mut s = self.settings.write().unwrap();
            s.enabled = self.enabled;
            s.host = self.host.clone();
            if let Ok(p) = self.port.trim().parse() {
                s.port = p;
            }
            s.in_enabled = self.in_enabled;
            s.in_lan = self.in_lan;
            if let Ok(p) = self.in_port.trim().parse() {
                s.in_port = p;
            }
            drop(s);
            if out_changed {
                self.shared.osc_dirty.store(true, Ordering::Relaxed);
            }
            if in_changed {
                self.shared.osc_in_dirty.store(true, Ordering::Relaxed);
            }
        });
    }
}

/// Undo history: (track, snapshot of that track's whole gesture buffer before a
/// change). GUI-thread only, so Rc<RefCell> is fine. Transient across editor reopen.
type UndoStack = Rc<RefCell<Vec<(usize, Vec<i32>)>>>;
const UNDO_LIMIT: usize = 64;

fn snapshot(shared: &Shared, track: usize) -> Vec<i32> {
    shared.gesture[track].iter().map(|c| c.load(Ordering::Relaxed)).collect()
}

fn push_undo(undo: &UndoStack, shared: &Shared, track: usize) {
    let mut u = undo.borrow_mut();
    u.push((track, snapshot(shared, track)));
    if u.len() > UNDO_LIMIT {
        u.remove(0);
    }
}

pub fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (820, 720))
}

pub fn create(
    params: Arc<TrafalgarParams>,
    shared: Arc<Shared>,
    state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_light(cx);
        Data { params: params.clone(), settings_open: false, osc_in_status: 0 }.build(cx);

        // Undo history lives here, shared among this build's widgets (GUI thread).
        let undo: UndoStack = Rc::new(RefCell::new(Vec::new()));

        // ~30fps redraw so the euclidean step heads animate; also poll the OSC-in
        // bind status (set on the audio thread) and push it to the model on change.
        let status_shared = shared.clone();
        let last_status = Cell::new(u8::MAX);
        let timer = cx.add_timer(Duration::from_millis(33), None, move |cx, action| {
            if matches!(action, TimerAction::Tick(_)) {
                let s = status_shared.osc_in_status.load(Ordering::Relaxed);
                if s != last_status.get() {
                    last_status.set(s);
                    cx.emit(AppEvent::SetOscInStatus(s));
                }
                cx.needs_redraw();
            }
        });
        cx.start_timer(timer);

        VStack::new(cx, |cx| {
            HStack::new(cx, |cx| {
                Label::new(cx, "TRAFALGAR").font_size(22.0);
                UndoButton::new(cx, shared.clone(), undo.clone());
                Button::new(cx, |cx| cx.emit(AppEvent::ToggleSettings), |cx| Label::new(cx, "Settings"))
                    .height(Pixels(22.0));
            })
            .col_between(Pixels(12.0))
            .top(Pixels(4.0))
            .height(Auto);
            HStack::new(cx, |cx| {
                for i in 0..NUM_TRACKS {
                    track_column(cx, i, params.clone(), shared.clone(), undo.clone());
                }
            })
            .col_between(Pixels(8.0))
            .width(Auto) // pack columns at their fixed width, don't stretch into neighbours
            .height(Auto);
        })
        .child_space(Pixels(10.0))
        .row_between(Pixels(8.0));

        // OSC settings popout, drawn over everything when open.
        let (sp, ss) = (params.clone(), shared.clone());
        Binding::new(cx, Data::settings_open, move |cx, open| {
            if open.get(cx) {
                settings_panel(cx, sp.clone(), ss.clone());
            }
        });
    })
}

fn track_column(cx: &mut Context, i: usize, params: Arc<TrafalgarParams>, shared: Arc<Shared>, undo: UndoStack) {
    const NAMES: [&str; NUM_TRACKS] = ["TRACK 1", "TRACK 2", "TRACK 3", "TRACK 4"];
    VStack::new(cx, |cx| {
        Label::new(cx, NAMES[i]).font_size(13.0);

        XyPad::new(cx, Data::params, params, shared.clone(), undo.clone(), i)
            .width(Stretch(1.0))
            .height(Pixels(150.0));

        HStack::new(cx, |cx| {
            ParamButton::new(cx, Data::params, move |p| &p.tracks[i].hold);
            ParamButton::new(cx, Data::params, move |p| &p.tracks[i].percussive);
        })
        .col_between(Pixels(4.0))
        .height(Auto);
        HStack::new(cx, |cx| {
            ParamButton::new(cx, Data::params, move |p| &p.tracks[i].record);
            HoldButton::new(cx, shared.clone(), undo.clone(), i, "Erase"); // momentary: erases only while held
            ClearButton::new(cx, shared.clone(), undo.clone(), i); // one-shot: wipe the whole loop
        })
        .col_between(Pixels(4.0))
        .height(Auto);
        slider_row(cx, "Scale", move |p| &p.tracks[i].scale);
        // Len slider with a small Portamento toggle tucked on the end.
        VStack::new(cx, |cx| {
            Label::new(cx, "Len").font_size(10.0);
            HStack::new(cx, |cx| {
                ParamSlider::new(cx, Data::params, move |p| &p.tracks[i].note_len)
                    .height(Pixels(20.0))
                    .width(Stretch(1.0));
                ParamButton::new(cx, Data::params, move |p| &p.tracks[i].portamento)
                    .height(Pixels(20.0))
                    .width(Pixels(58.0));
            })
            .col_between(Pixels(4.0))
            .height(Auto);
        })
        .width(Stretch(1.0))
        .height(Auto)
        .row_between(Pixels(1.0));
        slider_row(cx, "Bars", move |p| &p.tracks[i].length);
        slider_row(cx, "Rot", move |p| &p.tracks[i].rotation);
        slider_row(cx, "Accent", move |p| &p.tracks[i].accent);
        slider_row(cx, "Vel", move |p| &p.tracks[i].base_vel);
        slider_row(cx, "Acc lvl", move |p| &p.tracks[i].accent_vel);
        slider_row(cx, "Note", move |p| &p.tracks[i].note);
    })
    .width(Pixels(185.0))
    .row_between(Pixels(3.0));
}

/// OSC settings popout: an overlay panel over the editor. Edits write through to
/// the persisted `OscSettings` and flag the audio thread to rebuild the sender.
fn settings_panel(cx: &mut Context, params: Arc<TrafalgarParams>, shared: Arc<Shared>) {
    let cfg = params.osc.read().unwrap().clone();
    VStack::new(cx, |cx| {
        SettingsData {
            settings: params.osc.clone(),
            shared,
            enabled: cfg.enabled,
            host: cfg.host,
            port: cfg.port.to_string(),
            in_enabled: cfg.in_enabled,
            in_port: cfg.in_port.to_string(),
            in_lan: cfg.in_lan,
        }
        .build(cx);

        Label::new(cx, "SETTINGS").font_size(16.0);

        Label::new(cx, "OSC output").font_size(11.0).top(Pixels(6.0));
        HStack::new(cx, |cx| {
            Checkbox::new(cx, SettingsData::enabled).on_toggle(|cx| cx.emit(SettingsEvent::ToggleEnabled));
            Label::new(cx, "Enabled").hoverable(false);
        })
        .col_between(Pixels(6.0))
        .height(Auto);
        settings_field(cx, "Host", SettingsData::host, |cx, t, _| cx.emit(SettingsEvent::SetHost(t)));
        settings_field(cx, "Port", SettingsData::port, |cx, t, _| cx.emit(SettingsEvent::SetPort(t)));

        Label::new(cx, "OSC input (remote pad)").font_size(11.0).top(Pixels(10.0));
        HStack::new(cx, |cx| {
            Checkbox::new(cx, SettingsData::in_enabled).on_toggle(|cx| cx.emit(SettingsEvent::ToggleInEnabled));
            Label::new(cx, "Enabled").hoverable(false);
        })
        .col_between(Pixels(6.0))
        .height(Auto);
        settings_field(cx, "Port", SettingsData::in_port, |cx, t, _| cx.emit(SettingsEvent::SetInPort(t)));
        HStack::new(cx, |cx| {
            Checkbox::new(cx, SettingsData::in_lan).on_toggle(|cx| cx.emit(SettingsEvent::ToggleInLan));
            Label::new(cx, "LAN access (uncheck = loopback only)").hoverable(false);
        })
        .col_between(Pixels(6.0))
        .height(Auto);
        // Live bind status from the audio thread (0 off, 1 listening, 2 failed).
        // Colours chosen for contrast on the light panel background.
        Binding::new(cx, Data::osc_in_status, |cx, s| {
            let (text, col) = match s.get(cx) {
                1 => ("listening", Color::rgb(20, 130, 60)),
                2 => ("port in use — OSC-in off", Color::rgb(190, 40, 30)),
                _ => ("off", Color::rgb(120, 120, 120)),
            };
            Label::new(cx, text).font_size(10.0).color(col);
        });

        Label::new(cx, "MIDI").font_size(11.0).top(Pixels(10.0));
        Label::new(cx, "Output goes to the host / virtual port. Input and device\nare chosen at launch (--midi-input / --midi-output).")
            .font_size(10.0);

        Button::new(cx, |cx| cx.emit(AppEvent::ToggleSettings), |cx| Label::new(cx, "Close"))
            .top(Pixels(12.0))
            .height(Pixels(24.0));
    })
    .position_type(PositionType::SelfDirected)
    .left(Pixels(20.0))
    .top(Pixels(40.0))
    .width(Pixels(320.0))
    .height(Auto)
    .child_space(Pixels(12.0))
    .row_between(Pixels(4.0))
    // Light panel to match the app theme so the (dark) default label text is readable.
    .background_color(Color::rgb(238, 238, 242))
    .border_color(Color::rgb(150, 150, 160))
    .border_width(Pixels(1.0));
}

/// A labelled text field row for the settings panel.
fn settings_field<F>(cx: &mut Context, label: &'static str, lens: impl Lens<Target = String>, on_submit: F)
where
    F: 'static + Fn(&mut EventContext, String, bool) + Send + Sync,
{
    HStack::new(cx, |cx| {
        Label::new(cx, label).width(Pixels(44.0));
        Textbox::new(cx, lens).on_submit(on_submit).width(Stretch(1.0)).height(Pixels(22.0));
    })
    .col_between(Pixels(6.0))
    .height(Auto);
}

/// A labelled slider: a small name above the slider.
fn slider_row<P, F>(cx: &mut Context, label: &'static str, f: F)
where
    P: Param + 'static,
    F: Fn(&Arc<TrafalgarParams>) -> &P + Copy + 'static,
{
    VStack::new(cx, |cx| {
        Label::new(cx, label).font_size(10.0);
        ParamSlider::new(cx, Data::params, f).height(Pixels(20.0)).width(Stretch(1.0));
    })
    .width(Stretch(1.0))
    .height(Auto)
    .row_between(Pixels(1.0));
}

/// A momentary button: sets `shared.erase[track]` true only while held (press +
/// capture, cleared on release). Used for Figure-style scrub erase.
pub struct HoldButton {
    shared: Arc<Shared>,
    undo: UndoStack,
    track: usize,
}

impl HoldButton {
    pub fn new<'a>(cx: &'a mut Context, shared: Arc<Shared>, undo: UndoStack, track: usize, label: &'static str) -> Handle<'a, Self> {
        Self { shared, undo, track }
            .build(cx, |cx| {
                Label::new(cx, label).hoverable(false);
            })
            .height(Pixels(24.0))
            .width(Stretch(1.0))
            .child_space(Stretch(1.0))
            .background_color(Color::rgb(72, 46, 46))
    }
}

impl View for HoldButton {
    fn element(&self) -> Option<&'static str> {
        Some("hold-button")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match *window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                push_undo(&self.undo, &self.shared, self.track); // snapshot before erasing
                self.shared.erase[self.track].store(true, Ordering::Relaxed);
                cx.capture();
                meta.consume();
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                self.shared.erase[self.track].store(false, Ordering::Relaxed);
                cx.release();
                meta.consume();
            }
            _ => {}
        });
    }
}

/// One-shot Clear: wipes this track's entire recorded loop (undoable).
pub struct ClearButton {
    shared: Arc<Shared>,
    undo: UndoStack,
    track: usize,
}

impl ClearButton {
    pub fn new<'a>(cx: &'a mut Context, shared: Arc<Shared>, undo: UndoStack, track: usize) -> Handle<'a, Self> {
        Self { shared, undo, track }
            .build(cx, |cx| {
                Label::new(cx, "Clear").hoverable(false);
            })
            .height(Pixels(24.0))
            .width(Stretch(1.0))
            .child_space(Stretch(1.0))
            .background_color(Color::rgb(72, 46, 46))
    }
}

impl View for ClearButton {
    fn element(&self) -> Option<&'static str> {
        Some("clear-button")
    }

    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| {
            if let WindowEvent::MouseDown(MouseButton::Left) = *window_event {
                push_undo(&self.undo, &self.shared, self.track); // snapshot before wiping
                for c in self.shared.gesture[self.track].iter() {
                    c.store(-1, Ordering::Relaxed);
                }
                meta.consume();
            }
        });
    }
}

/// Global undo: restores the most recently changed track's loop from history.
pub struct UndoButton {
    shared: Arc<Shared>,
    undo: UndoStack,
}

impl UndoButton {
    pub fn new<'a>(cx: &'a mut Context, shared: Arc<Shared>, undo: UndoStack) -> Handle<'a, Self> {
        Self { shared, undo }
            .build(cx, |cx| {
                Label::new(cx, "Undo").hoverable(false);
            })
            .height(Pixels(22.0))
            .width(Pixels(64.0))
            .child_space(Stretch(1.0))
            .background_color(Color::rgb(52, 52, 60))
    }
}

impl View for UndoButton {
    fn element(&self) -> Option<&'static str> {
        Some("undo-button")
    }

    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| {
            if let WindowEvent::MouseDown(MouseButton::Left) = *window_event {
                if let Some((track, snap)) = self.undo.borrow_mut().pop() {
                    for (c, &v) in self.shared.gesture[track].iter().zip(&snap) {
                        c.store(v, Ordering::Relaxed);
                    }
                }
                meta.consume();
            }
        });
    }
}

/// Two-parameter performance pad for one track. X drives pitch, Y drives density.
/// Draws the live euclidean pattern with the playhead and an accent ring.
pub struct XyPad {
    x: ParamWidgetBase,           // pitch (write)
    y: ParamWidgetBase,           // density (write)
    params: Arc<TrafalgarParams>, // read-only, for drawing
    shared: Arc<Shared>,          // gate (GUI->audio) + step (audio->GUI)
    undo: UndoStack,
    track: usize,
    drag: bool,
}

impl XyPad {
    pub fn new<'a, L>(
        cx: &'a mut Context,
        lens: L,
        params: Arc<TrafalgarParams>,
        shared: Arc<Shared>,
        undo: UndoStack,
        track: usize,
    ) -> Handle<'a, Self>
    where
        L: Lens<Target = Arc<TrafalgarParams>> + Clone,
    {
        Self {
            x: ParamWidgetBase::new(cx, lens.clone(), move |p| &p.tracks[track].pitch),
            y: ParamWidgetBase::new(cx, lens, move |p| &p.tracks[track].density),
            params,
            shared,
            undo,
            track,
            drag: false,
        }
        .build(cx, |_| {})
    }

    fn apply(&self, cx: &mut EventContext, mx: f32, my: f32) {
        let b = cx.bounds();
        if b.w == 0.0 || b.h == 0.0 {
            return;
        }
        let nx = ((mx - b.x) / b.w).clamp(0.0, 1.0);
        let dnorm = 1.0 - ((my - b.y) / b.h).clamp(0.0, 1.0); // Y up = denser
        // Instant position for the audio thread (see Shared::pos). Store before the
        // gate opens so the two are seen together.
        let packed = ((nx.to_bits() as u64) << 32) | dnorm.to_bits() as u64;
        self.shared.pos[self.track].store(packed, Ordering::Relaxed);
        self.x.set_normalized_value(cx, nx); // X = pitch
        self.y.set_normalized_value(cx, dnorm);
    }
}

impl View for XyPad {
    fn element(&self) -> Option<&'static str> {
        Some("xy-pad")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match *window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.drag = true;
                // Snapshot the loop before a record pass so it can be undone.
                if self.params.tracks[self.track].record.value() {
                    push_undo(&self.undo, &self.shared, self.track);
                }
                cx.capture();
                self.x.begin_set_parameter(cx);
                self.y.begin_set_parameter(cx);
                let (mx, my) = (cx.mouse().cursorx, cx.mouse().cursory);
                self.apply(cx, mx, my); // writes position atomic first...
                self.shared.gate[self.track].store(true, Ordering::Release); // ...then opens the gate
                meta.consume();
            }
            WindowEvent::MouseMove(x, y) => {
                if self.drag {
                    self.apply(cx, x, y);
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.drag {
                    self.drag = false;
                    self.shared.gate[self.track].store(false, Ordering::Release);
                    self.x.end_set_parameter(cx);
                    self.y.end_set_parameter(cx);
                    cx.release();
                    meta.consume();
                }
            }
            _ => {}
        });
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let b = cx.bounds();
        if b.w == 0.0 || b.h == 0.0 {
            return;
        }
        let p = &self.params.tracks[self.track];

        // background
        let mut bg = vg::Path::new();
        bg.rect(b.x, b.y, b.w, b.h);
        canvas.fill_path(&bg, &vg::Paint::color(vg::Color::rgb(24, 24, 28)));

        // live euclidean pattern along the bottom
        let pattern = rotated(p.density.value() as usize, STEPS, p.rotation.value() as usize);
        let accents = euclid(p.accent.value() as usize, STEPS);
        let cur = self.shared.step[self.track].load(Ordering::Relaxed); // absolute step, -1 idle
        let accent_col = vg::Color::rgb(255, 120, 80);
        for s in 0..STEPS {
            let x = b.x + b.w * (s as f32 + 0.5) / STEPS as f32;
            let y = b.bottom() - 14.0;
            let is_cur = cur >= 0 && cur.rem_euclid(STEPS as i64) == s as i64;
            let (r, col) = match (pattern[s], is_cur) {
                (true, true) => (6.0, accent_col),
                (true, false) => (4.0, vg::Color::rgb(200, 200, 200)),
                (false, true) => (4.0, vg::Color::rgb(120, 60, 40)),
                (false, false) => (2.0, vg::Color::rgb(70, 70, 70)),
            };
            let mut dot = vg::Path::new();
            dot.circle(x, y, r);
            canvas.fill_path(&dot, &vg::Paint::color(col));
            if pattern[s] && accents[s] {
                let mut ring = vg::Path::new();
                ring.circle(x, y, r + 3.0);
                canvas.stroke_path(&ring, &vg::Paint::color(accent_col).with_line_width(1.5));
            }
        }

        // crosshair derived from the actual pitch/density params (tracks automation)
        let nx = p.pitch.value() as f32 / PITCH_RANGE as f32;
        let ny = 1.0 - (p.density.value() - 1) as f32 / (STEPS as f32 - 1.0);
        let px = b.x + nx * b.w;
        let py = b.y + ny * b.h;

        let mut cross = vg::Path::new();
        cross.move_to(b.x, py);
        cross.line_to(b.right(), py);
        cross.move_to(px, b.y);
        cross.line_to(px, b.bottom());
        canvas.stroke_path(
            &cross,
            &vg::Paint::color(vg::Color::rgb(70, 70, 80)).with_line_width(1.0),
        );

        let mut ring = vg::Path::new();
        ring.circle(px, py, 7.0);
        canvas.stroke_path(&ring, &vg::Paint::color(accent_col).with_line_width(2.0));

        // recorded gesture loop, spanning the full record length: a green dot per
        // recorded step (height = pitch), the one at the playhead drawn brighter.
        let loop_steps = self.params.tracks[self.track].length.value() as usize * STEPS;
        let gcur = if cur >= 0 { cur.rem_euclid(loop_steps as i64) } else { -1 };
        let rec_col = vg::Color::rgb(90, 210, 140);
        let rec_cur = vg::Color::rgb(150, 255, 190);
        for s in 0..loop_steps {
            let v = self.shared.gesture[self.track][s].load(Ordering::Relaxed);
            if v < 0 {
                continue;
            }
            let gx = b.x + b.w * (s as f32 + 0.5) / loop_steps as f32;
            let gy = b.y + (1.0 - v as f32 / 127.0) * b.h; // v is a MIDI note (0..127)
            let (r, col) = if gcur == s as i64 { (4.0, rec_cur) } else { (3.0, rec_col) };
            let mut d = vg::Path::new();
            d.circle(gx, gy, r);
            canvas.fill_path(&d, &vg::Paint::color(col));
        }
    }
}

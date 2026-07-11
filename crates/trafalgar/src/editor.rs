// Vizia editor: four track columns side by side. Each column = an XY performance
// pad (X = pitch/probability, Y = density) with a live euclidean step display,
// plus that track's mode/hold buttons and param sliders. A 30fps timer drives
// redraws so the playheads animate.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use nih_plug::prelude::{Editor, Param};
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg;
use nih_plug_vizia::widgets::param_base::ParamWidgetBase;
use nih_plug_vizia::widgets::{ParamButton, ParamSlider};
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};

use crate::{euclid, rotated, Shared, TrafalgarParams, NUM_TRACKS, PITCH_RANGE, STEPS};

#[derive(Lens)]
struct Data {
    params: Arc<TrafalgarParams>,
}
impl Model for Data {}

pub fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (700, 560))
}

pub fn create(
    params: Arc<TrafalgarParams>,
    shared: Arc<Shared>,
    state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_light(cx);
        Data { params: params.clone() }.build(cx);

        // ~30fps redraw so the euclidean step heads animate.
        let timer = cx.add_timer(Duration::from_millis(33), None, |cx, action| {
            if matches!(action, TimerAction::Tick(_)) {
                cx.needs_redraw();
            }
        });
        cx.start_timer(timer);

        VStack::new(cx, |cx| {
            Label::new(cx, "TRAFALGAR").font_size(22.0).top(Pixels(4.0));
            HStack::new(cx, |cx| {
                for i in 0..NUM_TRACKS {
                    track_column(cx, i, params.clone(), shared.clone());
                }
            })
            .col_between(Pixels(8.0))
            .height(Auto);
        })
        .child_space(Pixels(10.0))
        .row_between(Pixels(8.0));
    })
}

fn track_column(cx: &mut Context, i: usize, params: Arc<TrafalgarParams>, shared: Arc<Shared>) {
    const NAMES: [&str; NUM_TRACKS] = ["TRACK 1", "TRACK 2", "TRACK 3", "TRACK 4"];
    VStack::new(cx, |cx| {
        Label::new(cx, NAMES[i]).font_size(13.0);

        XyPad::new(cx, Data::params, params, shared, i)
            .width(Pixels(150.0))
            .height(Pixels(150.0));

        ParamButton::new(cx, Data::params, move |p| &p.tracks[i].hold);
        ParamButton::new(cx, Data::params, move |p| &p.tracks[i].percussive);
        slider_row(cx, "Rot", move |p| &p.tracks[i].rotation);
        slider_row(cx, "Accent", move |p| &p.tracks[i].accent);
        slider_row(cx, "Vel", move |p| &p.tracks[i].base_vel);
        slider_row(cx, "Acc lvl", move |p| &p.tracks[i].accent_vel);
        slider_row(cx, "Note", move |p| &p.tracks[i].note);
    })
    .width(Pixels(160.0))
    .row_between(Pixels(3.0));
}

/// A labelled slider: a small name above the slider.
fn slider_row<P, F>(cx: &mut Context, label: &'static str, f: F)
where
    P: Param + 'static,
    F: Fn(&Arc<TrafalgarParams>) -> &P + Copy + 'static,
{
    VStack::new(cx, |cx| {
        Label::new(cx, label).font_size(10.0);
        ParamSlider::new(cx, Data::params, f).height(Pixels(20.0));
    })
    .height(Auto)
    .row_between(Pixels(1.0));
}

/// Two-parameter performance pad for one track. X drives pitch, Y drives density.
/// Draws the live euclidean pattern with the playhead and an accent ring.
pub struct XyPad {
    x: ParamWidgetBase,           // pitch (write)
    y: ParamWidgetBase,           // density (write)
    params: Arc<TrafalgarParams>, // read-only, for drawing
    shared: Arc<Shared>,          // gate (GUI->audio) + step (audio->GUI)
    track: usize,
    drag: bool,
}

impl XyPad {
    pub fn new<L>(
        cx: &mut Context,
        lens: L,
        params: Arc<TrafalgarParams>,
        shared: Arc<Shared>,
        track: usize,
    ) -> Handle<Self>
    where
        L: Lens<Target = Arc<TrafalgarParams>> + Clone,
    {
        Self {
            x: ParamWidgetBase::new(cx, lens.clone(), move |p| &p.tracks[track].pitch),
            y: ParamWidgetBase::new(cx, lens, move |p| &p.tracks[track].density),
            params,
            shared,
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
        let cur = self.shared.step[self.track].load(Ordering::Relaxed);
        let accent_col = vg::Color::rgb(255, 120, 80);
        for s in 0..STEPS {
            let x = b.x + b.w * (s as f32 + 0.5) / STEPS as f32;
            let y = b.bottom() - 14.0;
            let is_cur = cur == s as i64;
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
    }
}

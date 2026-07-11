// Vizia editor: the XY performance pad (X = pitch keybed, Y = density) with a live
// euclidean step display, plus stock param widgets. One track for now. The pad is
// the only custom widget; a 30fps timer drives redraws so the playhead animates.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nih_plug::prelude::Editor;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg;
use nih_plug_vizia::widgets::param_base::ParamWidgetBase;
use nih_plug_vizia::widgets::{ParamButton, ParamSlider};
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};

use crate::{euclid, rotated, TrafalgarParams, PITCH_RANGE, STEPS};

#[derive(Lens)]
struct Data {
    params: Arc<TrafalgarParams>,
}
impl Model for Data {}

pub fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (300, 480))
}

pub fn create(
    params: Arc<TrafalgarParams>,
    gate: Arc<AtomicBool>,
    step: Arc<AtomicI64>,
    state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_light(cx);
        Data { params: params.clone() }.build(cx);

        // ~30fps redraw so the euclidean step head animates.
        let timer = cx.add_timer(Duration::from_millis(33), None, |cx, action| {
            if matches!(action, TimerAction::Tick(_)) {
                cx.needs_redraw();
            }
        });
        cx.start_timer(timer);

        VStack::new(cx, |cx| {
            Label::new(cx, "TRAFALGAR").font_size(22.0).top(Pixels(4.0));

            XyPad::new(cx, Data::params, params.clone(), gate.clone(), step.clone())
                .width(Pixels(260.0))
                .height(Pixels(260.0));

            ParamButton::new(cx, Data::params, |p| &p.hold);
            Label::new(cx, "Rotation");
            ParamSlider::new(cx, Data::params, |p| &p.rotation);
            Label::new(cx, "Accent");
            ParamSlider::new(cx, Data::params, |p| &p.accent);
            Label::new(cx, "Velocity");
            ParamSlider::new(cx, Data::params, |p| &p.base_vel);
            Label::new(cx, "Accent level");
            ParamSlider::new(cx, Data::params, |p| &p.accent_vel);
        })
        .child_space(Pixels(10.0))
        .row_between(Pixels(6.0));
    })
}

/// Two-parameter performance pad. X drives pitch, Y drives density (up = denser).
/// Draws the live euclidean pattern with the playhead and an accent ring.
pub struct XyPad {
    x: ParamWidgetBase,           // pitch (write)
    y: ParamWidgetBase,           // density (write)
    params: Arc<TrafalgarParams>, // read-only, for drawing
    gate: Arc<AtomicBool>,        // pad touch state -> audio thread
    step: Arc<AtomicI64>,         // playhead step from audio thread (-1 = idle)
    drag: bool,
}

impl XyPad {
    pub fn new<L>(
        cx: &mut Context,
        lens: L,
        params: Arc<TrafalgarParams>,
        gate: Arc<AtomicBool>,
        step: Arc<AtomicI64>,
    ) -> Handle<Self>
    where
        L: Lens<Target = Arc<TrafalgarParams>> + Clone,
    {
        Self {
            x: ParamWidgetBase::new(cx, lens.clone(), |p| &p.pitch),
            y: ParamWidgetBase::new(cx, lens, |p| &p.density),
            params,
            gate,
            step,
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
        let ny = ((my - b.y) / b.h).clamp(0.0, 1.0);
        self.x.set_normalized_value(cx, nx); // X = pitch
        self.y.set_normalized_value(cx, 1.0 - ny); // Y up = denser
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
                self.gate.store(true, Ordering::Relaxed);
                cx.capture();
                self.x.begin_set_parameter(cx);
                self.y.begin_set_parameter(cx);
                let (mx, my) = (cx.mouse().cursorx, cx.mouse().cursory);
                self.apply(cx, mx, my);
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
                    self.gate.store(false, Ordering::Relaxed);
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

        // background
        let mut bg = vg::Path::new();
        bg.rect(b.x, b.y, b.w, b.h);
        canvas.fill_path(&bg, &vg::Paint::color(vg::Color::rgb(24, 24, 28)));

        // live euclidean pattern along the bottom
        let density = self.params.density.value() as usize;
        let rotation = self.params.rotation.value() as usize;
        let pattern = rotated(density, STEPS, rotation);
        let accents = euclid(self.params.accent.value() as usize, STEPS);
        let cur = self.step.load(Ordering::Relaxed);
        let accent_col = vg::Color::rgb(255, 120, 80);
        for s in 0..STEPS {
            let x = b.x + b.w * (s as f32 + 0.5) / STEPS as f32;
            let y = b.bottom() - 16.0;
            let is_cur = cur == s as i64;
            let (r, col) = match (pattern[s], is_cur) {
                (true, true) => (7.0, accent_col),
                (true, false) => (5.0, vg::Color::rgb(200, 200, 200)),
                (false, true) => (5.0, vg::Color::rgb(120, 60, 40)),
                (false, false) => (3.0, vg::Color::rgb(70, 70, 70)),
            };
            let mut dot = vg::Path::new();
            dot.circle(x, y, r);
            canvas.fill_path(&dot, &vg::Paint::color(col));
            // accent ring on accented onsets
            if pattern[s] && accents[s] {
                let mut ring = vg::Path::new();
                ring.circle(x, y, r + 3.0);
                canvas.stroke_path(&ring, &vg::Paint::color(accent_col).with_line_width(1.5));
            }
        }

        // crosshair derived from the actual pitch/density params (tracks automation)
        let nx = self.params.pitch.value() as f32 / PITCH_RANGE as f32;
        let ny = 1.0 - (self.params.density.value() - 1) as f32 / (STEPS as f32 - 1.0);
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
        ring.circle(px, py, 8.0);
        canvas.stroke_path(&ring, &vg::Paint::color(accent_col).with_line_width(2.0));
    }
}

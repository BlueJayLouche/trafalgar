// Vizia editor: the XY performance pad (X = pitch keybed, Y = density) plus stock
// param sliders for rotation / accent / velocities. First real UI cut — one track.
// The pad is the only custom widget; everything else is nih_plug_vizia's ParamSlider.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nih_plug::prelude::Editor;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg;
use nih_plug_vizia::widgets::param_base::ParamWidgetBase;
use nih_plug_vizia::widgets::{ParamButton, ParamSlider};
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};

use crate::TrafalgarParams;

#[derive(Lens)]
struct Data {
    params: Arc<TrafalgarParams>,
}
impl Model for Data {}

pub fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (300, 460))
}

pub fn create(
    params: Arc<TrafalgarParams>,
    gate: Arc<AtomicBool>,
    state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_light(cx);
        Data { params: params.clone() }.build(cx);

        let gate = gate.clone();
        VStack::new(cx, |cx| {
            Label::new(cx, "TRAFALGAR").font_size(22.0).top(Pixels(4.0));

            XyPad::new(cx, Data::params, gate.clone())
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
pub struct XyPad {
    x: ParamWidgetBase, // pitch
    y: ParamWidgetBase, // density
    gate: Arc<AtomicBool>, // pad touch state -> audio thread
    drag: bool,
    pos: Cell<(f32, f32)>, // last normalized (x, y) for the crosshair
}

impl XyPad {
    pub fn new<L>(cx: &mut Context, params: L, gate: Arc<AtomicBool>) -> Handle<Self>
    where
        L: Lens<Target = Arc<TrafalgarParams>> + Clone,
    {
        Self {
            x: ParamWidgetBase::new(cx, params.clone(), |p| &p.pitch),
            y: ParamWidgetBase::new(cx, params, |p| &p.density),
            gate,
            drag: false,
            pos: Cell::new((0.33, 0.2)),
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
        self.pos.set((nx, ny));
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

        let mut bg = vg::Path::new();
        bg.rect(b.x, b.y, b.w, b.h);
        canvas.fill_path(&bg, &vg::Paint::color(vg::Color::rgb(24, 24, 28)));

        let (nx, ny) = self.pos.get();
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

        let mut dot = vg::Path::new();
        dot.circle(px, py, 8.0);
        canvas.stroke_path(
            &dot,
            &vg::Paint::color(vg::Color::rgb(255, 120, 80)).with_line_width(2.0),
        );
    }
}

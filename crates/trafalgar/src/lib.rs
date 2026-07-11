// Trafalgar — euclidean jam sequencer. This first slice ports the validated
// prototype (euclid + rotation + pentatonic pitch) into a real nih-plug plugin
// that sequences off the host transport and emits MIDI. No editor/OSC/multi-track
// yet — those are the next increments. Builds as CLAP, VST3, and standalone.

use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

mod editor;

pub(crate) const STEPS: usize = 16;
const BASE_NOTE: u8 = 48; // C3
pub(crate) const PITCH_RANGE: i32 = 15; // scale degrees the pitch param spans
const PENTATONIC: [u8; 5] = [0, 3, 5, 7, 10]; // minor pentatonic

/// Bjorklund (Bresenham form): `pulses` onsets spread evenly over `steps`,
/// onset on step 0. `(i*pulses) % steps < pulses`.
pub(crate) fn euclid(pulses: usize, steps: usize) -> Vec<bool> {
    if steps == 0 {
        return vec![];
    }
    let pulses = pulses.min(steps);
    (0..steps).map(|i| (i * pulses) % steps < pulses).collect()
}

/// Euclidean pattern rotated right by `rot` steps (rot=0 => onset on step 0).
pub(crate) fn rotated(pulses: usize, steps: usize, rot: usize) -> Vec<bool> {
    if steps == 0 {
        return vec![];
    }
    let base = euclid(pulses, steps);
    let r = rot % steps;
    (0..steps).map(|i| base[(i + steps - r) % steps]).collect()
}

/// Map a scale degree onto a MIDI note in the minor pentatonic.
fn scale_note(degree: u8) -> u8 {
    let oct = (degree / PENTATONIC.len() as u8) as i32;
    let step = PENTATONIC[(degree as usize) % PENTATONIC.len()];
    (BASE_NOTE as i32 + 12 * oct + step as i32).clamp(0, 127) as u8
}

#[derive(Params)]
struct TrafalgarParams {
    #[id = "density"]
    density: IntParam,
    #[id = "rotation"]
    rotation: IntParam,
    #[id = "pitch"]
    pitch: IntParam,
    /// How many steps carry an accent (a second euclidean layer).
    #[id = "accent"]
    accent: IntParam,
    #[id = "basevel"]
    base_vel: FloatParam,
    #[id = "accentvel"]
    accent_vel: FloatParam,
    /// Hold on = latched/continuous. Hold off = only sounds while dragging the pad.
    #[id = "hold"]
    hold: BoolParam,

    #[persist = "editor-state"]
    editor_state: Arc<ViziaState>,
}

impl Default for TrafalgarParams {
    fn default() -> Self {
        Self {
            density: IntParam::new("Density", 4, IntRange::Linear { min: 1, max: STEPS as i32 }),
            rotation: IntParam::new("Rotation", 0, IntRange::Linear { min: 0, max: STEPS as i32 - 1 }),
            pitch: IntParam::new("Pitch", 5, IntRange::Linear { min: 0, max: PITCH_RANGE }),
            accent: IntParam::new("Accent", 2, IntRange::Linear { min: 0, max: STEPS as i32 }),
            base_vel: FloatParam::new("Velocity", 0.7, FloatRange::Linear { min: 0.0, max: 1.0 }),
            accent_vel: FloatParam::new("Accent Level", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 }),
            hold: BoolParam::new("Hold", true),
            editor_state: editor::default_state(),
        }
    }
}

pub struct Trafalgar {
    params: Arc<TrafalgarParams>,
    /// Pad touch state, shared GUI -> audio. Gates notes when Hold is off.
    gate: Arc<AtomicBool>,
    /// Current playhead step, shared audio -> GUI for the step display. -1 = idle.
    step: Arc<AtomicI64>,
    last_step: i64,           // last step index emitted; -1 = none
    playing_note: Option<u8>, // currently sounding note (monophonic, this slice)
}

impl Default for Trafalgar {
    fn default() -> Self {
        Self {
            params: Arc::new(TrafalgarParams::default()),
            gate: Arc::new(AtomicBool::new(false)),
            step: Arc::new(AtomicI64::new(-1)),
            last_step: -1,
            playing_note: None,
        }
    }
}

impl Plugin for Trafalgar {
    const NAME: &'static str = "Trafalgar";
    const VENDOR: &'static str = "BlueJayLouche";
    const URL: &'static str = "https://github.com/BlueJayLouche/trafalgar";
    const EMAIL: &'static str = "noreply@example.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // No audio in; a silent stereo out just so hosts/standalone give us a clock.
    // ponytail: we never write audio — the output stays zeroed.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.params.clone(),
            self.gate.clone(),
            self.step.clone(),
            self.params.editor_state.clone(),
        )
    }

    fn reset(&mut self) {
        self.last_step = -1;
        self.playing_note = None;
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // We emit no audio — keep the output silent.
        for ch in buffer.as_slice() {
            ch.fill(0.0);
        }

        let t = context.transport();
        let sr = t.sample_rate as f64;
        let block = buffer.samples();

        // Need tempo + position + play state to sequence. Otherwise idle (and
        // release any hanging note).
        let (Some(tempo), Some(pos)) = (t.tempo, t.pos_samples()) else {
            if let Some(n) = self.playing_note.take() {
                context.send_event(NoteEvent::NoteOff { timing: 0, voice_id: None, channel: 0, note: n, velocity: 0.0 });
            }
            return ProcessStatus::Normal;
        };
        // Hold on => always sound; Hold off => only while the pad is being dragged.
        let gate_open = self.params.hold.value() || self.gate.load(Ordering::Relaxed);
        if !t.playing || !gate_open {
            if let Some(n) = self.playing_note.take() {
                context.send_event(NoteEvent::NoteOff { timing: 0, voice_id: None, channel: 0, note: n, velocity: 0.0 });
            }
            self.last_step = -1;
            self.step.store(-1, Ordering::Relaxed);
            return ProcessStatus::Normal;
        }

        let samples_per_step = 60.0 / tempo * sr / 4.0; // 16th notes
        let density = self.params.density.value() as usize;
        let rotation = self.params.rotation.value() as usize;
        let pattern = rotated(density, STEPS, rotation);
        // Second euclidean layer picks accented (louder) steps.
        let accents = euclid(self.params.accent.value() as usize, STEPS);

        for s in 0..block {
            let global = pos + s as i64;
            let step = (global as f64 / samples_per_step).floor() as i64;
            if step == self.last_step {
                continue;
            }
            self.last_step = step;
            let timing = s as u32;

            // note-off the previous note at the step boundary (monophonic)
            if let Some(n) = self.playing_note.take() {
                context.send_event(NoteEvent::NoteOff { timing, voice_id: None, channel: 0, note: n, velocity: 0.0 });
            }
            let idx = step.rem_euclid(STEPS as i64) as usize;
            if pattern[idx] {
                let note = scale_note(self.params.pitch.value() as u8);
                let velocity = if accents[idx] {
                    self.params.accent_vel.value()
                } else {
                    self.params.base_vel.value()
                };
                context.send_event(NoteEvent::NoteOn { timing, voice_id: None, channel: 0, note, velocity });
                self.playing_note = Some(note);
            }
        }

        self.step.store(self.last_step.rem_euclid(STEPS as i64), Ordering::Relaxed);
        ProcessStatus::Normal
    }
}

impl ClapPlugin for Trafalgar {
    const CLAP_ID: &'static str = "com.bluejaylouche.trafalgar";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Euclidean jam sequencer");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] =
        &[ClapFeature::Instrument, ClapFeature::NoteEffect, ClapFeature::Utility];
}

impl Vst3Plugin for Trafalgar {
    const VST3_CLASS_ID: [u8; 16] = *b"TrafalgarSeq0001";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Tools];
}

nih_export_clap!(Trafalgar);
nih_export_vst3!(Trafalgar);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn euclid_onsets() {
        assert_eq!(euclid(4, 16).iter().filter(|&&b| b).count(), 4);
        assert!(euclid(4, 16)[0], "downbeat on step 0");
        assert_eq!(euclid(3, 8), [true, false, false, true, false, false, true, false]);
        assert_eq!(euclid(0, 8), vec![false; 8]);
        assert_eq!(euclid(8, 8), vec![true; 8]);
    }
    #[test]
    fn rotation_shifts_onset() {
        assert!(rotated(4, 16, 0)[0]);
        assert!(rotated(4, 16, 1)[1]);
        assert!(rotated(4, 16, 15)[15]);
        assert_eq!(rotated(3, 8, 3).iter().filter(|&&b| b).count(), 3);
    }
    #[test]
    fn pitch_in_range() {
        for d in 0..=PITCH_RANGE as u8 {
            assert!(scale_note(d) <= 127);
        }
    }
}

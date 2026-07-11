// Trafalgar — euclidean jam sequencer. nih-plug plugin emitting MIDI from four
// independent euclidean tracks driven by the host transport. Each track has its
// own XY pad (X = pitch/probability, Y = density), euclidean accents, a hold gate,
// a melodic/percussive mode, and its own MIDI channel. Builds CLAP/VST3/standalone.

use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

mod editor;

pub(crate) const STEPS: usize = 16;
pub(crate) const NUM_TRACKS: usize = 4;
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

/// Tiny xorshift64 -> [0, 1). ponytail: a real RNG crate is overkill for hit dice.
fn xorshift(state: &mut u64) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    (x >> 40) as f32 / (1u64 << 24) as f32
}

/// Map a scale degree onto a MIDI note in the minor pentatonic.
fn scale_note(degree: u8) -> u8 {
    let oct = (degree / PENTATONIC.len() as u8) as i32;
    let step = PENTATONIC[(degree as usize) % PENTATONIC.len()];
    (BASE_NOTE as i32 + 12 * oct + step as i32).clamp(0, 127) as u8
}

/// One euclidean voice.
#[derive(Params)]
pub(crate) struct TrackParams {
    #[id = "density"]
    pub density: IntParam,
    #[id = "rotation"]
    pub rotation: IntParam,
    #[id = "pitch"]
    pub pitch: IntParam,
    #[id = "accent"]
    pub accent: IntParam,
    #[id = "basevel"]
    pub base_vel: FloatParam,
    #[id = "accentvel"]
    pub accent_vel: FloatParam,
    /// Hold on = latched. Hold off = only sounds while dragging the pad.
    #[id = "hold"]
    pub hold: BoolParam,
    /// Percussive: fixed drum note, X axis = hit probability. Melodic: X = scale pitch.
    #[id = "perc"]
    pub percussive: BoolParam,
    #[id = "note"]
    pub note: IntParam,
}

impl Default for TrackParams {
    fn default() -> Self {
        Self {
            density: IntParam::new("Density", 4, IntRange::Linear { min: 1, max: STEPS as i32 }),
            rotation: IntParam::new("Rotation", 0, IntRange::Linear { min: 0, max: STEPS as i32 - 1 }),
            pitch: IntParam::new("Pitch", 5, IntRange::Linear { min: 0, max: PITCH_RANGE }),
            accent: IntParam::new("Accent", 2, IntRange::Linear { min: 0, max: STEPS as i32 }),
            base_vel: FloatParam::new("Velocity", 0.7, FloatRange::Linear { min: 0.0, max: 1.0 }),
            accent_vel: FloatParam::new("Accent Level", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 }),
            hold: BoolParam::new("Hold", true),
            percussive: BoolParam::new("Percussive", false),
            note: IntParam::new("Drum Note", 36, IntRange::Linear { min: 0, max: 127 }),
        }
    }
}

#[derive(Params)]
struct TrafalgarParams {
    #[nested(array, group = "Track")]
    tracks: [TrackParams; NUM_TRACKS],

    #[persist = "editor-state"]
    editor_state: Arc<ViziaState>,
}

impl Default for TrafalgarParams {
    fn default() -> Self {
        Self {
            tracks: std::array::from_fn(|_| TrackParams::default()),
            editor_state: editor::default_state(),
        }
    }
}

/// Per-track runtime state, shared audio<->GUI where needed.
pub(crate) struct Shared {
    /// Pad touch state per track (GUI -> audio); gates notes when Hold is off.
    pub gate: [AtomicBool; NUM_TRACKS],
    /// Current playhead step per track (audio -> GUI); -1 = idle.
    pub step: [AtomicI64; NUM_TRACKS],
}

pub struct Trafalgar {
    params: Arc<TrafalgarParams>,
    shared: Arc<Shared>,
    last_step: [i64; NUM_TRACKS],
    playing_note: [Option<u8>; NUM_TRACKS],
    rng: [u64; NUM_TRACKS],
}

impl Default for Trafalgar {
    fn default() -> Self {
        Self {
            params: Arc::new(TrafalgarParams::default()),
            shared: Arc::new(Shared {
                gate: std::array::from_fn(|_| AtomicBool::new(false)),
                step: std::array::from_fn(|_| AtomicI64::new(-1)),
            }),
            last_step: [-1; NUM_TRACKS],
            playing_note: [None; NUM_TRACKS],
            rng: std::array::from_fn(|i| {
                0x9E37_79B9_7F4A_7C15u64.wrapping_add((i as u64).wrapping_mul(0x1234_5678_9ABC_DEF1))
            }),
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
        editor::create(self.params.clone(), self.shared.clone(), self.params.editor_state.clone())
    }

    fn reset(&mut self) {
        self.last_step = [-1; NUM_TRACKS];
        self.playing_note = [None; NUM_TRACKS];
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

        let (Some(tempo), Some(pos)) = (t.tempo, t.pos_samples()) else {
            for tr in 0..NUM_TRACKS {
                if let Some(n) = self.playing_note[tr].take() {
                    context.send_event(NoteEvent::NoteOff { timing: 0, voice_id: None, channel: tr as u8, note: n, velocity: 0.0 });
                }
                self.shared.step[tr].store(-1, Ordering::Relaxed);
            }
            return ProcessStatus::Normal;
        };
        let playing = t.playing;
        let samples_per_step = 60.0 / tempo * sr / 4.0; // 16th notes

        for tr in 0..NUM_TRACKS {
            let p = &self.params.tracks[tr];

            // Hold on => always sound; Hold off => only while the pad is dragged.
            let gate_open = p.hold.value() || self.shared.gate[tr].load(Ordering::Relaxed);
            if !playing || !gate_open {
                if let Some(n) = self.playing_note[tr].take() {
                    context.send_event(NoteEvent::NoteOff { timing: 0, voice_id: None, channel: tr as u8, note: n, velocity: 0.0 });
                }
                self.last_step[tr] = -1;
                self.shared.step[tr].store(-1, Ordering::Relaxed);
                continue;
            }

            let pattern = rotated(p.density.value() as usize, STEPS, p.rotation.value() as usize);
            let accents = euclid(p.accent.value() as usize, STEPS);

            for s in 0..block {
                let global = pos + s as i64;
                let stp = (global as f64 / samples_per_step).floor() as i64;
                if stp == self.last_step[tr] {
                    continue;
                }
                self.last_step[tr] = stp;
                let timing = s as u32;

                // note-off the previous note at the step boundary (monophonic per track)
                if let Some(n) = self.playing_note[tr].take() {
                    context.send_event(NoteEvent::NoteOff { timing, voice_id: None, channel: tr as u8, note: n, velocity: 0.0 });
                }

                let idx = stp.rem_euclid(STEPS as i64) as usize;
                if pattern[idx] {
                    // Melodic: scale pitch, always fires. Percussive: fixed note, X = probability.
                    let (note, fire) = if p.percussive.value() {
                        let prob = p.pitch.value() as f32 / PITCH_RANGE as f32;
                        (p.note.value() as u8, xorshift(&mut self.rng[tr]) < prob)
                    } else {
                        (scale_note(p.pitch.value() as u8), true)
                    };
                    if fire {
                        let velocity = if accents[idx] { p.accent_vel.value() } else { p.base_vel.value() };
                        context.send_event(NoteEvent::NoteOn { timing, voice_id: None, channel: tr as u8, note, velocity });
                        self.playing_note[tr] = Some(note);
                    }
                }
            }

            self.shared.step[tr].store(self.last_step[tr].rem_euclid(STEPS as i64), Ordering::Relaxed);
        }

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
    fn xorshift_in_unit_range() {
        let mut s = 1u64;
        for _ in 0..1000 {
            let v = xorshift(&mut s);
            assert!((0.0..1.0).contains(&v), "{v} out of range");
        }
    }
    #[test]
    fn pitch_in_range() {
        for d in 0..=PITCH_RANGE as u8 {
            assert!(scale_note(d) <= 127);
        }
    }
}

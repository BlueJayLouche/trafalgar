// Trafalgar — euclidean jam sequencer. nih-plug plugin emitting MIDI from four
// independent euclidean tracks driven by the host transport. Each track has its
// own XY pad (X = pitch/probability, Y = density), euclidean accents, a hold gate,
// a melodic/percussive mode, and its own MIDI channel. Builds CLAP/VST3/standalone.

use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

mod editor;
mod osc;

pub(crate) const STEPS: usize = 16;
pub(crate) const NUM_TRACKS: usize = 4;
pub(crate) const MAX_BARS: usize = 8; // longest recordable gesture loop
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
    /// Armed: touching the pad writes the pitch into the loop at the playhead.
    #[id = "record"]
    pub record: BoolParam,
    /// Gesture loop length in bars (the euclidean rhythm still repeats every bar).
    #[id = "length"]
    pub length: IntParam,
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
            record: BoolParam::new("Record", false),
            length: IntParam::new("Bars", 1, IntRange::Linear { min: 1, max: MAX_BARS as i32 }),
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
    /// Live pad position while touched: packed (x_norm << 32 | density_norm), as
    /// f32 bits. Written before `gate` opens so the audio thread sees position and
    /// gate together — params lag through the event queue, this doesn't.
    pub pos: [AtomicU64; NUM_TRACKS],
    /// Current playhead step per track (audio -> GUI); -1 = idle.
    pub step: [AtomicI64; NUM_TRACKS],
    /// Recorded gesture: pitch degree per step (up to MAX_BARS bars), per track.
    /// -1 = empty. Owned by the audio thread (record/erase/playback), read by the
    /// GUI to draw the loop.
    pub gesture: [[AtomicI32; STEPS * MAX_BARS]; NUM_TRACKS],
    /// Momentary erase (GUI -> audio): true only while the Erase button is held.
    pub erase: [AtomicBool; NUM_TRACKS],
}

pub struct Trafalgar {
    params: Arc<TrafalgarParams>,
    shared: Arc<Shared>,
    last_step: [i64; NUM_TRACKS],
    playing_note: [Option<u8>; NUM_TRACKS],
    rng: [u64; NUM_TRACKS],
    osc: Option<osc::OscSender>,
}

impl Default for Trafalgar {
    fn default() -> Self {
        Self {
            params: Arc::new(TrafalgarParams::default()),
            shared: Arc::new(Shared {
                gate: std::array::from_fn(|_| AtomicBool::new(false)),
                pos: std::array::from_fn(|_| AtomicU64::new(0)),
                step: std::array::from_fn(|_| AtomicI64::new(-1)),
                gesture: std::array::from_fn(|_| std::array::from_fn(|_| AtomicI32::new(-1))),
                erase: std::array::from_fn(|_| AtomicBool::new(false)),
            }),
            last_step: [-1; NUM_TRACKS],
            playing_note: [None; NUM_TRACKS],
            rng: std::array::from_fn(|i| {
                0x9E37_79B9_7F4A_7C15u64.wrapping_add((i as u64).wrapping_mul(0x1234_5678_9ABC_DEF1))
            }),
            osc: None,
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

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        _buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        if self.osc.is_none() {
            self.osc = osc::OscSender::new();
        }
        true
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

            // While the pad is touched, take pitch/density from the instant position
            // atomic (no event-queue lag) so the first note lands at the clicked
            // position; otherwise use the params (preserves host automation + latch).
            let touching = self.shared.gate[tr].load(Ordering::Acquire);
            let (live_pitch, density) = if touching {
                let packed = self.shared.pos[tr].load(Ordering::Relaxed);
                let nx = f32::from_bits((packed >> 32) as u32);
                let dnorm = f32::from_bits(packed as u32);
                (
                    (nx * PITCH_RANGE as f32).round().clamp(0.0, PITCH_RANGE as f32) as i32,
                    (1.0 + dnorm * (STEPS as f32 - 1.0)).round().clamp(1.0, STEPS as f32) as usize,
                )
            } else {
                (p.pitch.value(), p.density.value() as usize)
            };

            let record = p.record.value();
            let erase = self.shared.erase[tr].load(Ordering::Relaxed);
            let loop_steps = p.length.value() as usize * STEPS;
            // A cell is -1 empty, -2 a recorded rest, or 0..=127 a recorded note.
            let has_rec = self.shared.gesture[tr][..loop_steps]
                .iter()
                .any(|c| c.load(Ordering::Relaxed) != -1);

            // Sound if Hold is on, the pad is touched, or a loop is recorded.
            let gate_open = p.hold.value() || touching || has_rec;
            if !playing || !gate_open {
                if let Some(n) = self.playing_note[tr].take() {
                    context.send_event(NoteEvent::NoteOff { timing: 0, voice_id: None, channel: tr as u8, note: n, velocity: 0.0 });
                }
                self.last_step[tr] = -1;
                self.shared.step[tr].store(-1, Ordering::Relaxed);
                continue;
            }

            let pattern = rotated(density, STEPS, p.rotation.value() as usize);
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

                let idx = stp.rem_euclid(STEPS as i64) as usize; // euclidean pattern (per bar)
                let gidx = stp.rem_euclid(loop_steps as i64) as usize; // gesture loop position

                let cell = &self.shared.gesture[tr][gidx];
                if erase {
                    cell.store(-1, Ordering::Relaxed);
                }

                // The note the live generator produces this step at `pitch_deg`:
                // onset comes from the (Y-driven) euclidean pattern, pitch from X.
                let gen_note = |pitch_deg: i32, rng: &mut u64| -> Option<u8> {
                    if !pattern[idx] {
                        return None;
                    }
                    if p.percussive.value() {
                        let prob = pitch_deg as f32 / PITCH_RANGE as f32;
                        (xorshift(rng) < prob).then(|| p.note.value() as u8)
                    } else {
                        Some(scale_note(pitch_deg as u8))
                    }
                };

                // What actually sounds this step. While touched we play (and, if
                // armed, record) the live generator; otherwise a recorded loop plays
                // its literal notes; otherwise the latched generator runs.
                let emit: Option<u8> = if touching {
                    let n = gen_note(live_pitch, &mut self.rng[tr]);
                    if record {
                        // record the literal outcome: the note that fired, or a rest
                        cell.store(n.map(|x| x as i32).unwrap_or(-2), Ordering::Relaxed);
                    }
                    n
                } else if has_rec {
                    let v = cell.load(Ordering::Relaxed);
                    (v >= 0).then_some(v as u8)
                } else {
                    gen_note(p.pitch.value(), &mut self.rng[tr])
                };

                if let Some(note) = emit {
                    let velocity = if accents[idx] { p.accent_vel.value() } else { p.base_vel.value() };
                    context.send_event(NoteEvent::NoteOn { timing, voice_id: None, channel: tr as u8, note, velocity });
                    if let Some(o) = &self.osc {
                        o.note(tr as u8, note, velocity);
                    }
                    self.playing_note[tr] = Some(note);
                }
            }

            self.shared.step[tr].store(self.last_step[tr], Ordering::Relaxed);
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

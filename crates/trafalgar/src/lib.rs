// Trafalgar — euclidean jam sequencer. nih-plug plugin emitting MIDI from four
// independent euclidean tracks driven by the host transport. Each track has its
// own XY pad (X = pitch/probability, Y = density), euclidean accents, a hold gate,
// a melodic/percussive mode, and its own MIDI channel. Builds CLAP/VST3/standalone.

use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::ops::Index;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

mod editor;
mod link;
mod midi;
mod osc;

pub(crate) const STEPS: usize = 16;
pub(crate) const NUM_TRACKS: usize = 4;
pub(crate) const MAX_BARS: usize = 8; // longest recordable gesture loop
pub(crate) const PITCH_RANGE: i32 = 15; // scale degrees the pitch param spans

/// Selectable scales. Semitone offsets returned by `steps()`; note count varies
/// per scale (that's the "note steps" — 5 for pentatonic, 7 for the modes, 12
/// chromatic), so the same pitch range spans more or fewer octaves per scale.
#[derive(Enum, PartialEq, Clone, Copy)]
pub enum Scale {
    #[id = "minpent"]
    #[name = "Min Pent"]
    MinorPentatonic,
    #[id = "majpent"]
    #[name = "Maj Pent"]
    MajorPentatonic,
    #[id = "major"]
    Major,
    #[id = "minor"]
    Minor,
    #[id = "dorian"]
    Dorian,
    #[id = "chromatic"]
    Chromatic,
}

impl Scale {
    fn steps(self) -> &'static [u8] {
        match self {
            Scale::MinorPentatonic => &[0, 3, 5, 7, 10],
            Scale::MajorPentatonic => &[0, 2, 4, 7, 9],
            Scale::Major => &[0, 2, 4, 5, 7, 9, 11],
            Scale::Minor => &[0, 2, 3, 5, 7, 8, 10],
            Scale::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Scale::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        }
    }
}

/// Human-readable MIDI note, e.g. 36 -> "36/C2" (scientific pitch, C4 = 60).
fn note_name(v: i32) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}/{}{}", v, NAMES[v.rem_euclid(12) as usize], v.div_euclid(12) - 1)
}

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

/// Map a scale degree onto a MIDI note in `scale`, rooted at `base`.
fn scale_note(base: u8, scale: Scale, degree: u8) -> u8 {
    let steps = scale.steps();
    let oct = (degree as usize / steps.len()) as i32;
    let step = steps[degree as usize % steps.len()];
    (base as i32 + 12 * oct + step as i32).clamp(0, 127) as u8
}

/// OSC output target, editable from the standalone settings popout and persisted
/// with the plugin state.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct OscSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// OSC *input* (remote pad control). Opt-in; the port is the instance's identity.
    pub in_enabled: bool,
    pub in_port: u16,
    /// true → bind 0.0.0.0 (reachable from the phone on the LAN); false → loopback only.
    pub in_lan: bool,
}

impl Default for OscSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 9000,
            in_enabled: false,
            in_port: 9100,
            in_lan: true,
        }
    }
}

/// MIDI output target, editable from the settings popout and persisted with state.
/// Opt-in and independent of the host/standalone routing (see midi.rs).
#[derive(Serialize, Deserialize, Clone, Default)]
pub(crate) struct MidiSettings {
    pub enabled: bool,
    /// Name of an existing output port to connect to (ignored when `virtual_port`).
    pub port: String,
    /// Create our own virtual port named "Trafalgar" instead (macOS/Linux only).
    pub virtual_port: bool,
}

/// The recorded gesture loops: one cell per step (up to MAX_BARS bars) per track —
/// -1 empty, -2 a recorded rest, 0..=127 a recorded note. Shared audio<->GUI as
/// atomics AND persisted with the plugin state (a `#[persist]` param field), so
/// recorded patterns survive save/reload in a host. Cloning shares the same atomics
/// (Arc inside), so the persisted param and `Shared` see the same buffer. Index by
/// track: `gesture[tr]`.
#[derive(Clone)]
pub(crate) struct Gesture(Arc<[[AtomicI32; STEPS * MAX_BARS]; NUM_TRACKS]>);

impl Default for Gesture {
    fn default() -> Self {
        Gesture(Arc::new(std::array::from_fn(|_| std::array::from_fn(|_| AtomicI32::new(-1)))))
    }
}

impl Index<usize> for Gesture {
    type Output = [AtomicI32; STEPS * MAX_BARS];
    fn index(&self, track: usize) -> &Self::Output {
        &self.0[track]
    }
}

// Persist as track-major Vec<Vec<i32>>. `set` zips, so a size change across versions
// (different STEPS/MAX_BARS/NUM_TRACKS) truncates safely rather than panicking.
impl<'a> PersistentField<'a, Vec<Vec<i32>>> for Gesture {
    fn set(&self, new_value: Vec<Vec<i32>>) {
        for (track, cells) in self.0.iter().zip(&new_value) {
            for (cell, &v) in track.iter().zip(cells) {
                cell.store(v, Ordering::Relaxed);
            }
        }
    }
    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Vec<Vec<i32>>) -> R,
    {
        let snap = self
            .0
            .iter()
            .map(|t| t.iter().map(|c| c.load(Ordering::Relaxed)).collect())
            .collect();
        f(&snap)
    }
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
    /// Percussive: the drum note. Melodic: the base (root) note the scale sits on.
    #[id = "note"]
    pub note: IntParam,
    #[id = "scale"]
    pub scale: EnumParam<Scale>,
    /// Armed: touching the pad writes the pitch into the loop at the playhead.
    #[id = "record"]
    pub record: BoolParam,
    /// Gesture loop length in bars (the euclidean rhythm still repeats every bar).
    #[id = "length"]
    pub length: IntParam,
    /// Note length as a fraction of one step (gate time). Ignored when portamento
    /// is on (that forces full-length legato so the synth can glide).
    #[id = "notelen"]
    pub note_len: FloatParam,
    /// Legato + MIDI portamento (CC 65): trigger the next note before releasing the
    /// current one and hold notes full-length, so a mono synth glides between pitches.
    #[id = "porta"]
    pub portamento: BoolParam,
    /// MIDI output channel (1–16). Default = track number, so the four tracks land
    /// on channels 1–4 out of the box.
    #[id = "channel"]
    pub channel: IntParam,
}

impl TrackParams {
    fn new(track: usize) -> Self {
        Self {
            density: IntParam::new("Density", 4, IntRange::Linear { min: 1, max: STEPS as i32 }),
            rotation: IntParam::new("Rotation", 0, IntRange::Linear { min: 0, max: STEPS as i32 - 1 }),
            pitch: IntParam::new("Pitch", 5, IntRange::Linear { min: 0, max: PITCH_RANGE }),
            accent: IntParam::new("Accent", 2, IntRange::Linear { min: 0, max: STEPS as i32 }),
            base_vel: FloatParam::new("Velocity", 0.7, FloatRange::Linear { min: 0.0, max: 1.0 }),
            accent_vel: FloatParam::new("Accent Level", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 }),
            hold: BoolParam::new("Hold", true),
            percussive: BoolParam::new("Percussive", false),
            note: IntParam::new("Note", 36, IntRange::Linear { min: 0, max: 127 })
                .with_value_to_string(Arc::new(note_name)),
            scale: EnumParam::new("Scale", Scale::MinorPentatonic),
            record: BoolParam::new("Record", false),
            length: IntParam::new("Bars", 1, IntRange::Linear { min: 1, max: MAX_BARS as i32 }),
            note_len: FloatParam::new("Note Len", 1.0, FloatRange::Linear { min: 0.05, max: 1.0 }),
            portamento: BoolParam::new("Porta", false),
            channel: IntParam::new("MIDI Ch", (track as i32 % 16) + 1, IntRange::Linear { min: 1, max: 16 }),
        }
    }
}

impl Default for TrackParams {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Params)]
struct TrafalgarParams {
    #[nested(array, group = "Track")]
    tracks: [TrackParams; NUM_TRACKS],

    #[persist = "editor-state"]
    editor_state: Arc<ViziaState>,

    #[persist = "osc"]
    osc: Arc<RwLock<OscSettings>>,

    #[persist = "midi"]
    midi: Arc<RwLock<MidiSettings>>,

    /// Ableton Link enabled. Just a bool, but persisted like the other settings.
    #[persist = "link"]
    link: Arc<RwLock<bool>>,

    /// Recorded patterns. Shared with the audio thread via `Shared::gesture`, which
    /// holds a clone of this same Arc, so persisting here saves the live loops.
    #[persist = "gesture"]
    gesture: Gesture,
}

impl Default for TrafalgarParams {
    fn default() -> Self {
        Self {
            tracks: std::array::from_fn(TrackParams::new),
            editor_state: editor::default_state(),
            osc: Arc::new(RwLock::new(OscSettings::default())),
            midi: Arc::new(RwLock::new(MidiSettings::default())),
            link: Arc::new(RwLock::new(false)),
            gesture: Gesture::default(),
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
    /// Recorded gesture loops (see `Gesture`). A clone of the persisted param Arc,
    /// so what the audio thread records is what gets saved with the project.
    pub gesture: Gesture,
    /// Momentary erase (GUI -> audio): true only while the Erase button is held.
    pub erase: [AtomicBool; NUM_TRACKS],
    /// Set by the settings popout when OSC config changes; the audio thread
    /// rebuilds the sender on the next process block and clears it.
    pub osc_dirty: AtomicBool,
    /// Same, for the OSC *input* receiver.
    pub osc_in_dirty: AtomicBool,
    /// OSC-in bind state for the settings-panel status line: 0 off, 1 listening,
    /// 2 bind failed (port in use). Written by the audio thread on rebuild.
    pub osc_in_status: AtomicU8,
    /// Same pattern for the MIDI output port: dirty flag + connection status
    /// (0 off, 1 connected, 2 failed).
    pub midi_dirty: AtomicBool,
    pub midi_status: AtomicU8,
    /// Ableton Link: dirty flag (enable/disable changed) + live peer count for the
    /// settings status line. Peers written by the audio thread; 0 when disabled.
    pub link_dirty: AtomicBool,
    pub link_peers: AtomicU32,
}

pub struct Trafalgar {
    params: Arc<TrafalgarParams>,
    shared: Arc<Shared>,
    last_step: [i64; NUM_TRACKS],
    /// The currently-sounding note per track, as `(note, channel)` — the channel is
    /// stored so a NoteOff always goes out on the same channel its NoteOn did, even
    /// if the channel param changed while the note was held.
    playing_note: [Option<(u8, u8)>; NUM_TRACKS],
    /// Absolute sample at which the current note should release (note-length gate).
    /// i64::MAX = no pending release (held until the next onset, e.g. portamento).
    note_off_at: [i64; NUM_TRACKS],
    rng: [u64; NUM_TRACKS],
    osc: Option<osc::OscSender>,
    osc_in: Option<osc::OscReceiver>,
    midi: Option<midi::MidiSender>,
    /// Ableton Link clock, `Some` only while enabled. When set it dictates tempo +
    /// position instead of the host transport.
    link: Option<link::LinkClock>,
    /// Monotonic sample counter for Link's host-time filter (see link.rs).
    sample_clock: u64,
}

impl Default for Trafalgar {
    fn default() -> Self {
        let params = Arc::new(TrafalgarParams::default());
        Self {
            shared: Arc::new(Shared {
                gate: std::array::from_fn(|_| AtomicBool::new(false)),
                pos: std::array::from_fn(|_| AtomicU64::new(0)),
                step: std::array::from_fn(|_| AtomicI64::new(-1)),
                gesture: params.gesture.clone(), // same atomics the param persists
                erase: std::array::from_fn(|_| AtomicBool::new(false)),
                osc_dirty: AtomicBool::new(true), // build the sender on first process
                osc_in_dirty: AtomicBool::new(true), // build the receiver on first process
                osc_in_status: AtomicU8::new(0),
                midi_dirty: AtomicBool::new(true), // build the port on first process
                midi_status: AtomicU8::new(0),
                link_dirty: AtomicBool::new(true), // apply the persisted Link setting on first process
                link_peers: AtomicU32::new(0),
            }),
            params,
            last_step: [-1; NUM_TRACKS],
            playing_note: [None; NUM_TRACKS],
            note_off_at: [i64::MAX; NUM_TRACKS],
            rng: std::array::from_fn(|i| {
                0x9E37_79B9_7F4A_7C15u64.wrapping_add((i as u64).wrapping_mul(0x1234_5678_9ABC_DEF1))
            }),
            osc: None,
            osc_in: None,
            midi: None,
            link: None,
            sample_clock: 0,
        }
    }
}

impl Trafalgar {
    /// Send a note event to the host AND, if configured, our own MIDI port.
    #[inline]
    fn emit(&self, context: &mut impl ProcessContext<Self>, ev: NoteEvent<()>) {
        if let Some(m) = &self.midi {
            m.send_event(&ev);
        }
        context.send_event(ev);
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
        // Sender/receiver are (re)built lazily in process() whenever their dirty
        // flags are set; start dirty so the first block opens them from settings.
        self.shared.osc_dirty.store(true, Ordering::Relaxed);
        self.shared.osc_in_dirty.store(true, Ordering::Relaxed);
        self.shared.midi_dirty.store(true, Ordering::Relaxed);
        self.shared.link_dirty.store(true, Ordering::Relaxed);
        true
    }

    fn reset(&mut self) {
        self.last_step = [-1; NUM_TRACKS];
        self.playing_note = [None; NUM_TRACKS];
        self.note_off_at = [i64::MAX; NUM_TRACKS];
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

        // Rebuild the OSC sender if the settings popout changed the target. ponytail:
        // binds a socket + spawns a thread here, but only on the rare config change —
        // move to a BackgroundTask if it ever shows up as an xrun.
        if self.shared.osc_dirty.swap(false, Ordering::Relaxed) {
            let cfg = self.params.osc.read().unwrap().clone();
            self.osc = cfg.enabled.then(|| osc::OscSender::new(&cfg.host, cfg.port)).flatten();
        }
        // Same for the OSC-in receiver; also publish the bind status for the GUI.
        if self.shared.osc_in_dirty.swap(false, Ordering::Relaxed) {
            let cfg = self.params.osc.read().unwrap().clone();
            self.osc_in = cfg
                .in_enabled
                .then(|| osc::OscReceiver::new(self.shared.clone(), cfg.in_lan, cfg.in_port))
                .flatten();
            let status = match (cfg.in_enabled, self.osc_in.is_some()) {
                (false, _) => 0,
                (true, true) => 1,
                (true, false) => 2, // enabled but bind failed → port in use
            };
            self.shared.osc_in_status.store(status, Ordering::Relaxed);
        }
        // Same for the MIDI output port.
        if self.shared.midi_dirty.swap(false, Ordering::Relaxed) {
            let cfg = self.params.midi.read().unwrap().clone();
            self.midi = cfg
                .enabled
                .then(|| midi::MidiSender::new(&cfg.port, cfg.virtual_port))
                .flatten();
            let status = match (cfg.enabled, self.midi.is_some()) {
                (false, _) => 0,
                (true, true) => 1,
                (true, false) => 2, // enabled but the port couldn't be opened
            };
            self.shared.midi_status.store(status, Ordering::Relaxed);
        }
        // Ableton Link: `link` is Some only while enabled. ponytail: drop-on-disable
        // (leaves the session); re-enabling rejoins + rediscovers peers — fine for a
        // rare user toggle. Creating AblLink here binds a socket + spawns Link's
        // discovery thread; that only happens on the toggle, not per block.
        if self.shared.link_dirty.swap(false, Ordering::Relaxed) {
            let on = *self.params.link.read().unwrap();
            self.link = on.then(link::LinkClock::new);
            if self.link.is_none() {
                self.shared.link_peers.store(0, Ordering::Relaxed);
            }
        }

        let t = context.transport();
        let sr = t.sample_rate as f64;
        let block = buffer.samples();

        // When Link is enabled it dictates tempo + position (and always plays);
        // otherwise the host transport does. `sample_clock` feeds Link's time filter
        // and must keep advancing every block.
        let sc = self.sample_clock;
        self.sample_clock = self.sample_clock.wrapping_add(block as u64);
        let (tempo, pos, playing) = if let Some(link) = self.link.as_mut() {
            let (tempo, pos) = link.transport(sc, sr);
            self.shared.link_peers.store(link.peers(), Ordering::Relaxed);
            (tempo, pos, true)
        } else {
            let (Some(tempo), Some(pos)) = (t.tempo, t.pos_samples()) else {
                for tr in 0..NUM_TRACKS {
                    if let Some((n, ch)) = self.playing_note[tr].take() {
                        self.emit(context, NoteEvent::NoteOff { timing: 0, voice_id: None, channel: ch, note: n, velocity: 0.0 });
                    }
                    self.note_off_at[tr] = i64::MAX;
                    self.shared.step[tr].store(-1, Ordering::Relaxed);
                }
                return ProcessStatus::Normal;
            };
            (tempo, pos, t.playing)
        };
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
                if let Some((n, ch)) = self.playing_note[tr].take() {
                    self.emit(context, NoteEvent::NoteOff { timing: 0, voice_id: None, channel: ch, note: n, velocity: 0.0 });
                }
                self.note_off_at[tr] = i64::MAX;
                self.last_step[tr] = -1;
                self.shared.step[tr].store(-1, Ordering::Relaxed);
                continue;
            }

            let pattern = rotated(density, STEPS, p.rotation.value() as usize);
            let accents = euclid(p.accent.value() as usize, STEPS);
            let porta = p.portamento.value();
            let note_len = p.note_len.value();
            let chan = (p.channel.value() - 1) as u8; // param is 1–16, MIDI is 0–15

            for s in 0..block {
                let global = pos + s as i64;

                // Staccato release: fire the note-length gate when its sample arrives.
                // (Skipped under portamento, which holds full-length for legato glide.)
                if global >= self.note_off_at[tr] {
                    if let Some((n, ch)) = self.playing_note[tr].take() {
                        self.emit(context, NoteEvent::NoteOff { timing: s as u32, voice_id: None, channel: ch, note: n, velocity: 0.0 });
                    }
                    self.note_off_at[tr] = i64::MAX;
                }

                let stp = (global as f64 / samples_per_step).floor() as i64;
                if stp == self.last_step[tr] {
                    continue;
                }
                self.last_step[tr] = stp;
                let timing = s as u32;

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
                        Some(scale_note(p.note.value() as u8, p.scale.value(), pitch_deg as u8))
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
                    // Keep the synth's portamento in sync with the toggle (CC 65).
                    self.emit(context, NoteEvent::MidiCC { timing, channel: chan, cc: 65, value: if porta { 1.0 } else { 0.0 } });

                    if porta {
                        // Legato: sound the new note before releasing the old so a mono
                        // synth glides instead of retriggering. Hold full-length.
                        self.emit(context, NoteEvent::NoteOn { timing, voice_id: None, channel: chan, note, velocity });
                        if let Some((prev, ch)) = self.playing_note[tr].take() {
                            self.emit(context, NoteEvent::NoteOff { timing, voice_id: None, channel: ch, note: prev, velocity: 0.0 });
                        }
                        self.note_off_at[tr] = i64::MAX;
                    } else {
                        if let Some((prev, ch)) = self.playing_note[tr].take() {
                            self.emit(context, NoteEvent::NoteOff { timing, voice_id: None, channel: ch, note: prev, velocity: 0.0 });
                        }
                        self.emit(context, NoteEvent::NoteOn { timing, voice_id: None, channel: chan, note, velocity });
                        // Release this note after note_len of the step.
                        self.note_off_at[tr] = ((stp as f64 + note_len as f64) * samples_per_step) as i64;
                    }
                    if let Some(o) = &self.osc {
                        o.note(tr as u8, note, velocity);
                    }
                    self.playing_note[tr] = Some((note, chan));
                } else if !porta {
                    // A rest step with no portamento: release any held note at the boundary.
                    if let Some((prev, ch)) = self.playing_note[tr].take() {
                        self.emit(context, NoteEvent::NoteOff { timing, voice_id: None, channel: ch, note: prev, velocity: 0.0 });
                    }
                    self.note_off_at[tr] = i64::MAX;
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
        for scale in [Scale::MinorPentatonic, Scale::Major, Scale::Dorian, Scale::Chromatic] {
            for d in 0..=PITCH_RANGE as u8 {
                assert!(scale_note(60, scale, d) <= 127);
            }
        }
    }
    #[test]
    fn gesture_persist_roundtrip() {
        // A recorded loop survives a serialize (map) -> deserialize (set) round-trip,
        // and a clone shares the same atomics (what makes Shared see the persisted buf).
        let g = Gesture::default();
        g[0][3].store(60, Ordering::Relaxed);
        g[2][7].store(-2, Ordering::Relaxed); // a recorded rest
        let snap = g.map(|v| v.clone());

        let loaded = Gesture::default();
        loaded.set(snap);
        assert_eq!(loaded[0][3].load(Ordering::Relaxed), 60);
        assert_eq!(loaded[2][7].load(Ordering::Relaxed), -2);
        assert_eq!(loaded[1][0].load(Ordering::Relaxed), -1, "untouched cell stays empty");

        // set truncates rather than panicking if the saved shape is smaller.
        let short = Gesture::default();
        short.set(vec![vec![42]]);
        assert_eq!(short[0][0].load(Ordering::Relaxed), 42);
    }

    #[test]
    fn note_names() {
        assert_eq!(note_name(36), "36/C2");
        assert_eq!(note_name(60), "60/C4");
        assert_eq!(note_name(0), "0/C-1");
        assert_eq!(note_name(69), "69/A4");
    }
}

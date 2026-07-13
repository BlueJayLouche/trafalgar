// Ableton Link: network tempo + beat-phase sync. When enabled, Link (not the host
// transport) dictates tempo and bar position, so the standalone jams in time with
// Ableton Live and other Link peers on the LAN.
//
// Polled inline on the audio thread each block via `capture_audio_session_state`
// (the realtime-safe capture) — no callbacks (whose rusty_link API has lifetime
// hazards) and no GUI-timer jitter. A HostTimeFilter maps our monotonic sample
// clock to Link's host clock, the way Ableton's own example audio callback does.

use rusty_link::{AblLink, HostTimeFilter, SessionState};

/// One bar = 4 beats, matching the 16-step (16th-note) pattern. Fixed for now.
const QUANTUM: f64 = 4.0;

pub struct LinkClock {
    link: AblLink,
    filter: HostTimeFilter,
    session: SessionState,
}

impl LinkClock {
    /// Create and join the Link session. Only constructed while Link is enabled, so
    /// this enables participation immediately; dropping it leaves the session.
    pub fn new() -> Self {
        let link = AblLink::new(120.0);
        link.enable(true);
        Self { link, filter: HostTimeFilter::new(), session: SessionState::new() }
    }

    pub fn peers(&self) -> u32 {
        self.link.num_peers() as u32
    }

    /// Capture Link's state for this block and return `(tempo_bpm, step_position)`,
    /// where the position is in samples on the same timeline the host transport uses
    /// (step = sample_pos / samples_per_step), so the downstream step logic is
    /// unchanged. `sample_clock` must increase monotonically across blocks.
    pub fn transport(&mut self, sample_clock: u64, sample_rate: f64) -> (f64, i64) {
        let host_time = self.filter.sample_time_to_host_time(self.link.clock_micros(), sample_clock);
        self.link.capture_audio_session_state(&mut self.session);
        let tempo = self.session.tempo();
        let beat = self.session.beat_at_time(host_time, QUANTUM);
        let samples_per_step = 60.0 / tempo * sample_rate / 4.0; // 16th notes
        let pos = (beat * 4.0 * samples_per_step).round() as i64;
        (tempo, pos)
    }
}

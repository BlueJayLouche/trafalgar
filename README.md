# Trafalgar

A euclidean jam sequencer — CLAP/VST3 plugin **and** standalone app — that emits
**MIDI + OSC** to drive your own synths. Figure's gestural jam feel, no built-in
sound engine.

Inspired by [Propellerhead Figure](https://en.wikipedia.org/wiki/Figure_(software)).
Not affiliated with Reason Studios.

## Status

Early. The core interaction (dragging an XY pad live-morphs a euclidean pattern)
is validated; this repo is the real build growing out from that.

- **v1 target:** 4 uniform euclidean tracks, each with an XY performance pad
  (X = density, Y = pitch/velocity), a `melodic | percussive` mode, its own MIDI
  channel, plus recordable gesture lanes → assignable MIDI CC / OSC, and a native
  MIDI note-delay. Pure live jam (no scenes/song mode yet).
- **Clock:** follows host transport as a plugin; internal clock + Ableton Link +
  MIDI clock out in standalone.

## Build

```sh
cargo build --release                 # library + standalone
cargo xtask bundle trafalgar --release # CLAP + VST3 bundles
cargo test                            # sequencer logic
```

Standalone runs the plugin against your audio/MIDI devices; the CLAP/VST3 bundles
land in `target/bundled/`.

## License

GPL-3.0-or-later. (nih-plug's VST3 export requires GPL; CLAP + standalone come
along for the ride.)

# Trafalgar

A euclidean jam sequencer — CLAP/VST3 plugin **and** standalone app — that emits
**MIDI + OSC** to drive your own synths. Figure's gestural jam feel, no built-in
sound engine.

Inspired by [Propellerhead Figure](https://en.wikipedia.org/wiki/Figure_(software)).
Not affiliated with Reason Studios.

## Status

Playable. Four euclidean tracks, each with:

- An **XY performance pad** — X = pitch (keybed) / hit probability, Y = density —
  with a live step display and a hold gate (off = touch-to-play).
- **Rotation** and a second euclidean **accent** layer (base + accent velocity).
- A `melodic | percussive` mode toggle and its own **MIDI channel** (0–3).

Notes are emitted as MIDI and mirrored over **OSC** (`/fig/track/{n}/note`) to
`127.0.0.1:9000`. Clock follows the host transport.

**Not yet:** recordable gesture lanes → CC/OSC, a native MIDI note-delay,
configurable OSC target, Ableton Link in standalone, scenes/song mode. The
standalone runs the editor but does not yet route MIDI out to other apps (that
lands with the dedicated MIDI/OSC output layer).

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

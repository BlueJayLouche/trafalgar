// MIDI output over midir — a selectable hardware/virtual port, independent of the
// host/standalone routing (so notes can go straight to a synth on any system, and
// the port can be changed at runtime from the settings panel). Architecture mirrors
// osc.rs: the audio thread pushes 3-byte messages through a channel and a background
// thread owns the connection and does the (realtime-unsafe) send.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use midir::{MidiOutput, MidiOutputConnection};
use nih_plug::prelude::NoteEvent;

/// Available MIDI output port names, for the settings picker. Empty on error.
pub fn output_ports() -> Vec<String> {
    match MidiOutput::new("Trafalgar scan") {
        Ok(o) => o.ports().iter().filter_map(|p| o.port_name(p).ok()).collect(),
        Err(_) => Vec::new(),
    }
}

pub struct MidiSender {
    tx: Sender<[u8; 3]>,
}

impl MidiSender {
    /// Connect to the existing port named `port`, or (`virtual_port`) create our own
    /// virtual port named "Trafalgar". Returns `None` if the port can't be opened
    /// (MIDI just stays off), same as the OSC sender.
    pub fn new(port: &str, virtual_port: bool) -> Option<Self> {
        let out = MidiOutput::new("Trafalgar").ok()?;
        let conn = if virtual_port {
            // ponytail: virtual ports are a CoreMIDI/ALSA feature — unavailable on
            // Windows, where this reports as a failed connection in the status line.
            #[cfg(unix)]
            {
                use midir::os::unix::VirtualOutput;
                out.create_virtual("Trafalgar").ok()?
            }
            #[cfg(not(unix))]
            {
                let _ = out;
                return None;
            }
        } else {
            let ports = out.ports();
            let p = ports.iter().find(|p| out.port_name(p).as_deref() == Ok(port))?;
            out.connect(p, "Trafalgar out").ok()?
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run(conn, rx));
        Some(Self { tx })
    }

    /// Forward a note event to the port. Non-blocking; drops on a full/closed channel.
    pub fn send_event(&self, ev: &NoteEvent<()>) {
        let clamp = |v: f32| (v * 127.0).clamp(0.0, 127.0) as u8;
        let bytes = match *ev {
            NoteEvent::NoteOn { channel, note, velocity, .. } => [0x90 | channel, note, clamp(velocity)],
            NoteEvent::NoteOff { channel, note, .. } => [0x80 | channel, note, 0],
            NoteEvent::MidiCC { channel, cc, value, .. } => [0xB0 | channel, cc, clamp(value)],
            _ => return,
        };
        let _ = self.tx.send(bytes);
    }
}

fn run(mut conn: MidiOutputConnection, rx: Receiver<[u8; 3]>) {
    // Exits when the Sender is dropped (plugin torn down / port changed).
    while let Ok(b) = rx.recv() {
        let _ = conn.send(&b);
    }
}

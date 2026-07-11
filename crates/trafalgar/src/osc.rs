// OSC output. The audio thread pushes note events over an mpsc channel; a
// background thread owns the socket and does the rosc encoding + UDP send, so the
// real-time path only does a channel send (no sockets, no encoding).
// ponytail: target is hardcoded to localhost for now — make it configurable when
// someone actually needs a remote host. mpsc is fine at note rates; swap for a
// lock-free ringbuffer only if it ever shows up in an xrun.

use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use rosc::{encoder, OscMessage, OscPacket, OscType};

const TARGET: &str = "127.0.0.1:9000";

struct Note {
    track: u8,
    note: u8,
    vel: f32,
}

pub struct OscSender {
    tx: Sender<Note>,
}

impl OscSender {
    /// Returns `None` if the socket can't be opened (OSC just stays off).
    pub fn new() -> Option<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect(TARGET).ok()?;
        let (tx, rx) = mpsc::channel::<Note>();
        thread::spawn(move || run(socket, rx));
        Some(Self { tx })
    }

    /// Emit `/fig/track/{n}/note [note, velocity]`. Non-blocking; drops on a full/closed channel.
    pub fn note(&self, track: u8, note: u8, vel: f32) {
        let _ = self.tx.send(Note { track, note, vel });
    }
}

fn run(socket: UdpSocket, rx: Receiver<Note>) {
    // Exits when the Sender is dropped (plugin torn down).
    while let Ok(n) = rx.recv() {
        let msg = OscMessage {
            addr: format!("/fig/track/{}/note", n.track),
            args: vec![OscType::Int(n.note as i32), OscType::Float(n.vel)],
        };
        if let Ok(buf) = encoder::encode(&OscPacket::Message(msg)) {
            let _ = socket.send(&buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn emits_note_osc() {
        // Bind the target first so we can receive what the sender emits.
        // If the port is busy, skip rather than fail flakily.
        let Ok(listener) = UdpSocket::bind(TARGET) else {
            return;
        };
        listener.set_read_timeout(Some(Duration::from_millis(500))).unwrap();

        let sender = OscSender::new().expect("open sender");
        sender.note(2, 60, 0.8);

        let mut buf = [0u8; 1024];
        let n = listener.recv(&mut buf).expect("received an OSC packet");
        let (_, pkt) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
        match pkt {
            OscPacket::Message(m) => {
                assert_eq!(m.addr, "/fig/track/2/note");
                assert_eq!(m.args, vec![OscType::Int(60), OscType::Float(0.8)]);
            }
            _ => panic!("expected an OSC message"),
        }
    }
}

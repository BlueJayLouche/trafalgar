// OSC output. The audio thread pushes note events over an mpsc channel; a
// background thread owns the socket and does the rosc encoding + UDP send, so the
// real-time path only does a channel send (no sockets, no encoding).
//
// OSC input (bottom of file). A background thread owns a listening socket and
// writes incoming pad control straight into `Shared` — the same atomics the mouse
// pad writes — so a phone (e.g. TouchOSC) drives the same live-performance path.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rosc::{encoder, OscMessage, OscPacket, OscType};

use crate::{Shared, NUM_TRACKS};

struct Note {
    track: u8,
    note: u8,
    vel: f32,
}

pub struct OscSender {
    tx: Sender<Note>,
}

impl OscSender {
    /// Returns `None` if the socket can't be opened or the target won't resolve
    /// (OSC just stays off).
    pub fn new(host: &str, port: u16) -> Option<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect((host, port)).ok()?;
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

// ---- OSC input: phone/TouchOSC -> the shared pad path ----------------------

/// A decoded control message. Parsed (and validated) separately from applying it
/// so the parser is unit-testable without a socket or `Shared`.
enum Msg {
    /// Pad position: track, x (pitch 0..1), y (density 0..1), both clamped/finite.
    Xy(usize, f32, f32),
    /// Pad-touch gate: track, open?
    Gate(usize, bool),
    /// Momentary erase: track, held?
    Erase(usize, bool),
    /// One-shot wipe of the track's recorded loop.
    Clear(usize),
}

/// Coerce an OSC arg to a finite f32 (accepts int/float/double). None = reject.
fn as_f32(t: &OscType) -> Option<f32> {
    let v = match t {
        OscType::Float(f) => *f,
        OscType::Int(i) => *i as f32,
        OscType::Double(d) => *d as f32,
        _ => return None,
    };
    v.is_finite().then_some(v)
}

/// Defensive parse of one OSC control message. This is a trust boundary (the
/// socket is open on the LAN), so anything malformed, out of range, or non-finite
/// returns None rather than reaching `Shared`.
fn parse(addr: &str, args: &[OscType]) -> Option<Msg> {
    let mut seg = addr.strip_prefix('/')?.split('/');
    if seg.next()? != "track" {
        return None;
    }
    let n: usize = seg.next()?.parse().ok()?;
    if n >= NUM_TRACKS {
        return None;
    }
    let kind = seg.next()?;
    if seg.next().is_some() {
        return None; // trailing junk in the address
    }
    match kind {
        "xy" => {
            if args.len() != 2 {
                return None;
            }
            let x = as_f32(&args[0])?.clamp(0.0, 1.0);
            let y = as_f32(&args[1])?.clamp(0.0, 1.0);
            Some(Msg::Xy(n, x, y))
        }
        "gate" => Some(Msg::Gate(n, as_f32(args.first()?)? >= 0.5)),
        "erase" => Some(Msg::Erase(n, as_f32(args.first()?)? >= 0.5)),
        "clear" => (as_f32(args.first()?)? >= 0.5).then_some(Msg::Clear(n)), // rising edge only
        _ => None,
    }
}

/// Write a parsed message into the same atomics the mouse pad writes. Ordering
/// mirrors `XyPad`: position is Relaxed, the gate is Release.
fn apply(shared: &Shared, m: Msg) {
    match m {
        Msg::Xy(n, x, y) => {
            let packed = ((x.to_bits() as u64) << 32) | y.to_bits() as u64;
            shared.pos[n].store(packed, Ordering::Relaxed);
        }
        Msg::Gate(n, open) => shared.gate[n].store(open, Ordering::Release),
        Msg::Erase(n, on) => shared.erase[n].store(on, Ordering::Relaxed),
        // ponytail: phone clear/erase are not undoable — the undo stack is GUI-only.
        Msg::Clear(n) => {
            for c in shared.gesture[n].iter() {
                c.store(-1, Ordering::Relaxed);
            }
        }
    }
}

fn handle_packet(shared: &Shared, packet: OscPacket) {
    match packet {
        OscPacket::Message(m) => {
            if let Some(msg) = parse(&m.addr, &m.args) {
                apply(shared, msg);
            }
        }
        OscPacket::Bundle(b) => {
            for p in b.content {
                handle_packet(shared, p);
            }
        }
    }
}

/// Listens for pad control and writes it into `Shared`. Owns one background thread
/// bound to the OSC-in port; dropping it stops the thread (no join, so teardown
/// never blocks the audio thread).
pub struct OscReceiver {
    stop: Arc<AtomicBool>,
}

impl OscReceiver {
    /// `None` if the port can't be bound (e.g. a sibling instance already has it) —
    /// the caller reflects that as a "port in use" status.
    pub fn new(shared: Arc<Shared>, lan: bool, port: u16) -> Option<Self> {
        let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
        let socket = UdpSocket::bind((host, port)).ok()?;
        // Timeout so the loop periodically re-checks `stop` and can exit.
        socket.set_read_timeout(Some(Duration::from_millis(100))).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while !st.load(Ordering::Relaxed) {
                // ponytail: one stale step is possible if a gate packet lands before
                // its first xy — self-corrects next step; not worth bundling to fix.
                if let Ok(n) = socket.recv(&mut buf) {
                    if let Ok((_, packet)) = rosc::decoder::decode_udp(&buf[..n]) {
                        handle_packet(&shared, packet);
                    }
                }
            }
        });
        Some(Self { stop })
    }
}

impl Drop for OscReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed); // thread self-exits within one timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_rejects_junk_and_clamps() {
        use rosc::OscType::Float;
        // valid
        assert!(matches!(parse("/track/0/xy", &[Float(0.5), Float(0.2)]), Some(Msg::Xy(0, _, _))));
        assert!(matches!(parse("/track/3/gate", &[Float(1.0)]), Some(Msg::Gate(3, true))));
        assert!(matches!(parse("/track/1/erase", &[Float(0.0)]), Some(Msg::Erase(1, false))));
        assert!(matches!(parse("/track/2/clear", &[Float(1.0)]), Some(Msg::Clear(2))));
        // junk / boundary — all rejected, none reach Shared
        assert!(parse("/track/0/xy", &[Float(0.5)]).is_none(), "wrong arity");
        assert!(parse("/track/9/xy", &[Float(0.1), Float(0.1)]).is_none(), "track out of range");
        assert!(parse("/track/0/xy", &[Float(f32::NAN), Float(0.1)]).is_none(), "NaN");
        assert!(parse("/track/0/xy", &[Float(0.1), Float(f32::INFINITY)]).is_none(), "inf");
        assert!(parse("/bogus", &[]).is_none(), "unknown address");
        assert!(parse("/track/0/xy/extra", &[Float(0.1), Float(0.1)]).is_none(), "trailing seg");
        assert!(parse("/track/0/clear", &[Float(0.0)]).is_none(), "below clear threshold");
        // clamping
        match parse("/track/0/xy", &[Float(2.0), Float(-1.0)]) {
            Some(Msg::Xy(_, x, y)) => {
                assert_eq!(x, 1.0);
                assert_eq!(y, 0.0);
            }
            _ => panic!("expected clamped xy"),
        }
    }

    fn test_shared() -> Arc<Shared> {
        use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicU8};
        Arc::new(Shared {
            gate: std::array::from_fn(|_| AtomicBool::new(false)),
            pos: std::array::from_fn(|_| AtomicU64::new(0)),
            step: std::array::from_fn(|_| AtomicI64::new(-1)),
            gesture: crate::Gesture::default(),
            erase: std::array::from_fn(|_| AtomicBool::new(false)),
            osc_dirty: AtomicBool::new(false),
            osc_in_dirty: AtomicBool::new(false),
            osc_in_status: AtomicU8::new(0),
            midi_dirty: AtomicBool::new(false),
            midi_status: AtomicU8::new(0),
            link_dirty: AtomicBool::new(false),
            link_peers: AtomicU32::new(0),
        })
    }

    /// End-to-end: a UDP packet into the receiver lands in `Shared` via the thread.
    #[test]
    fn receiver_writes_shared() {
        let shared = test_shared();
        let port = 9199; // fixed loopback port; skip if a sibling test/run holds it
        let Some(_rx) = OscReceiver::new(shared.clone(), false, port) else {
            return;
        };
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let send = |addr: &str, args: Vec<OscType>| {
            let buf = encoder::encode(&OscPacket::Message(OscMessage { addr: addr.into(), args })).unwrap();
            sock.send_to(&buf, ("127.0.0.1", port)).unwrap();
        };
        send("/track/1/xy", vec![OscType::Float(0.5), OscType::Float(0.25)]);
        send("/track/1/gate", vec![OscType::Float(1.0)]);

        let mut opened = false;
        for _ in 0..100 {
            if shared.gate[1].load(Ordering::Acquire) {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(opened, "gate never opened from OSC");
        let packed = shared.pos[1].load(Ordering::Relaxed);
        assert_eq!(f32::from_bits((packed >> 32) as u32), 0.5, "x");
        assert_eq!(f32::from_bits(packed as u32), 0.25, "y");
    }

    #[test]
    fn emits_note_osc() {
        // Bind the target first so we can receive what the sender emits.
        // If the port is busy, skip rather than fail flakily.
        // Ephemeral port so this test never collides with a live instance on 9000.
        let Ok(listener) = UdpSocket::bind("127.0.0.1:0") else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        listener.set_read_timeout(Some(Duration::from_millis(500))).unwrap();

        let sender = OscSender::new("127.0.0.1", port).expect("open sender");
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

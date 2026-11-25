//! Host side of kwvsock (virtio-vsock stream packets) and a client for
//! unkernel's `kwtty` exec protocol (guest port 1022, served by
//! `kwbox tty serve`): run a program, get stdout, stderr and the exit status
//! back separately.
//!
//! Everything runs on the store's thread, driven from the device imports:
//! `on_packet` for each packet the guest sends, `tick` when the kernel idles.
//!
//! ponytail: one exec at a time, and host->guest sends ignore the guest's
//! credit (an OPEN frame and stdin are far below any buffer it advertises).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const HOST_CID: u64 = 2;
const HDR: usize = 44;
const TTY_PORT: u32 = 1022;
const BUF_ALLOC: u32 = 1 << 20;

const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

/// What one exec produced.
#[derive(Debug, Default)]
pub struct ExecResult {
    pub cmd: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `Ok(code)` for an exit, `Err(signal)` for a signal.
    pub status: Option<Result<u8, u8>>,
    pub error: Option<String>,
    /// From sending the connect request to the OPENED reply.
    pub open_time: Duration,
    /// From the connect request to the EXIT frame.
    pub total_time: Duration,
    pub finished: Option<Instant>,
}

#[derive(PartialEq)]
enum State {
    Connecting,
    Open,
}

struct Run {
    port: u32,
    state: State,
    started: Instant,
    buf: Vec<u8>,
    fwd_cnt: u32,
    sent_fwd: u32,
    res: ExecResult,
}

pub struct Vsock {
    pub guest_cid: Option<u64>,
    /// Packets for the guest, oldest first.
    pub rx: VecDeque<Vec<u8>>,
    pub cmds: VecDeque<String>,
    pub results: Vec<ExecResult>,
    cur: Option<Run>,
    retry_at: Option<Instant>,
    next_port: u32,
    /// The tty server refused us this many times (it was not up yet).
    pub refused: u32,
}

impl Vsock {
    pub fn new(cmds: Vec<String>) -> Self {
        Vsock {
            guest_cid: None,
            rx: VecDeque::new(),
            cmds: cmds.into(),
            results: Vec::new(),
            cur: None,
            retry_at: None,
            next_port: 49152,
            refused: 0,
        }
    }

    /// Nothing queued or running.
    pub fn idle(&self) -> bool {
        self.cur.is_none() && self.cmds.is_empty()
    }

    /// When `tick` next has something to do, if it is waiting on time.
    pub fn deadline(&self) -> Option<Instant> {
        if self.cur.is_some() || self.cmds.is_empty() {
            None
        } else {
            self.retry_at
        }
    }

    fn packet(&mut self, port: u32, op: u16, flags: u32, fwd_cnt: u32, payload: &[u8]) {
        let Some(cid) = self.guest_cid else { return };
        let mut p = vec![0u8; HDR + payload.len()];
        p[0..8].copy_from_slice(&HOST_CID.to_le_bytes());
        p[8..16].copy_from_slice(&cid.to_le_bytes());
        p[16..20].copy_from_slice(&port.to_le_bytes());
        p[20..24].copy_from_slice(&TTY_PORT.to_le_bytes());
        p[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        p[28..30].copy_from_slice(&1u16.to_le_bytes()); // stream
        p[30..32].copy_from_slice(&op.to_le_bytes());
        p[32..36].copy_from_slice(&flags.to_le_bytes());
        p[36..40].copy_from_slice(&BUF_ALLOC.to_le_bytes());
        p[40..44].copy_from_slice(&fwd_cnt.to_le_bytes());
        p[HDR..].copy_from_slice(payload);
        self.rx.push_back(p);
    }

    /// Starts the next command if nothing is running. Returns whether it queued packets.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.cur.is_some() || self.guest_cid.is_none() || self.retry_at.is_some_and(|t| now < t)
        {
            return false;
        }
        let Some(cmd) = self.cmds.pop_front() else {
            return false;
        };
        let port = self.next_port;
        self.next_port += 1;
        self.cur = Some(Run {
            port,
            state: State::Connecting,
            started: now,
            buf: Vec::new(),
            fwd_cnt: 0,
            sent_fwd: 0,
            res: ExecResult {
                cmd,
                ..Default::default()
            },
        });
        self.packet(port, OP_REQUEST, 0, 0, &[]);
        true
    }

    /// A packet from the guest. Returns whether it queued packets in reply.
    pub fn on_packet(&mut self, pkt: &[u8]) -> bool {
        if pkt.len() < HDR {
            return false;
        }
        let le32 = |i: usize| u32::from_le_bytes(pkt[i..i + 4].try_into().unwrap());
        let (src_port, dst_port, len) = (le32(16), le32(20), le32(24) as usize);
        let op = u16::from_le_bytes([pkt[30], pkt[31]]);
        let before = self.rx.len();
        let ours = self
            .cur
            .as_ref()
            .is_some_and(|r| r.port == dst_port && src_port == TTY_PORT);
        if !ours {
            if op != OP_RST {
                // not a connection of ours: refuse it
                let mut p = pkt[..HDR].to_vec();
                p[0..16].rotate_left(8);
                p[16..24].rotate_left(4);
                p[24..28].fill(0);
                p[30..32].copy_from_slice(&OP_RST.to_le_bytes());
                self.rx.push_back(p);
            }
            return self.rx.len() > before;
        }
        let mut run = self.cur.take().unwrap();
        match op {
            OP_RESPONSE if run.state == State::Connecting => {
                run.state = State::Open;
                let open = tty_frame(1, &open_payload(&run.res.cmd));
                let eof = tty_frame(7, &[]);
                self.packet(run.port, OP_RW, 0, run.fwd_cnt, &[open, eof].concat());
            }
            OP_RST if run.state == State::Connecting => {
                // the tty server is not listening yet: try again shortly
                self.refused += 1;
                self.cmds.push_front(std::mem::take(&mut run.res.cmd));
                self.retry_at = Some(Instant::now() + Duration::from_millis(20));
                return self.rx.len() > before;
            }
            OP_RW => {
                run.fwd_cnt = run.fwd_cnt.wrapping_add(len as u32);
                run.buf
                    .extend_from_slice(&pkt[HDR..HDR + len.min(pkt.len() - HDR)]);
                if self.frames(&mut run) {
                    self.packet(run.port, OP_RST, 0, run.fwd_cnt, &[]);
                    self.finish(run);
                    return true;
                }
                if run.fwd_cnt.wrapping_sub(run.sent_fwd) >= BUF_ALLOC / 2 {
                    run.sent_fwd = run.fwd_cnt;
                    self.packet(run.port, OP_CREDIT_UPDATE, 0, run.fwd_cnt, &[]);
                }
            }
            OP_CREDIT_REQUEST => {
                run.sent_fwd = run.fwd_cnt;
                self.packet(run.port, OP_CREDIT_UPDATE, 0, run.fwd_cnt, &[]);
            }
            OP_CREDIT_UPDATE => {}
            OP_SHUTDOWN | OP_RST => {
                if op == OP_SHUTDOWN {
                    self.packet(run.port, OP_RST, 0, run.fwd_cnt, &[]);
                }
                run.res
                    .error
                    .get_or_insert_with(|| "the guest closed the session before EXIT".into());
                self.finish(run);
                return self.rx.len() > before;
            }
            _ => {
                run.res.error = Some(format!("unexpected vsock op {op}"));
                self.packet(run.port, OP_RST, 0, run.fwd_cnt, &[]);
                self.finish(run);
                return true;
            }
        }
        self.cur = Some(run);
        self.rx.len() > before
    }

    /// Consumes whole kwtty frames; returns true once the session ended.
    fn frames(&mut self, run: &mut Run) -> bool {
        while run.buf.len() >= 4 {
            let n = u16::from_le_bytes([run.buf[2], run.buf[3]]) as usize;
            if run.buf.len() < 4 + n {
                break;
            }
            let ty = run.buf[0];
            let payload: Vec<u8> = run.buf.drain(..4 + n).skip(4).collect();
            match ty {
                1 => run.res.open_time = run.started.elapsed(),
                2 => run.res.stdout.extend_from_slice(&payload),
                8 => run.res.stderr.extend_from_slice(&payload),
                4 if payload.len() >= 3 => {
                    run.res.status = Some(if payload[0] == 0 {
                        Ok(payload[1])
                    } else {
                        Err(payload[1])
                    });
                    return true;
                }
                5 => {
                    run.res.error = Some(String::from_utf8_lossy(&payload).into_owned());
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn finish(&mut self, mut run: Run) {
        run.res.total_time = run.started.elapsed();
        run.res.finished = Some(Instant::now());
        // as they finish, so a long run still shows its progress
        eprintln!(
            "[exec-done] {:?}: {:?} in {:.1} ms\n[exec-done]   stdout: {:?}\n[exec-done]   stderr: {:?}",
            run.res.cmd,
            run.res.status,
            run.res.total_time.as_secs_f64() * 1e3,
            String::from_utf8_lossy(&run.res.stdout),
            String::from_utf8_lossy(&run.res.stderr),
        );
        self.results.push(run.res);
        self.retry_at = None;
    }
}

fn tty_frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![ty, 0];
    f.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// kwtty OPEN: `sh -c CMD` on pipes, as root, in /root.
fn open_payload(cmd: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1u16.to_le_bytes()); // version
    p.extend_from_slice(&[0; 8]); // rows, cols, xpixel, ypixel: no pty
    let mut opt = |tag: u8, v: &[u8]| {
        p.push(tag);
        p.extend_from_slice(&(v.len() as u16).to_le_bytes());
        p.extend_from_slice(v);
    };
    for a in ["/bin/sh", "-c", cmd] {
        opt(131, a.as_bytes());
    }
    opt(132, b"/root");
    opt(133, &[]); // pipes
    opt(7, &[]); // user: root
    p
}

//! Host side of kwnet: a slirp-style user-mode network, as in unkernel's JS
//! `NetStack`. The guest is 10.0.2.15 behind gateway 10.0.2.2 and DNS
//! 10.0.2.3; ARP, DHCP and DNS are answered here, and each guest TCP
//! connection is terminated into a host `TcpStream`.
//!
//! The link to the guest never drops frames, so there is no retransmission
//! or congestion control: we honour the guest's receive window and ACK what
//! it sends as soon as it is queued for the host socket.
//!
//! ponytail: no network policy at all (any host/port the guest names). In
//! eryx this is where the allow/block lists and resolved-IP checks would go.
//! ponytail: no UDP beyond DHCP/DNS, no ICMP unreachable, no IPv6.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::Event;

const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const NET: u32 = 0x0a00_0200;
const MASK: u32 = 0xffff_ff00;
const GW: u32 = 0x0a00_0202;
const DNS: u32 = 0x0a00_0203;
const GUEST: u32 = 0x0a00_020f;

const FIN: u8 = 1;
const SYN: u8 = 2;
const RST: u8 = 4;
const PSH: u8 = 8;
const ACK: u8 = 16;
const MSS: usize = 1460;
const OUR_WINDOW: u16 = 65535;

type Key = (u16, u32, u16); // guest port, remote ip, remote port
type Key3 = (u32, u16, u16); // remote ip, remote port, guest port

struct Shared {
    tx: Sender<Event>,
    ip_id: Mutex<u16>,
}

impl Shared {
    fn emit(&self, f: Vec<u8>) {
        let _ = self.tx.send(Event::Frame(f));
    }

    fn ip_frame(&self, src: u32, dst: u32, proto: u8, l4: &[u8], dst_mac: [u8; 6]) -> Vec<u8> {
        let mut f = Vec::with_capacity(34 + l4.len());
        f.extend_from_slice(&dst_mac);
        f.extend_from_slice(&GW_MAC);
        f.extend_from_slice(&[0x08, 0x00]);
        let id = {
            let mut g = self.ip_id.lock().unwrap();
            *g = g.wrapping_add(1);
            *g
        };
        let total = (20 + l4.len()) as u16;
        f.extend_from_slice(&[0x45, 0]);
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&id.to_be_bytes());
        f.extend_from_slice(&[0x40, 0, 64, proto, 0, 0]);
        f.extend_from_slice(&src.to_be_bytes());
        f.extend_from_slice(&dst.to_be_bytes());
        let c = fold(sum16(&f[14..34], 0));
        f[24..26].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(l4);
        f
    }

    /// `l4` with its checksum field zeroed at `csum_at`.
    fn l4_frame(
        &self,
        src: u32,
        dst: u32,
        proto: u8,
        mut l4: Vec<u8>,
        csum_at: usize,
        dst_mac: [u8; 6],
    ) -> Vec<u8> {
        let pseudo = (src >> 16)
            + (src & 0xffff)
            + (dst >> 16)
            + (dst & 0xffff)
            + proto as u32
            + l4.len() as u32;
        let mut c = fold(sum16(&l4, pseudo));
        if proto == 17 && c == 0 {
            c = 0xffff;
        }
        l4[csum_at..csum_at + 2].copy_from_slice(&c.to_be_bytes());
        self.ip_frame(src, dst, proto, &l4, dst_mac)
    }

    fn udp(&self, src: u32, sport: u16, dst: u32, dport: u16, payload: &[u8], dst_mac: [u8; 6]) {
        let mut l4 = Vec::with_capacity(8 + payload.len());
        l4.extend_from_slice(&sport.to_be_bytes());
        l4.extend_from_slice(&dport.to_be_bytes());
        l4.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        l4.extend_from_slice(&[0, 0]);
        l4.extend_from_slice(payload);
        let f = self.l4_frame(src, dst, 17, l4, 6, dst_mac);
        self.emit(f);
    }

    /// A TCP segment from `(remote ip, remote port, guest port)` to the guest.
    fn tcp(
        &self,
        (ip, port, gport): Key3,
        flags: u8,
        seq: u32,
        ack: u32,
        payload: &[u8],
        mss_opt: bool,
    ) {
        let hl = if mss_opt { 24 } else { 20 };
        let mut l4 = Vec::with_capacity(hl + payload.len());
        l4.extend_from_slice(&port.to_be_bytes());
        l4.extend_from_slice(&gport.to_be_bytes());
        l4.extend_from_slice(&seq.to_be_bytes());
        l4.extend_from_slice(&(if flags & ACK != 0 { ack } else { 0 }).to_be_bytes());
        l4.extend_from_slice(&[((hl / 4) << 4) as u8, flags]);
        l4.extend_from_slice(&OUR_WINDOW.to_be_bytes());
        l4.extend_from_slice(&[0, 0, 0, 0]); // csum, urg
        if mss_opt {
            l4.extend_from_slice(&[2, 4]);
            l4.extend_from_slice(&(MSS as u16).to_be_bytes());
        }
        l4.extend_from_slice(payload);
        let f = self.l4_frame(ip, GUEST, 6, l4, 16, GUEST_MAC);
        self.emit(f);
    }
}

struct ConnState {
    snd_una: u32,
    snd_nxt: u32,
    rcv_nxt: u32,
    wnd: u32,
    closed: bool,
}

struct Conn {
    irs: u32,
    ip: u32,
    port: u16,
    gport: u16,
    st: Mutex<ConnState>,
    cv: Condvar,
    to_host: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
}

impl Conn {
    fn send(&self, sh: &Shared, flags: u8, seq: u32, ack: u32, payload: &[u8]) {
        sh.tcp(
            (self.ip, self.port, self.gport),
            flags,
            seq,
            ack,
            payload,
            false,
        );
    }
    fn close(&self) {
        self.st.lock().unwrap().closed = true;
        self.cv.notify_all();
        self.to_host.lock().unwrap().take();
    }
}

pub struct Net {
    sh: Arc<Shared>,
    tcp: HashMap<Key, Arc<Conn>>,
}

impl Net {
    pub fn new(tx: Sender<Event>) -> Self {
        Net {
            sh: Arc::new(Shared {
                tx,
                ip_id: Mutex::new(1),
            }),
            tcp: HashMap::new(),
        }
    }
    pub fn guest_mac(&self) -> [u8; 6] {
        GUEST_MAC
    }

    /// A frame from the guest.
    pub fn send(&mut self, f: &[u8]) {
        if f.len() < 14 {
            return;
        }
        match u16::from_be_bytes([f[12], f[13]]) {
            0x0806 => self.arp(f),
            0x0800 => self.ipv4(f),
            _ => {}
        }
    }

    fn arp(&self, f: &[u8]) {
        if f.len() < 42 || be16(f, 20) != 1 {
            return;
        }
        let tip = be32(f, 38);
        let sip = be32(f, 28);
        if tip == sip || tip == GUEST || tip & MASK != NET {
            return;
        }
        let mut r = vec![0u8; 42];
        r[0..6].copy_from_slice(&f[22..28]);
        r[6..12].copy_from_slice(&GW_MAC);
        r[12..14].copy_from_slice(&[0x08, 0x06]);
        r[14..22].copy_from_slice(&[0, 1, 0x08, 0, 6, 4, 0, 2]);
        r[22..28].copy_from_slice(&GW_MAC);
        r[28..32].copy_from_slice(&tip.to_be_bytes());
        r[32..38].copy_from_slice(&f[22..28]);
        r[38..42].copy_from_slice(&sip.to_be_bytes());
        self.sh.emit(r);
    }

    fn ipv4(&mut self, f: &[u8]) {
        if f.len() < 34 || f[14] >> 4 != 4 {
            return;
        }
        let ihl = (f[14] & 15) as usize * 4;
        let total = be16(f, 16) as usize;
        if be16(f, 20) & 0x3fff != 0 {
            return; // fragments
        }
        let proto = f[23];
        let (src, dst) = (be32(f, 26), be32(f, 30));
        let l4 = 14 + ihl;
        let end = if total > 0 {
            f.len().min(14 + total)
        } else {
            f.len()
        };
        if end < l4 {
            return;
        }
        let seg = &f[l4..end];
        match proto {
            6 => self.tcp_input(seg, dst),
            17 => self.udp_input(seg, src, dst),
            1 => self.icmp_input(seg, src, dst),
            _ => {}
        }
    }

    fn icmp_input(&self, p: &[u8], src: u32, dst: u32) {
        if p.is_empty() || p[0] != 8 || (dst != GW && dst != DNS) {
            return;
        }
        let mut r = p.to_vec();
        r[0] = 0;
        r[2] = 0;
        r[3] = 0;
        let c = fold(sum16(&r, 0));
        r[2..4].copy_from_slice(&c.to_be_bytes());
        let f = self.sh.ip_frame(dst, src, 1, &r, GUEST_MAC);
        self.sh.emit(f);
    }

    fn udp_input(&self, p: &[u8], src: u32, dst: u32) {
        if p.len() < 8 {
            return;
        }
        let (sport, dport) = (be16(p, 0), be16(p, 2));
        let payload = &p[8..p.len().min(be16(p, 4) as usize)];
        if dport == 67 {
            self.dhcp(payload);
        } else if dport == 53 && (dst == DNS || dst == GW) {
            let q = payload.to_vec();
            let sh = self.sh.clone();
            std::thread::spawn(move || dns(&sh, &q, src, sport, dst));
        }
    }

    fn dhcp(&self, p: &[u8]) {
        if p.len() < 240 || p[0] != 1 || be32(p, 236) != 0x6382_5363 {
            return;
        }
        let mut ty = 0;
        let mut i = 240;
        while i + 1 < p.len() && p[i] != 255 {
            if p[i] == 0 {
                i += 1;
                continue;
            }
            if p[i] == 53 {
                ty = p[i + 2];
            }
            i += 2 + p[i + 1] as usize;
        }
        let reply = match ty {
            1 => 2,
            3 | 8 => 5,
            _ => return,
        };
        let mut o = vec![53, 1, reply, 54, 4];
        o.extend_from_slice(&GW.to_be_bytes());
        if ty != 8 {
            o.extend_from_slice(&[51, 4]);
            o.extend_from_slice(&86400u32.to_be_bytes());
        }
        for (code, v) in [(1, MASK), (3, GW), (6, DNS), (28, NET | !MASK)] {
            o.extend_from_slice(&[code, 4]);
            o.extend_from_slice(&v.to_be_bytes());
        }
        o.push(255);
        let mut r = vec![0u8; 240];
        r[0..3].copy_from_slice(&[2, 1, 6]);
        r[4..8].copy_from_slice(&p[4..8]);
        r[10..16].copy_from_slice(&p[10..16]);
        if ty != 8 {
            r[16..20].copy_from_slice(&GUEST.to_be_bytes());
        }
        r[20..24].copy_from_slice(&GW.to_be_bytes());
        r[28..44].copy_from_slice(&p[28..44]);
        r[236..240].copy_from_slice(&0x6382_5363u32.to_be_bytes());
        r.extend_from_slice(&o);
        self.sh.udp(GW, 67, 0xffff_ffff, 68, &r, [0xff; 6]);
    }

    fn tcp_input(&mut self, p: &[u8], dst: u32) {
        if p.len() < 20 {
            return;
        }
        let hl = (p[12] >> 4) as usize * 4;
        let (sport, dport) = (be16(p, 0), be16(p, 2));
        let (seq, ack, flags, win) = (be32(p, 4), be32(p, 8), p[13], be16(p, 14) as u32);
        let payload = &p[hl.min(p.len())..];
        let key = (sport, dst, dport);

        if flags & (SYN | ACK) == SYN {
            if self.tcp.get(&key).is_some_and(|c| c.irs == seq) {
                return; // the guest retrying while we connect
            }
            if let Some(old) = self.tcp.remove(&key) {
                old.close();
            }
            let Some(addr) = backend_addr(dst, dport) else {
                self.sh.tcp(
                    (dst, dport, sport),
                    RST | ACK,
                    0,
                    seq.wrapping_add(1),
                    &[],
                    false,
                );
                return;
            };
            let iss: u32 = 0x1000_0000 ^ (sport as u32) << 8 ^ dport as u32;
            let c = Arc::new(Conn {
                irs: seq,
                ip: dst,
                port: dport,
                gport: sport,
                st: Mutex::new(ConnState {
                    snd_una: iss,
                    snd_nxt: iss.wrapping_add(1),
                    rcv_nxt: seq.wrapping_add(1),
                    wnd: win,
                    closed: false,
                }),
                cv: Condvar::new(),
                to_host: Mutex::new(None),
            });
            self.tcp.insert(key, c.clone());
            let sh = self.sh.clone();
            std::thread::spawn(move || connect(sh, c, addr, iss));
            return;
        }

        let Some(c) = self.tcp.get(&key).cloned() else {
            if flags & RST == 0 {
                let (s, a, fl) = if flags & ACK != 0 {
                    (ack, 0, RST)
                } else {
                    (
                        0,
                        seq.wrapping_add(payload.len() as u32 + (flags & FIN) as u32),
                        RST | ACK,
                    )
                };
                self.sh.tcp((dst, dport, sport), fl, s, a, &[], false);
            }
            return;
        };
        if flags & RST != 0 {
            c.close();
            self.tcp.remove(&key);
            return;
        }
        let mut st = c.st.lock().unwrap();
        if flags & ACK != 0 {
            st.snd_una = ack;
            st.wnd = win;
            c.cv.notify_all();
        }
        let mut need_ack = false;
        if !payload.is_empty() || flags & FIN != 0 {
            if seq != st.rcv_nxt {
                need_ack = true; // out of order / retransmit: dup-ack
            } else {
                if !payload.is_empty() {
                    if let Some(w) = c.to_host.lock().unwrap().as_ref() {
                        let _ = w.send(payload.to_vec());
                    }
                    st.rcv_nxt = st.rcv_nxt.wrapping_add(payload.len() as u32);
                }
                if flags & FIN != 0 {
                    st.rcv_nxt = st.rcv_nxt.wrapping_add(1);
                    c.to_host.lock().unwrap().take(); // writer shuts down its half
                }
                need_ack = true;
            }
        }
        let done = st.closed && flags & FIN != 0;
        let (snd_nxt, rcv_nxt) = (st.snd_nxt, st.rcv_nxt);
        drop(st);
        if need_ack {
            c.send(&self.sh, ACK, snd_nxt, rcv_nxt, &[]);
        }
        if done {
            self.tcp.remove(&key);
        }
    }
}

/// Where a guest connection to `ip:port` goes on the host, if anywhere.
fn backend_addr(ip: u32, port: u16) -> Option<SocketAddr> {
    if ip & MASK == NET || ip == 0xffff_ffff || ip >> 28 == 14 || ip >> 24 == 0 || ip >> 24 == 127 {
        return None;
    }
    Some(SocketAddr::from((Ipv4Addr::from(ip), port)))
}

fn connect(sh: Arc<Shared>, c: Arc<Conn>, addr: SocketAddr, iss: u32) {
    let rcv = c.st.lock().unwrap().rcv_nxt;
    let sock = match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
        Ok(s) => s,
        Err(_) => {
            c.send(&sh, RST | ACK, 0, rcv, &[]);
            c.close();
            return;
        }
    };
    let _ = sock.set_nodelay(true);
    let (wtx, wrx) = mpsc::channel::<Vec<u8>>();
    *c.to_host.lock().unwrap() = Some(wtx);
    let mut wsock = sock.try_clone().unwrap();
    std::thread::spawn(move || {
        for chunk in wrx {
            if wsock.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = wsock.shutdown(Shutdown::Write);
    });
    sh.tcp((c.ip, c.port, c.gport), SYN | ACK, iss, rcv, &[], true);

    // host -> guest
    let mut rsock = sock;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let room = {
            let mut st = c.st.lock().unwrap();
            loop {
                if st.closed {
                    return;
                }
                let in_flight = st.snd_nxt.wrapping_sub(st.snd_una);
                if st.wnd > in_flight {
                    break (st.wnd - in_flight) as usize;
                }
                st = c.cv.wait(st).unwrap();
            }
        };
        let want = room.min(buf.len());
        let n = match rsock.read(&mut buf[..want]) {
            Ok(0) | Err(_) => 0,
            Ok(n) => n,
        };
        let mut st = c.st.lock().unwrap();
        if st.closed {
            return;
        }
        if n == 0 {
            c.send(&sh, FIN | ACK, st.snd_nxt, st.rcv_nxt, &[]);
            st.snd_nxt = st.snd_nxt.wrapping_add(1);
            st.closed = true;
            return;
        }
        for chunk in buf[..n].chunks(MSS) {
            c.send(&sh, ACK | PSH, st.snd_nxt, st.rcv_nxt, chunk);
            st.snd_nxt = st.snd_nxt.wrapping_add(chunk.len() as u32);
        }
    }
}

fn dns(sh: &Shared, q: &[u8], src: u32, sport: u16, dst: u32) {
    if q.len() < 12 || q[2] & 0x80 != 0 || be16(q, 4) != 1 {
        return;
    }
    let mut labels = Vec::new();
    let mut i = 12;
    while i < q.len() && q[i] != 0 {
        if q[i] & 0xc0 != 0 {
            return;
        }
        let l = q[i] as usize;
        labels.push(String::from_utf8_lossy(&q[i + 1..(i + 1 + l).min(q.len())]).into_owned());
        i += 1 + l;
    }
    if i + 5 > q.len() {
        return;
    }
    let (qtype, qclass) = (be16(q, i + 1), be16(q, i + 3));
    let qend = i + 5;
    let mut rcode = 0u16;
    let mut addrs = Vec::new();
    if qtype == 1 && qclass == 1 {
        match (labels.join("."), 0).to_socket_addrs() {
            Ok(it) => {
                for a in it {
                    if let SocketAddr::V4(v4) = a
                        && !addrs.contains(v4.ip())
                    {
                        addrs.push(*v4.ip());
                    }
                }
                if addrs.is_empty() {
                    rcode = 3;
                }
            }
            Err(_) => rcode = 3,
        }
    }
    let mut r = q[..qend].to_vec();
    let fl = ((q[2] as u16 & 1) << 8) | 0x8080 | rcode;
    r[2..4].copy_from_slice(&fl.to_be_bytes());
    r[6..8].copy_from_slice(&(addrs.len() as u16).to_be_bytes());
    r[8..12].fill(0);
    for a in addrs {
        r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        r.extend_from_slice(&a.octets());
    }
    sh.udp(dst, 53, src, sport, &r, GUEST_MAC);
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}
fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}
fn sum16(b: &[u8], init: u32) -> u32 {
    let mut s = init;
    for c in b.chunks(2) {
        s += if c.len() == 2 {
            u16::from_be_bytes([c[0], c[1]]) as u32
        } else {
            (c[0] as u32) << 8
        };
    }
    s
}
fn fold(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

//! Spike: boot unkernel (Linux compiled to wasm + an aarch64 user-mode CPU)
//! under wasmtime, with stack switching standing in for the JS host's JSPI.
//!
//! Port of the host half of unkernel's JS SDK (`dist/_chunks/core.mjs`), only
//! the parts a headless shell needs: console, initrd, block disks, network.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wasmtime::error::Context;
use wasmtime::*;

mod net;

const ERRNO_SUCCESS: i32 = 0;
const ERRNO_AGAIN: i32 = 6;
const ERRNO_BADF: i32 = 8;
const ERRNO_INVAL: i32 = 28;
const ERRNO_NOENT: i32 = 44;
const ERRNO_ROFS: i32 = 69;
const ENODEV: i64 = -19;

// IRQ bits in the kernel's `wasm_host_irqs` word (core.mjs).
const IRQ_CONSOLE: u32 = 2;
const IRQ_NET: u32 = 4;
const IRQ_BLOCK: u32 = 8;

/// Something the host side produced for the guest, delivered on the store's thread.
pub enum Event {
    Input(Vec<u8>),
    Frame(Vec<u8>),
}

struct Disk {
    file: File,
    size: u64,
    read_only: bool,
}

struct Host {
    memory: Option<Memory>,
    irq_addr: Option<usize>,
    args: Vec<Vec<u8>>,
    initrd: Vec<u8>,
    files: HashMap<i32, usize>, // fd -> read position in initrd
    input: VecDeque<u8>,
    net_rx: VecDeque<Vec<u8>>,
    net: Option<net::Net>,
    events: Receiver<Event>,
    disks: Vec<Disk>,
    slots: HashMap<i64, i32>,
    free_slots: Vec<i32>,
    next_slot: i32,
    start: Instant,
    stderr: Vec<u8>,
    exit: Option<i32>,
    stats: Stats,
    jit: bool,
    table: Option<Table>,
    jit_saved: HashMap<i32, Option<Func>>,
    jit_cache: HashMap<Vec<u8>, Module>,
    ready_at: Option<Duration>,
    t0: Instant,
    line: Vec<u8>,
    marks: HashMap<String, Instant>,
}

#[derive(Default, Debug)]
struct Stats {
    switches: u64,
    idles: u64,
    block_reads: u64,
    block_bytes: u64,
    jit_compiled: u64,
    jit_failed: u64,
    jit_compile_ms: f64,
    jit_cache_hits: u64,
}

impl Host {
    fn mono_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
    fn deliver(&mut self, ev: Event, mem: &mut [u8]) {
        match ev {
            Event::Input(b) => {
                self.input.extend(b);
                self.set_irq(mem, IRQ_CONSOLE);
            }
            Event::Frame(f) => {
                self.net_rx.push_back(f);
                self.set_irq(mem, IRQ_NET);
            }
        }
    }
    fn set_irq(&self, mem: &mut [u8], bits: u32) {
        if let Some(a) = self.irq_addr {
            let v = u32::from_le_bytes(mem[a..a + 4].try_into().unwrap()) | bits;
            mem[a..a + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
}

fn rd32(m: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(m[p..p + 4].try_into().unwrap())
}
fn rd64(m: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(m[p..p + 8].try_into().unwrap())
}
fn wr32(m: &mut [u8], p: usize, v: u32) {
    m[p..p + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr64(m: &mut [u8], p: usize, v: u64) {
    m[p..p + 8].copy_from_slice(&v.to_le_bytes());
}

/// Splits `caller` into the host state and guest memory.
fn parts<'a>(caller: &'a mut Caller<'_, Host>) -> (&'a mut [u8], &'a mut Host) {
    let mem = caller.data().memory.unwrap();
    mem.data_and_store_mut(caller)
}

struct Opts {
    kernel: String,
    distro: Option<String>,
    data: Option<String>,
    mem_mib: u64,
    input: Option<String>,
    timeout: Option<Duration>,
    net: bool,
    jit: bool,
    append: String,
}

fn parse_args() -> Result<Opts> {
    let mut o = Opts {
        kernel: String::new(),
        distro: None,
        data: None,
        mem_mib: 512,
        input: None,
        timeout: None,
        net: true,
        jit: true,
        append: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format_err!("{a} needs a value"));
        match a.as_str() {
            "--kernel" => o.kernel = val()?,
            "--distro" => o.distro = Some(val()?),
            "--data" => o.data = Some(val()?),
            "--mem" => o.mem_mib = val()?.parse()?,
            "--input" => o.input = Some(val()?),
            "--timeout" => o.timeout = Some(Duration::from_secs_f64(val()?.parse()?)),
            "--no-net" => o.net = false,
            "--no-jit" => o.jit = false,
            "--append" => o.append = val()?,
            _ => bail!("unknown argument {a}"),
        }
    }
    if o.kernel.is_empty() {
        bail!(
            "usage: unkernel-spike --kernel unkernel.wasm [--distro alpine.erofs] [--data data.img] [--mem MiB] [--input SCRIPT] [--timeout S] [--no-net]"
        );
    }
    Ok(o)
}

fn custom_section(bytes: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    for p in wasmparser::Parser::new(0).parse_all(bytes) {
        if let wasmparser::Payload::CustomSection(c) = p?
            && c.name() == name
        {
            return Ok(Some(c.data().to_vec()));
        }
    }
    Ok(None)
}

/// `--input` script, as in unkernel's CLI: `{N}` waits N seconds, `\n` is a newline.
fn spawn_input(script: Option<String>, tx: Sender<Event>) {
    std::thread::spawn(move || match script {
        Some(s) => {
            let s = s.replace("\\n", "\n");
            let mut rest = s.as_str();
            while !rest.is_empty() {
                if let Some(r) = rest.strip_prefix('{')
                    && let Some(end) = r.find('}')
                    && let Ok(secs) = r[..end].parse::<f64>()
                {
                    std::thread::sleep(Duration::from_secs_f64(secs));
                    rest = &r[end + 1..];
                    continue;
                }
                let n = rest[1..].find('{').map_or(rest.len(), |i| i + 1);
                if tx
                    .send(Event::Input(rest.as_bytes()[..n].to_vec()))
                    .is_err()
                {
                    return;
                }
                rest = &rest[n..];
            }
        }
        None => {
            let mut buf = [0u8; 4096];
            let mut stdin = std::io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || tx.send(Event::Input(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    });
}

fn main() -> Result<()> {
    let o = parse_args()?;
    let t0 = Instant::now();
    let bytes = std::fs::read(&o.kernel).with_context(|| format!("reading {}", o.kernel))?;
    let initrd = custom_section(&bytes, "unkernel.initrd")?.unwrap_or_default();
    let mut cmdline = custom_section(&bytes, "unkernel.cmdline")?
        .map(|c| String::from_utf8_lossy(&c).into_owned())
        .unwrap_or_default();
    if !o.append.is_empty() {
        cmdline = format!("{cmdline} {}", o.append);
    }

    let mut cfg = Config::new();
    cfg.wasm_function_references(true)
        .wasm_exceptions(true)
        .wasm_stack_switching(true)
        .max_wasm_stack(1 << 20)
        .async_stack_size(2 << 20);
    let engine = Engine::new(&cfg)?;
    // eryx-style AOT: compile once, then load the native code
    let cwasm = format!("{}.cwasm", o.kernel);
    let kernel = match std::fs::read(&cwasm) {
        Ok(b) => {
            unsafe { Module::deserialize(&engine, &b) }.context("loading the precompiled kernel")?
        }
        Err(_) => {
            let m = Module::new(&engine, &bytes).context("compiling the kernel")?;
            std::fs::write(&cwasm, m.serialize()?)?;
            m
        }
    };
    let switcher = Module::new(&engine, include_str!("switcher.wat"))?;
    eprintln!("[spike] compiled in {:?}", t0.elapsed());

    let (tx, rx) = mpsc::channel();
    spawn_input(o.input.clone(), tx.clone());

    let mut disks = Vec::new();
    for (path, ro) in [(&o.distro, true), (&o.data, false)] {
        if let Some(p) = path {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!ro)
                .open(p)
                .with_context(|| format!("opening {p}"))?;
            let size = file.metadata()?.len();
            disks.push(Disk {
                file,
                size,
                read_only: ro,
            });
        }
    }

    let mut args = vec![b"vmlinux\0".to_vec()];
    if !cmdline.is_empty() {
        args.push(format!("{cmdline}\0").into_bytes());
    }
    let host = Host {
        memory: None,
        irq_addr: None,
        args,
        initrd,
        files: HashMap::new(),
        input: VecDeque::new(),
        net_rx: VecDeque::new(),
        net: o.net.then(|| net::Net::new(tx.clone())),
        events: rx,
        disks,
        slots: HashMap::new(),
        free_slots: Vec::new(),
        next_slot: 0,
        start: Instant::now(),
        stderr: Vec::new(),
        exit: None,
        stats: Stats::default(),
        jit: o.jit,
        table: None,
        jit_saved: HashMap::new(),
        jit_cache: HashMap::new(),
        ready_at: None,
        t0,
        line: Vec::new(),
        marks: HashMap::new(),
    };
    let mut store = Store::new(&engine, host);
    let pages = (o.mem_mib * 1024 * 1024 / 65536) as u32;
    let memory = Memory::new(&mut store, MemoryType::new(pages, Some(pages))?)?;
    store.data_mut().memory = Some(memory);

    let mut linker = Linker::new(&engine);
    linker.func_wrap(
        "host",
        "slot",
        |mut c: Caller<'_, Host>, task: i64, alloc: i32| -> i32 {
            let h = c.data_mut();
            if alloc != 0 {
                h.stats.switches += 1;
                if let Some(&s) = h.slots.get(&task) {
                    return s;
                }
                let s = h.free_slots.pop().unwrap_or_else(|| {
                    h.next_slot += 1;
                    h.next_slot - 1
                });
                h.slots.insert(task, s);
                s
            } else {
                match h.slots.remove(&task) {
                    Some(s) => {
                        h.free_slots.push(s);
                        s
                    }
                    None => 0, // the table slot will be null -> unknown_task
                }
            }
        },
    )?;
    linker.func_wrap("host", "unknown_task", |task: i64| {
        eprintln!("[spike] fatal: switch to unknown task {task:#x}");
    })?;
    let sw = linker.instantiate(&mut store, &switcher)?;
    linker.instance(&mut store, "switcher", sw)?;
    linker.alias("switcher", "wasm_switch_to", "unkernel", "wasm_switch_to")?;
    linker.define(&mut store, "env", "memory", memory)?;
    define_wasi(&mut linker)?;
    define_devices(&mut linker)?;

    let inst = linker
        .instantiate(&mut store, &kernel)
        .context("instantiating the kernel")?;
    let irq = match inst
        .get_global(&mut store, "wasm_host_irqs")
        .context("no wasm_host_irqs")?
        .get(&mut store)
    {
        Val::I32(v) => v as u32 as usize,
        Val::I64(v) => v as usize,
        v => bail!("wasm_host_irqs is {v:?}"),
    };
    store.data_mut().irq_addr = Some(irq);
    store.data_mut().table = inst.get_table(&mut store, "__indirect_function_table");
    let entries = sw.get_table(&mut store, "entries").unwrap();
    for (i, name) in ["_start", "ret_from_fork"].iter().enumerate() {
        let f = inst
            .get_func(&mut store, name)
            .with_context(|| format!("no export {name}"))?;
        entries.set(&mut store, i as u64, Ref::Func(Some(f)))?;
    }
    eprintln!("[spike] instantiated in {:?}, booting", t0.elapsed());

    if let Some(t) = o.timeout {
        std::thread::spawn(move || {
            std::thread::sleep(t);
            eprintln!("\n[spike] timeout");
            std::process::exit(124);
        });
    }
    let run = sw.get_typed_func::<(), ()>(&mut store, "run")?;
    let r = run.call(&mut store, ());
    let h = store.data();
    eprintln!("[spike] ran for {:?}, {:?}", t0.elapsed(), h.stats);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in status
        .lines()
        .filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS"))
    {
        eprintln!("[spike] {l}");
    }
    match (h.exit, r) {
        (Some(253), _) => {
            eprintln!(
                "[spike] kernel panic: {}",
                String::from_utf8_lossy(&h.stderr)
            );
            std::process::exit(253);
        }
        (Some(code), _) => {
            eprintln!("[spike] kernel exited with {code}");
            std::process::exit(code);
        }
        (None, Err(e)) => {
            eprintln!("[spike] trap: {e:?}");
            std::process::exit(254);
        }
        (None, Ok(())) => bail!("run returned"),
    }
}

fn define_wasi(l: &mut Linker<Host>) -> Result<()> {
    const W: &str = "wasi_snapshot_preview1";
    l.func_wrap(
        W,
        "args_sizes_get",
        |mut c: Caller<'_, Host>, argc: i32, size: i32| {
            let (m, h) = parts(&mut c);
            wr32(m, argc as usize, h.args.len() as u32);
            wr32(
                m,
                size as usize,
                h.args.iter().map(|a| a.len()).sum::<usize>() as u32,
            );
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "args_get",
        |mut c: Caller<'_, Host>, argv: i32, buf: i32| {
            let (m, h) = parts(&mut c);
            let mut b = buf as usize;
            for (i, a) in h.args.iter().enumerate() {
                wr32(m, argv as usize + 4 * i, b as u32);
                m[b..b + a.len()].copy_from_slice(a);
                b += a.len();
            }
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "clock_time_get",
        |mut c: Caller<'_, Host>, id: i32, _prec: i64, out: i32| {
            let (m, h) = parts(&mut c);
            let ns = match id {
                0 => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64,
                1..=3 => h.mono_ns(),
                _ => return ERRNO_INVAL,
            };
            // the JS host polls its devices here too
            while let Ok(ev) = h.events.try_recv() {
                h.deliver(ev, m);
            }
            wr64(m, out as usize, ns);
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "random_get",
        |mut c: Caller<'_, Host>, buf: i32, len: i32| {
            let (m, _) = parts(&mut c);
            let dst = &mut m[buf as usize..(buf + len) as usize];
            File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(dst))
                .map_or(ERRNO_INVAL, |_| ERRNO_SUCCESS)
        },
    )?;
    l.func_wrap(
        W,
        "fd_write",
        |mut c: Caller<'_, Host>, fd: i32, iovs: i32, n: i32, nw: i32| {
            let (m, h) = parts(&mut c);
            let mut total = 0;
            let mut out = std::io::stdout().lock();
            for i in 0..n as usize {
                let p = rd32(m, iovs as usize + 8 * i) as usize;
                let len = rd32(m, iovs as usize + 8 * i + 4) as usize;
                match fd {
                    1 => {
                        let b = &m[p..p + len];
                        // `echo @@READ""Y@@` from the input script marks a usable shell
                        if h.ready_at.is_none() && b.windows(9).any(|w| w == b"@@READY@@") {
                            h.ready_at = Some(h.t0.elapsed());
                            eprintln!(
                                "[spike] shell ready after {:?} (process start)",
                                h.t0.elapsed()
                            );
                        }
                        let _ = out.write_all(b);
                        // `@@T start NAME` / `@@T end NAME` lines: host-side timing
                        for &c in b {
                            if c != b'\n' {
                                h.line.push(c);
                                continue;
                            }
                            let l = String::from_utf8_lossy(&h.line)
                                .trim_end_matches('\r')
                                .to_owned();
                            h.line.clear();
                            if let Some(name) = l.strip_prefix("@@T start ") {
                                h.marks.insert(name.to_owned(), Instant::now());
                            } else if let Some(name) = l.strip_prefix("@@T end ")
                                && let Some(t) = h.marks.remove(name)
                            {
                                eprintln!(
                                    "[time] {name} {:.1} ms",
                                    t.elapsed().as_secs_f64() * 1e3
                                );
                            }
                        }
                    }
                    2 => h.stderr.extend_from_slice(&m[p..p + len]),
                    _ => return ERRNO_BADF,
                }
                total += len;
            }
            let _ = out.flush();
            if h.stderr.len() > 4096 {
                h.stderr.drain(..h.stderr.len() - 4096);
            }
            wr32(m, nw as usize, total as u32);
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "fd_read",
        |mut c: Caller<'_, Host>, fd: i32, iovs: i32, n: i32, nr: i32| {
            let (m, h) = parts(&mut c);
            let mut total = 0;
            for i in 0..n as usize {
                let p = rd32(m, iovs as usize + 8 * i) as usize;
                let len = rd32(m, iovs as usize + 8 * i + 4) as usize;
                let got = if fd == 0 {
                    let k = len.min(h.input.len());
                    for (j, b) in h.input.drain(..k).enumerate() {
                        m[p + j] = b;
                    }
                    k
                } else if let Some(pos) = h.files.get_mut(&fd) {
                    let k = len.min(h.initrd.len() - *pos);
                    m[p..p + k].copy_from_slice(&h.initrd[*pos..*pos + k]);
                    *pos += k;
                    k
                } else {
                    return ERRNO_BADF;
                };
                total += got;
                if got < len {
                    break;
                }
            }
            if fd == 0 && total == 0 && n > 0 {
                return ERRNO_AGAIN;
            }
            wr32(m, nr as usize, total as u32);
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(W, "fd_close", |mut c: Caller<'_, Host>, fd: i32| {
        if c.data_mut().files.remove(&fd).is_some() {
            ERRNO_SUCCESS
        } else {
            ERRNO_BADF
        }
    })?;
    l.func_wrap(
        W,
        "fd_filestat_get",
        |mut c: Caller<'_, Host>, fd: i32, buf: i32| {
            let (m, h) = parts(&mut c);
            let b = buf as usize;
            m[b..b + 64].fill(0);
            let (ty, size) = match fd {
                0..=2 => (2, 0),
                3 => (3, 0),
                fd if h.files.contains_key(&fd) => (4, h.initrd.len() as u64),
                _ => return ERRNO_BADF,
            };
            m[b + 16] = ty;
            wr64(m, b + 24, 1);
            wr64(m, b + 32, size);
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "fd_prestat_get",
        |mut c: Caller<'_, Host>, fd: i32, buf: i32| {
            if fd != 3 {
                return ERRNO_BADF;
            }
            let (m, _) = parts(&mut c);
            wr32(m, buf as usize, 0);
            wr32(m, buf as usize + 4, 1);
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "path_open",
        |mut c: Caller<'_, Host>,
         dirfd: i32,
         _df: i32,
         path: i32,
         plen: i32,
         oflags: i32,
         _rb: i64,
         _ri: i64,
         _ff: i32,
         opened: i32| {
            if dirfd != 3 {
                return ERRNO_BADF;
            }
            if oflags & 9 != 0 {
                return ERRNO_ROFS;
            }
            let (m, h) = parts(&mut c);
            let name =
                String::from_utf8_lossy(&m[path as usize..(path + plen) as usize]).into_owned();
            if name.trim_start_matches("./").trim_start_matches('/') != "initrd"
                || h.initrd.is_empty()
            {
                return ERRNO_NOENT;
            }
            let fd = (4..).find(|f| !h.files.contains_key(f)).unwrap();
            h.files.insert(fd, 0);
            wr32(m, opened as usize, fd as u32);
            ERRNO_SUCCESS
        },
    )?;
    l.func_wrap(
        W,
        "poll_oneoff",
        |mut c: Caller<'_, Host>, inp: i32, out: i32, nsubs: i32, nev: i32| -> Result<i32> {
            if nsubs == 0 {
                return Ok(ERRNO_INVAL);
            }
            let (m, h) = parts(&mut c);
            if h.exit.is_some() {
                bail!("exited");
            }
            let mut wrote = poll_ready(
                m,
                h,
                inp as usize,
                out as usize,
                nsubs as usize,
                nev as usize,
            );
            if wrote == 0 {
                h.stats.idles += 1;
                let now = h.mono_ns();
                let deadline = (0..nsubs as usize)
                    .filter_map(|i| sub_deadline(m, h, inp as usize + 48 * i))
                    .min()
                    .unwrap_or(now + 60_000_000_000);
                let wait = Duration::from_nanos(deadline.saturating_sub(now));
                match h.events.recv_timeout(wait) {
                    Ok(ev) => {
                        h.deliver(ev, m);
                        while let Ok(ev) = h.events.try_recv() {
                            h.deliver(ev, m);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
                }
                wrote = poll_ready(
                    m,
                    h,
                    inp as usize,
                    out as usize,
                    nsubs as usize,
                    nev as usize,
                );
            }
            let _ = wrote;
            Ok(ERRNO_SUCCESS)
        },
    )?;
    l.func_wrap(W, "sched_yield", |mut c: Caller<'_, Host>| {
        let (m, h) = parts(&mut c);
        while let Ok(ev) = h.events.try_recv() {
            h.deliver(ev, m);
        }
        ERRNO_SUCCESS
    })?;
    l.func_wrap(
        W,
        "proc_exit",
        |mut c: Caller<'_, Host>, code: i32| -> Result<()> {
            c.data_mut().exit = Some(code);
            bail!("proc_exit({code})")
        },
    )?;
    Ok(())
}

/// Absolute monotonic deadline of a clock subscription.
fn sub_deadline(m: &[u8], h: &Host, b: usize) -> Option<u64> {
    if m[b + 8] != 0 {
        return None;
    }
    let timeout = rd64(m, b + 24);
    let abs = u16::from_le_bytes([m[b + 40], m[b + 41]]) & 1 != 0;
    Some(if abs { timeout } else { h.mono_ns() + timeout })
}

/// Writes the subscriptions that are ready now, returns how many.
fn poll_ready(m: &mut [u8], h: &Host, inp: usize, out: usize, n: usize, nev: usize) -> usize {
    let mut k = 0;
    for i in 0..n {
        let b = inp + 48 * i;
        let userdata = rd64(m, b);
        let tag = m[b + 8];
        let (mut err, mut nbytes) = (0u16, 0u64);
        match tag {
            0 => {
                // Relative clocks were relative to the first call; the kernel
                // passes absolute deadlines in practice (as does the JS host's
                // own check, which recomputes them each call).
                if sub_deadline(m, h, b).is_some_and(|d| h.mono_ns() < d) {
                    continue;
                }
            }
            1 => {
                let fd = rd32(m, b + 16);
                if fd == 0 {
                    if h.input.is_empty() {
                        continue;
                    }
                    nbytes = h.input.len() as u64;
                } else {
                    err = ERRNO_BADF as u16;
                }
            }
            _ => err = ERRNO_BADF as u16,
        }
        let e = out + 32 * k;
        m[e..e + 32].fill(0);
        wr64(m, e, userdata);
        m[e + 8..e + 10].copy_from_slice(&err.to_le_bytes());
        m[e + 10] = tag;
        wr64(m, e + 16, nbytes);
        k += 1;
    }
    wr32(m, nev, k as u32);
    k
}

fn define_devices(l: &mut Linker<Host>) -> Result<()> {
    const U: &str = "unkernel";
    l.func_wrap(
        U,
        "wasm_snapshot_point",
        |_: i32, _: i64, _: i64, _: i64| -16i32,
    )?;
    l.func_wrap(
        U,
        "wasm_dump_stacktrace",
        |mut c: Caller<'_, Host>, buf: i64, max: i64| {
            if max > 0 {
                parts(&mut c).0[buf as usize] = 0;
            }
        },
    )?;
    l.func_wrap(U, "wasm_driver_console_size", || 0i32)?;

    // block
    l.func_wrap(
        U,
        "wasm_driver_block_info",
        |mut c: Caller<'_, Host>, dev: i32, info: i64| {
            let (m, h) = parts(&mut c);
            let Some(d) = h.disks.get(dev as usize) else {
                return -19;
            };
            let p = info as usize;
            wr64(m, p, d.size);
            wr32(m, p + 8, d.read_only as u32);
            wr32(m, p + 12, 512);
            0
        },
    )?;
    l.func_wrap(
        U,
        "wasm_driver_block_submit",
        |mut c: Caller<'_, Host>, req: i64| -> i32 {
            let (m, h) = parts(&mut c);
            let p = req as usize;
            let dev = rd32(m, p) as usize;
            let op = rd32(m, p + 4);
            let off = rd64(m, p + 8) * 512;
            let bytes = rd64(m, p + 16);
            let nseg = rd32(m, p + 24) as usize;
            let Some(d) = h.disks.get(dev) else {
                return -19;
            };
            if op != 2 && off + bytes > d.size {
                return -5;
            }
            let mut at = off;
            for i in 0..nseg {
                let a = rd64(m, p + 32 + 16 * i) as usize;
                let len = rd64(m, p + 32 + 16 * i + 8) as usize;
                let r = match op {
                    0 => d.file.read_exact_at(&mut m[a..a + len], at),
                    1 if d.read_only => return -30,
                    1 => d.file.write_all_at(&m[a..a + len], at),
                    _ => Ok(()),
                };
                if r.is_err() {
                    return -5;
                }
                at += len as u64;
            }
            match op {
                0 => {
                    h.stats.block_reads += 1;
                    h.stats.block_bytes += bytes;
                }
                2 => {
                    let _ = d.file.sync_data();
                }
                3 | 4 if op == 3 => {} // discard: ignore
                4 => {
                    // write zeroes
                    let z = vec![0u8; bytes as usize];
                    if d.read_only || d.file.write_all_at(&z, off).is_err() {
                        return -5;
                    }
                }
                _ if op > 4 => return -95,
                _ => {}
            }
            let _ = IRQ_BLOCK;
            0 // done synchronously
        },
    )?;
    l.func_wrap(U, "wasm_driver_block_reap", || 0i64)?;

    // network
    l.func_wrap(
        U,
        "wasm_driver_net_init",
        |mut c: Caller<'_, Host>, mac: i64| -> i64 {
            let (m, h) = parts(&mut c);
            let Some(n) = &h.net else { return -1 };
            m[mac as usize..mac as usize + 6].copy_from_slice(&n.guest_mac());
            0
        },
    )?;
    l.func_wrap(
        U,
        "wasm_driver_net_send",
        |mut c: Caller<'_, Host>, buf: i64, len: i64| -> i64 {
            let (m, h) = parts(&mut c);
            if let Some(n) = &mut h.net {
                n.send(&m[buf as usize..(buf + len) as usize]);
            }
            // frames the stack answers straight away (ARP, DNS, SYN-ACK) land on
            // the event channel; pick them up now rather than at the next idle
            while let Ok(ev) = h.events.try_recv() {
                h.deliver(ev, m);
            }
            len
        },
    )?;
    l.func_wrap(U, "wasm_driver_net_rx_len", |c: Caller<'_, Host>| {
        c.data().net_rx.front().map_or(0, |f| f.len() as i64)
    })?;
    l.func_wrap(
        U,
        "wasm_driver_net_recv",
        |mut c: Caller<'_, Host>, buf: i64, max: i64| -> i64 {
            let (m, h) = parts(&mut c);
            let Some(f) = h.net_rx.pop_front() else {
                return 0;
            };
            if buf != 0 {
                let k = f.len().min(max as usize);
                m[buf as usize..buf as usize + k].copy_from_slice(&f[..k]);
            }
            f.len() as i64
        },
    )?;

    // devices this spike doesn't have
    l.func_wrap(U, "wasm_driver_fb_init", || ENODEV)?;
    l.func_wrap(U, "wasm_driver_fb_mode", || 0i32)?;
    l.func_wrap(
        U,
        "wasm_driver_fb_attach",
        |_: i64, _: i32, _: i32, _: i32, _: i32| -19i32,
    )?;
    l.func_wrap(U, "wasm_driver_virtio_init", |_: i32| 0i32)?;
    l.func_wrap(U, "wasm_driver_virtio_features", |_: i32| 0i64)?;
    l.func_wrap(U, "wasm_driver_virtio_set_features", |_: i32, _: i64| {
        -19i32
    })?;
    l.func_wrap(
        U,
        "wasm_driver_virtio_config",
        |_: i32, _: i32, _: i64, _: i32, _: i32| {},
    )?;
    l.func_wrap(U, "wasm_driver_virtio_status", |_: i32, _: i32| {})?;
    l.func_wrap(U, "wasm_driver_virtio_queue_max", |_: i32, _: i32| 0i32)?;
    l.func_wrap(
        U,
        "wasm_driver_virtio_queue_set",
        |_: i32, _: i32, _: i32, _: i64, _: i64, _: i64| -19i32,
    )?;
    l.func_wrap(U, "wasm_driver_virtio_interrupt", |_: i32| 0i32)?;
    l.func_wrap(U, "wasm_driver_virtio_notify", |_: i32, _: i32| {})?;
    l.func_wrap(U, "wasm_driver_shm_init", |_: i32| -19i32)?;
    l.func_wrap(U, "wasm_driver_shm_request", |_: i64, _: i32| -19i32)?;
    l.func_wrap(U, "wasm_driver_shm_reply", |_: i32, _: i32| {})?;
    l.func_wrap(U, "wasm_driver_shm_detach", |_: i32| {})?;
    l.func_wrap(
        U,
        "wasm_driver_shm_attach",
        |_: i32, _: i32, _: i32, _: i64, _: i64, _: i32| {},
    )?;
    l.func_wrap(U, "wasm_driver_input_init", || ENODEV)?;
    l.func_wrap(U, "wasm_driver_input_read", |_: i64, _: i64| 0i64)?;
    l.func_wrap(U, "wasm_driver_input_state", |_: i32| {})?;
    l.func_wrap(U, "wasm_driver_sound_init", || ENODEV)?;
    l.func_wrap(
        U,
        "wasm_driver_sound_event",
        |_: i32, _: i32, _: i32, _: i32| {},
    )?;
    l.func_wrap(U, "wasm_driver_sound_write", |_: i64, _: i64| 0i64)?;
    l.func_wrap(U, "wasm_driver_vsock_init", |_: i32, _: i32| -19i32)?;
    l.func_wrap(U, "wasm_driver_vsock_send", |_: i64, _: i64, _: i32| -19i32)?;
    l.func_wrap(U, "wasm_driver_vsock_rx_len", || 0i32)?;
    l.func_wrap(U, "wasm_driver_vsock_recv", |_: i64, _: i32| 0i32)?;
    l.func_wrap(U, "wasm_driver_vsock_keep", |_: i32, _: i32| 0i32)?;
    // kwemu's JIT hands us a wasm module per hot block; it shares the
    // kernel's memory and function table, and replaces table[slot].
    l.func_wrap(U, "kw_host_jit_caps", |c: Caller<'_, Host>| {
        let h = c.data();
        if !h.jit || h.table.is_none() {
            0
        } else {
            1 | 2 | 16 | 32
        } // available, mem32, table, simd
    })?;
    l.func_wrap(
        U,
        "kw_host_jit_free",
        |mut c: Caller<'_, Host>, slot: i32| {
            let Some(orig) = c.data_mut().jit_saved.remove(&slot) else {
                return;
            };
            let t = c.data().table.unwrap();
            let _ = t.set(&mut c, slot as u64, Ref::Func(orig));
        },
    )?;
    l.func_wrap(
        U,
        "kw_host_jit_compile",
        |mut c: Caller<'_, Host>, ptr: i64, len: i64, slot: i32, _flags: i32| -> i32 {
            let t0 = Instant::now();
            let r = jit_compile(&mut c, ptr as usize, len as usize, slot);
            let st = &mut c.data_mut().stats;
            st.jit_compile_ms += t0.elapsed().as_secs_f64() * 1e3;
            match r {
                Ok(()) => {
                    st.jit_compiled += 1;
                    0
                }
                Err(e) => {
                    st.jit_failed += 1;
                    if st.jit_failed <= 3 {
                        eprintln!("[spike] jit: slot {slot}: {e:#}");
                    }
                    -1
                }
            }
        },
    )?;
    Ok(())
}

// ponytail: every compiled block is a new instance that lives as long as the
// store (wasmtime frees instances only with it); fine for a session, a real
// host would pool blocks per module or recycle the store.
fn jit_compile(c: &mut Caller<'_, Host>, ptr: usize, len: usize, slot: i32) -> Result<()> {
    let (memory, table) = (
        c.data().memory.unwrap(),
        c.data().table.context("no table")?,
    );
    if len == 0 || len > 1 << 20 || ptr + len > memory.data_size(&*c) {
        bail!("bad module range {ptr}+{len}");
    }
    let bytes = memory.data(&*c)[ptr..ptr + len].to_vec();
    // the same block recurs across processes (same binary, same addresses)
    let cached = c.data().jit_cache.get(&bytes).cloned();
    let module = match cached {
        Some(m) => {
            c.data_mut().stats.jit_cache_hits += 1;
            m
        }
        None => {
            let m = Module::new(&c.engine().clone(), &bytes)?;
            c.data_mut().jit_cache.insert(bytes, m.clone());
            m
        }
    };
    let imports: Vec<Extern> = module
        .imports()
        .map(|i| match i.name() {
            "memory" => Ok(Extern::Memory(memory)),
            "table" => Ok(Extern::Table(table)),
            n => Err(format_err!("unexpected import {n}")),
        })
        .collect::<Result<_>>()?;
    let inst = Instance::new(&mut *c, &module, &imports)?;
    let f = inst
        .get_func(&mut *c, "f")
        .context("the module exports no function f")?;
    let prev = table.get(&mut *c, slot as u64).context("bad slot")?;
    table.set(&mut *c, slot as u64, Ref::Func(Some(f)))?;
    let prev = prev.as_func().flatten().copied();
    c.data_mut().jit_saved.entry(slot).or_insert(prev);
    Ok(())
}

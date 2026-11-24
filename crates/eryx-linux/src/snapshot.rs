//! Post-boot snapshots: the kernel parks at a consistent point and hands us
//! a bitmap of its free pages; we keep the 64 KiB chunks that hold anything,
//! and a new instance resumes from them through `kw_resume`.
//!
//! ponytail: restore copies the chunks into the new memory (memcpy). A
//! copy-on-write mapping of the image (as eryx's preinit gets from wasmtime)
//! is the upgrade once the copy is measured to matter.

use std::io::{Read, Write};

use wasmtime::{Result, bail};

pub const CHUNK: usize = 64 * 1024;
const PAGE: usize = 4096;
const MAGIC: &[u8; 8] = b"UKSNAP1\n";

pub struct Snapshot {
    pub mem_len: usize,
    pub mono_ns: u64,
    pub guest_cid: Option<u64>,
    /// (chunk index, CHUNK bytes) for every chunk that is in use and not zero.
    pub chunks: Vec<(u32, Vec<u8>)>,
}

pub fn capture(
    mem: &[u8],
    free: &[u8],
    free_bits: usize,
    mono_ns: u64,
    guest_cid: Option<u64>,
) -> Snapshot {
    let is_free = |page: usize| page < free_bits && free[page / 8] & (1 << (page % 8)) != 0;
    let mut chunks = Vec::new();
    for (i, c) in mem.chunks(CHUNK).enumerate() {
        let mut out = c.to_vec();
        for p in 0..CHUNK / PAGE {
            if is_free(i * CHUNK / PAGE + p) {
                out[p * PAGE..(p + 1) * PAGE].fill(0);
            }
        }
        if out.iter().any(|&b| b != 0) {
            chunks.push((i as u32, out));
        }
    }
    Snapshot {
        mem_len: mem.len(),
        mono_ns,
        guest_cid,
        chunks,
    }
}

/// Fills `mem` (a fresh instance's: zero apart from the kernel's data
/// segments below `end`) with the snapshot.
pub fn restore(s: &Snapshot, mem: &mut [u8], end: usize) -> Result<()> {
    if mem.len() != s.mem_len {
        bail!("snapshot of {} bytes, memory of {}", s.mem_len, mem.len());
    }
    let mut next = 0usize;
    for &(i, ref data) in &s.chunks {
        let i = i as usize;
        // chunks the snapshot leaves out are zero; only the data segments wrote any
        for z in next..i.min(end.div_ceil(CHUNK)) {
            mem[z * CHUNK..(z + 1) * CHUNK].fill(0);
        }
        mem[i * CHUNK..(i + 1) * CHUNK].copy_from_slice(data);
        next = i + 1;
    }
    for z in next..end.div_ceil(CHUNK) {
        mem[z * CHUNK..(z + 1) * CHUNK].fill(0);
    }
    Ok(())
}

pub fn save(s: &Snapshot, path: &str) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(MAGIC)?;
    for v in [
        s.mem_len as u64,
        s.mono_ns,
        s.guest_cid.map_or(0, |c| c + 1),
        s.chunks.len() as u64,
    ] {
        f.write_all(&v.to_le_bytes())?;
    }
    for (i, data) in &s.chunks {
        f.write_all(&i.to_le_bytes())?;
        f.write_all(data)?;
    }
    f.flush()?;
    Ok(())
}

pub fn load(path: &str) -> Result<Snapshot> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("{path} is not a snapshot");
    }
    let mut u64s = [0u64; 4];
    for v in &mut u64s {
        let mut b = [0u8; 8];
        f.read_exact(&mut b)?;
        *v = u64::from_le_bytes(b);
    }
    let [mem_len, mono_ns, cid, n] = u64s;
    let mut chunks = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let mut i = [0u8; 4];
        f.read_exact(&mut i)?;
        let mut data = vec![0u8; CHUNK];
        f.read_exact(&mut data)?;
        chunks.push((u32::from_le_bytes(i), data));
    }
    Ok(Snapshot {
        mem_len: mem_len as usize,
        mono_ns,
        guest_cid: cid.checked_sub(1),
        chunks,
    })
}

/// A module whose exported memory starts out as the snapshot, so wasmtime
/// can map it copy-on-write instead of copying it into each sandbox.
pub fn image_module(s: &Snapshot) -> Vec<u8> {
    use wasm_encoder::{
        ConstExpr, DataSection, ExportKind, ExportSection, MemorySection, MemoryType,
    };
    let pages = (s.mem_len / 65536) as u64;
    let mut mems = MemorySection::new();
    mems.memory(MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    let mut data = DataSection::new();
    // one segment per run of consecutive chunks
    let mut i = 0;
    while i < s.chunks.len() {
        let start = s.chunks[i].0;
        let mut j = i;
        while j + 1 < s.chunks.len() && s.chunks[j + 1].0 == s.chunks[j].0 + 1 {
            j += 1;
        }
        let bytes: Vec<u8> = s.chunks[i..=j]
            .iter()
            .flat_map(|(_, d)| d.iter().copied())
            .collect();
        data.active(
            0,
            &ConstExpr::i32_const((start as usize * CHUNK) as u32 as i32),
            bytes,
        );
        i = j + 1;
    }
    let mut m = wasm_encoder::Module::new();
    m.section(&mems).section(&exports).section(&data);
    m.finish()
}

/// The kernel as a resumed sandbox needs it: instantiating it must not touch
/// memory, which already holds the snapshot. Drops the start function
/// (`__wasm_init_memory` zeroes the BSS) and makes the data segments passive
/// (nothing refers to them once the start function is gone).
pub fn resume_variant(kernel: &[u8]) -> wasmtime::Result<Vec<u8>> {
    let mut out = kernel[..8].to_vec();
    let mut p = 8;
    while p < kernel.len() {
        let id = kernel[p];
        let (size, n) = leb(&kernel[p + 1..]);
        let body = &kernel[p + 1 + n..p + 1 + n + size];
        match id {
            8 => {} // start
            11 => {
                let mut data = wasm_encoder::DataSection::new();
                for d in wasmparser::DataSectionReader::new(wasmparser::BinaryReader::new(body, 0))?
                {
                    data.passive(d?.data.to_vec());
                }
                out.push(11);
                wasm_encoder::Encode::encode(&data, &mut out); // size-prefixed
            }
            _ => out.extend_from_slice(&kernel[p..p + 1 + n + size]),
        }
        p += 1 + n + size;
    }
    Ok(out)
}

fn leb(b: &[u8]) -> (usize, usize) {
    let (mut v, mut shift, mut i) = (0usize, 0, 0);
    loop {
        v |= ((b[i] & 0x7f) as usize) << shift;
        shift += 7;
        i += 1;
        if b[i - 1] & 0x80 == 0 {
            return (v, i);
        }
    }
}

/// Writes `bytes` leaving 4 KiB blocks of zeros as holes: the dense memory
/// image is mostly zeros between the low and high ends of guest memory.
pub fn write_sparse(path: &str, bytes: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::create(path)?;
    for (i, b) in bytes.chunks(PAGE).enumerate() {
        if b.iter().all(|&x| x == 0) {
            continue;
        }
        f.seek(SeekFrom::Start((i * PAGE) as u64))?;
        f.write_all(b)?;
    }
    f.set_len(bytes.len() as u64)?;
    Ok(())
}

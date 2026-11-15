//! Text sequence (a, b, c, ... aa, ab, ...) and the four on-disk databases.
//!
//! Generated rows only need their digest: the text is a pure function of the
//! sequence position, so nothing else has to be stored.
//!
//!   data/sha256/0000.bkt … 0511.bkt   digests, split by prefix into 512 shards
//!   data/sha256/state.bin             per-shard position bookkeeping
//!   data/sha256.user.db               text typed into the encode tab
//!
//! Every record is `[7-byte key][varint position delta]`: the key is the top 55
//! bits of the digest prefix (the omitted top 9 bits select the shard file), and
//! the delta is the gap to the previous index in that shard. Because text and
//! index are derived, a record costs ~9 bytes instead of ~80.
//!
//! Decoding a hash is fast because only one shard can contain it: about
//! 1/512th of the db is read (roughly 16 MB even at a billion entries) instead
//! of the whole file. Every candidate whose key matches is re-hashed in full
//! and compared, so the short key can never return a wrong text.
//!
//! Shards are opened on first write and their bookkeeping lives in one small
//! state file, so starting up reads a few kilobytes instead of every file.
//! Writes are buffered per shard and flushed in 32 KB chunks, which keeps
//! generation close to hash speed while the files stay small.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::sha;

pub const NAMES: [&str; 4] = ["sha1", "sha256", "sha384", "sha512"];
pub const HLEN: [usize; 4] = [20, 32, 48, 64];

const PREFIX: usize = 8; // digest bytes taken before splitting into shards
const BUCKETS: usize = 512;
const KEY_BITS: u32 = 9; // 512 shards, so a fragment is 55 bits
const KEY_BYTES: usize = 7;
const FILE_HDR: usize = 7; // magic + alg id + shard id
const STATE_REC: usize = 24; // last index, record count, byte length
const FLUSH_BYTES: usize = 1 << 15;

const MAGIC: &[u8; 4] = b"SHD3";

// ------------------------------------------------------------- sequence ----

pub const N: u64 = 95;

const fn is_alnum(b: u8) -> bool {
    (b >= b'a' && b <= b'z') || (b >= b'A' && b <= b'Z') || (b >= b'0' && b <= b'9')
}

/// Order matters: generation starts at "a", so letters come first, then
/// digits, then every punctuation symbol on a standard keyboard, then space.
const fn build_charset() -> [u8; 95] {
    let mut c = [0u8; 95];
    let mut i = 0;
    let mut b = b'a';
    while b <= b'z' {
        c[i] = b;
        i += 1;
        b += 1;
    }
    b = b'A';
    while b <= b'Z' {
        c[i] = b;
        i += 1;
        b += 1;
    }
    b = b'0';
    while b <= b'9' {
        c[i] = b;
        i += 1;
        b += 1;
    }
    b = 0x21;
    while b <= 0x7e {
        if !is_alnum(b) {
            c[i] = b;
            i += 1;
        }
        b += 1;
    }
    c[i] = b' ';
    debug_assert!(i + 1 == 95);
    c
}

pub const CHARSET: [u8; 95] = build_charset();

/// Write the text at sequence position `idx` into `out`, returning its length.
/// Fixed-length blocks are plain base-95: "a".."z", then "aa", "ab", ...
pub fn text_at(idx: u64, out: &mut [u8; 16]) -> usize {
    let mut rem = idx;
    let mut len = 1usize;
    let mut count = N;
    while rem >= count {
        rem -= count;
        len += 1;
        count = count.saturating_mul(N);
        if len >= out.len() {
            break;
        }
    }

    let mut tmp = [0u8; 16];
    let mut r = rem;
    for pos in (0..len).rev() {
        tmp[pos] = CHARSET[(r % N) as usize];
        r /= N;
    }
    out[..len].copy_from_slice(&tmp[..len]);
    len
}

pub fn text_of(idx: u64) -> Vec<u8> {
    let mut b = [0u8; 16];
    let n = text_at(idx, &mut b);
    b[..n].to_vec()
}

pub fn digest(alg: usize, text: &[u8]) -> Vec<u8> {
    match alg {
        0 => sha::sha1(text).to_vec(),
        1 => sha::sha256(text).to_vec(),
        2 => sha::sha384(text).to_vec(),
        _ => sha::sha512(text).to_vec(),
    }
}

/// Hash `n` consecutive sequence positions into one write-ready buffer per
/// algorithm (8-byte prefixes). Called from several threads at once.
pub fn build_chunk(start: u64, n: u64, enabled: [bool; 4]) -> [Vec<u8>; 4] {
    let cap = (n as usize).saturating_mul(PREFIX);
    let mut out: [Vec<u8>; 4] = std::array::from_fn(|_| Vec::with_capacity(cap));
    let mut tbuf = [0u8; 16];

    for k in 0..n {
        let idx = start + k;
        let tlen = text_at(idx, &mut tbuf);
        let text = &tbuf[..tlen];

        // Only hash what is switched on: disabled algorithms cost nothing.
        if enabled[0] {
            let d = sha::sha1(text);
            out[0].extend_from_slice(&d[..PREFIX]);
        }
        if enabled[1] {
            let d = sha::sha256(text);
            out[1].extend_from_slice(&d[..PREFIX]);
        }
        if enabled[2] {
            let d = sha::sha384(text);
            out[2].extend_from_slice(&d[..PREFIX]);
        }
        if enabled[3] {
            let d = sha::sha512(text);
            out[3].extend_from_slice(&d[..PREFIX]);
        }
    }
    out
}

// -------------------------------------------------------------- sharding ----

fn bucket_of(prefix: u64) -> usize {
    (prefix >> (64 - KEY_BITS)) as usize
}

fn key_of(prefix: u64) -> u64 {
    prefix >> KEY_BITS
}

fn key_bytes(key: u64) -> [u8; KEY_BYTES] {
    let b = key.to_le_bytes();
    let mut out = [0u8; KEY_BYTES];
    out.copy_from_slice(&b[..KEY_BYTES]);
    out
}

fn key_from(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a[..KEY_BYTES].copy_from_slice(&b[..KEY_BYTES]);
    u64::from_le_bytes(a)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn get_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0;
    for (i, b) in buf.iter().enumerate() {
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn read_varint<R: Read>(r: &mut R) -> io::Result<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0;
    let mut used = 0;
    loop {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        used += 1;
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] & 0x80 == 0 {
            return Ok((v, used));
        }
        shift += 7;
        if shift > 63 {
            return Err(io::Error::other("bad record"));
        }
    }
}

// ---------------------------------------------------------------- files ----

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn header_bytes(alg: usize, bucket: usize) -> [u8; FILE_HDR] {
    let mut h = [0u8; FILE_HDR];
    h[..4].copy_from_slice(MAGIC);
    h[4] = alg as u8;
    h[5..7].copy_from_slice(&(bucket as u16).to_le_bytes());
    h
}

fn has_header(path: &Path, alg: usize, bucket: usize) -> bool {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut h = [0u8; FILE_HDR];
    if f.read_exact(&mut h).is_err() {
        return false;
    }
    &h[..4] == MAGIC && h[4] == alg as u8 && u16::from_le_bytes([h[5], h[6]]) as usize == bucket
}

fn write_header(path: &Path, alg: usize, bucket: usize) -> io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(&header_bytes(alg, bucket))
}

/// Continue a shard from the byte offset the state file recorded, dropping any
/// half-written record left by a crash.
fn scan_tail(path: &Path, from: u64, mut last: u64, mut count: u64) -> io::Result<(u64, u64, u64)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut r = BufReader::with_capacity(1 << 16, f);
    let mut pos = from;
    let mut key = [0u8; KEY_BYTES];
    loop {
        if r.read_exact(&mut key).is_err() {
            break;
        }
        pos += KEY_BYTES as u64;
        let (d, n) = match read_varint(&mut r) {
            Ok(v) => v,
            Err(_) => break,
        };
        pos += n as u64;
        last += d;
        count += 1;
    }
    let len = file_len(path);
    if pos != len {
        OpenOptions::new().write(true).open(path)?.set_len(pos)?;
    }
    Ok((last, count, pos))
}

fn read_state(path: &Path) -> Vec<(u64, u64, u64)> {
    let want = BUCKETS * STATE_REC;
    let v = std::fs::read(path).unwrap_or_default();
    if v.len() != want {
        return vec![(0, 0, 0); BUCKETS];
    }
    (0..BUCKETS)
        .map(|i| {
            let o = i * STATE_REC;
            let g = |k: usize| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&v[o + k * 8..o + k * 8 + 8]);
                u64::from_le_bytes(b)
            };
            (g(0), g(1), g(2))
        })
        .collect()
}

/// Rewrite the whole state file in place. Cheap enough to run after every
/// flush, and keeping the handle open avoids a create/truncate per batch.
fn write_state_to(f: &mut File, st: &[(u64, u64, u64)]) -> io::Result<()> {
    let mut v = Vec::with_capacity(BUCKETS * STATE_REC);
    for (a, b, c) in st {
        v.extend_from_slice(&a.to_le_bytes());
        v.extend_from_slice(&b.to_le_bytes());
        v.extend_from_slice(&c.to_le_bytes());
    }
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&v)?;
    f.set_len(v.len() as u64)
}

// ----------------------------------------------------------------- store ----

/// One pair of entries sharing a 55-bit shard key.
pub struct Crossover {
    pub shard: usize,
    pub a_index: u64,
    pub b_index: u64,
    pub key: u64,
}

/// A digest found in a database. `index` is `u64::MAX` for text saved through
/// the encode tab, which does not live in the sequence.
pub struct Found {
    pub index: u64,
    pub text: Vec<u8>,
}

struct Alg {
    dir: PathBuf,
    paths: Vec<PathBuf>,
    state_path: PathBuf,
    state_file: Option<File>,
    user_path: PathBuf,
    files: Vec<Option<File>>,
    user_file: Option<File>,
    bufs: Vec<Vec<u8>>,
    buf_counts: Vec<u64>,
    last: Vec<u64>,
    state: Vec<(u64, u64, u64)>,
    enabled: bool,
    /// Generated records stored (flushed or still buffered).
    counts: u64,
    user_counts: u64,
    bytes: u64,
}

pub struct Store {
    dir: PathBuf,
    algs: Vec<Alg>,
    next: u64,
    last_index: Option<u64>,
    last_text: Vec<u8>,
}

/// Open one shard for appending, first checking it against the state file and
/// repairing it if a crash interrupted the last write.
fn open_shard(alg: &mut Alg, alg_id: usize, b: usize) -> io::Result<()> {
    if alg.files[b].is_some() {
        return Ok(());
    }
    let p = &alg.paths[b];
    let len = file_len(p);
    let st = alg.state[b];

    if len < FILE_HDR as u64 || !has_header(p, alg_id, b) || len < st.2 {
        write_header(p, alg_id, b)?;
        alg.bytes = alg.bytes.saturating_sub(st.2);
        alg.state[b] = (0, 0, 0);
    } else if len > st.2 {
        let (l, c, n) = scan_tail(p, st.2, st.0, st.1)?;
        alg.state[b] = (l, c, n);
        alg.bytes += n.saturating_sub(st.2);
    }
    alg.last[b] = alg.state[b].0;
    alg.files[b] = Some(OpenOptions::new().append(true).open(p)?);
    Ok(())
}

fn count_user_records(path: &Path, alg: usize) -> io::Result<u64> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(0),
    };
    let mut r = BufReader::with_capacity(1 << 16, f);
    let hlen = HLEN[alg];
    let mut lenbuf = [0u8; 2];
    let mut text = Vec::new();
    let mut hash = [0u8; 64];
    let mut n = 0;
    loop {
        if r.read_exact(&mut lenbuf).is_err() {
            return Ok(n);
        }
        let tlen = u16::from_le_bytes(lenbuf) as usize;
        text.resize(tlen, 0);
        if r.read_exact(&mut text).is_err() || r.read_exact(&mut hash[..hlen]).is_err() {
            return Ok(n);
        }
        n += 1;
    }
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("algs.conf")
}

/// Which algorithms are switched on, persisted so it survives a restart.
fn read_config(dir: &Path) -> Option<[bool; 4]> {
    let text = std::fs::read_to_string(config_path(dir)).ok()?;
    let mut out = [true; 4];
    let mut seen = 0;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if let Some(i) = NAMES.iter().position(|n| *n == k.trim()) {
                out[i] = v.trim() != "0";
                seen += 1;
            }
        }
    }
    if seen == 4 {
        Some(out)
    } else {
        None
    }
}

/// Append one record to a shard buffer, opening the shard on demand.
fn push_record(
    alg: &mut Alg,
    alg_id: usize,
    bucket: usize,
    key: u64,
    idx: u64,
) -> io::Result<()> {
    if alg.files[bucket].is_none() {
        open_shard(alg, alg_id, bucket)?;
    }
    let delta = idx - alg.last[bucket];
    let buf = &mut alg.bufs[bucket];
    buf.extend_from_slice(&key_bytes(key));
    put_varint(buf, delta);
    alg.last[bucket] = idx;
    alg.buf_counts[bucket] += 1;
    alg.counts += 1;
    alg.bytes += (KEY_BYTES + varint_len(delta)) as u64;
    Ok(())
}

/// Open (or create) one algorithm's shards. Only the small state file is read,
/// so a db with a billion entries still starts instantly.
fn make_alg(dir: &Path, alg: usize, want: bool) -> io::Result<Alg> {
    let adir = dir.join(NAMES[alg]);
    std::fs::create_dir_all(&adir)?;
    let state_path = adir.join("state.bin");
    let user_path = dir.join(format!("{}.user.db", NAMES[alg]));
    if !user_path.exists() {
        File::create(&user_path)?;
    }
    let paths: Vec<PathBuf> = (0..BUCKETS)
        .map(|b| adir.join(format!("{b:04}.bkt")))
        .collect();

    let mut state = read_state(&state_path);
    if file_len(&state_path) != (BUCKETS * STATE_REC) as u64 {
        // No bookkeeping yet: rebuild it from whatever shards are present.
        for b in 0..BUCKETS {
            let p = &paths[b];
            let len = file_len(p);
            state[b] = if len > FILE_HDR as u64 && has_header(p, alg, b) {
                scan_tail(p, 0, 0, 0)?
            } else {
                (0, 0, 0)
            };
        }
    }
    let mut state_file = Some(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&state_path)?,
    );
    if let Some(f) = state_file.as_mut() {
        write_state_to(f, &state)?;
    }

    let bytes = state.iter().map(|s| s.2).sum::<u64>()
        + (BUCKETS * STATE_REC) as u64
        + file_len(&user_path);
    let counts: u64 = state.iter().map(|s| s.1).sum();
    let last: Vec<u64> = state.iter().map(|s| s.0).collect();
    let user_counts = count_user_records(&user_path, alg).unwrap_or(0);
    let user_file = if want {
        Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&user_path)?,
        )
    } else {
        None
    };

    Ok(Alg {
        dir: adir,
        paths,
        state_path,
        state_file,
        user_path,
        files: (0..BUCKETS).map(|_| None).collect(),
        user_file,
        bufs: vec![Vec::new(); BUCKETS],
        buf_counts: vec![0; BUCKETS],
        last,
        state,
        enabled: want,
        counts,
        user_counts,
        bytes,
    })
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let cfg = read_config(dir);
        let mut algs = Vec::with_capacity(4);
        for alg in 0..4 {
            let want = cfg.map(|c| c[alg]).unwrap_or(true);
            algs.push(make_alg(dir, alg, want)?);
        }

        let mut next = 0u64;
        let mut last_index = None;
        for alg in &algs {
            for s in &alg.state {
                if s.1 > 0 && last_index.map_or(true, |l: u64| s.0 > l) {
                    last_index = Some(s.0);
                }
            }
        }
        if let Some(last) = last_index {
            next = last + 1;
        }
        let last_text = last_index.map(text_of).unwrap_or_default();

        Ok(Store {
            dir: dir.to_path_buf(),
            algs,
            next,
            last_index,
            last_text,
        })
    }

    pub fn next_index(&self) -> u64 {
        self.next
    }

    /// Generated entries plus text saved from the encode tab.
    pub fn counts(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.algs[i].counts + self.algs[i].user_counts)
    }

    pub fn enabled(&self) -> [bool; 4] {
        std::array::from_fn(|i| self.algs[i].enabled)
    }

    /// On-disk size of each algorithm's files, in bytes.
    pub fn sizes(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.algs[i].bytes)
    }

    pub fn last_text(&self) -> &[u8] {
        &self.last_text
    }

    pub fn next_text(&self) -> Vec<u8> {
        text_of(self.next)
    }

    fn save_config(&self) -> io::Result<()> {
        let mut s = String::new();
        for i in 0..4 {
            let on = if self.algs[i].enabled { 1 } else { 0 };
            s.push_str(&format!("{}={}\n", NAMES[i], on));
        }
        std::fs::write(config_path(&self.dir), s)
    }

    /// Write out shard buffers. `all` flushes everything; otherwise only shards
    /// whose buffer is full, so generation keeps moving in large writes.
    fn flush_alg(&mut self, a: usize, all: bool) -> io::Result<()> {
        let alg = &mut self.algs[a];
        let mut any = false;
        for b in 0..BUCKETS {
            if alg.bufs[b].is_empty() {
                continue;
            }
            if !all && alg.bufs[b].len() < FLUSH_BYTES {
                continue;
            }
            match alg.files[b].as_mut() {
                Some(f) => f.write_all(&alg.bufs[b])?,
                None => continue, // switched off: keep the buffer for later
            }
            alg.state[b].0 = alg.last[b];
            alg.state[b].1 += alg.buf_counts[b];
            alg.state[b].2 += alg.bufs[b].len() as u64;
            alg.bufs[b].clear();
            alg.buf_counts[b] = 0;
            any = true;
        }
        if any {
            if let Some(f) = alg.state_file.as_mut() {
                write_state_to(f, &alg.state)?;
            }
        }
        Ok(())
    }

    /// Flush every buffer; called when generation stops so nothing is lost.
    pub fn flush(&mut self) -> io::Result<()> {
        for a in 0..4 {
            self.flush_alg(a, true)?;
        }
        Ok(())
    }

    /// Turn one algorithm on or off. Turning it on picks up whatever its shards
    /// already hold; turning it off stops writes.
    pub fn set_enabled(&mut self, alg: usize, on: bool) -> io::Result<()> {
        if on {
            self.flush_alg(alg, true)?;
            self.algs[alg] = make_alg(&self.dir, alg, true)?;
            let last = self.algs[alg].state.iter().map(|s| s.0).max().unwrap_or(0);
            if self.algs[alg].counts > 0 && last + 1 > self.next {
                self.next = last + 1;
                self.last_text = text_of(last);
            }
        } else {
            self.flush_alg(alg, true)?;
            let a = &mut self.algs[alg];
            for f in &mut a.files {
                *f = None;
            }
            a.user_file = None;
            a.enabled = false;
        }
        self.save_config()
    }

    /// Delete one algorithm's files and switch it off.
    pub fn remove(&mut self, alg: usize) -> io::Result<()> {
        {
            let a = &mut self.algs[alg];
            a.enabled = false;
            for f in &mut a.files {
                *f = None;
            }
            a.user_file = None;
            a.state_file = None;
            match std::fs::remove_dir_all(&a.dir) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let _ = std::fs::remove_file(&a.user_path);
            a.counts = 0;
            a.user_counts = 0;
            a.bytes = 0;
            for b in 0..BUCKETS {
                a.state[b] = (0, 0, 0);
                a.last[b] = 0;
                a.bufs[b].clear();
                a.buf_counts[b] = 0;
            }
        }
        self.save_config()
    }

    /// Append a freshly generated batch. `parts` holds one buffer per worker,
    /// already in sequence order.
    pub fn write_gen(&mut self, parts: &[[Vec<u8>; 4]], base: u64, total: u64) -> io::Result<()> {
        let mut part_base = base;
        for part in parts {
            let count = part.iter().map(|p| p.len()).max().unwrap_or(0) / PREFIX;
            for k in 0..count {
                let idx = part_base + k as u64;
                for a in 0..4 {
                    if !self.algs[a].enabled || part[a].len() < (k + 1) * PREFIX {
                        continue;
                    }
                    let raw = &part[a][k * PREFIX..(k + 1) * PREFIX];
                    let p = u64::from_be_bytes(raw.try_into().unwrap());
                    let b = bucket_of(p);
                    push_record(&mut self.algs[a], a, b, key_of(p), idx)?;
                }
            }
            part_base += count as u64;
        }

        for a in 0..4 {
            self.flush_alg(a, false)?;
        }

        if total > 0 {
            let last = part_base - 1;
            self.last_index = Some(last);
            self.last_text = text_of(last);
            self.next = last + 1;
        }
        Ok(())
    }

    /// Throw away every db and reset the sequence.
    pub fn clear(&mut self) -> io::Result<()> {
        for a in 0..4 {
            {
                let alg = &mut self.algs[a];
                for f in &mut alg.files {
                    *f = None;
                }
                alg.user_file = None;
                alg.state_file = None;
                match std::fs::remove_dir_all(&alg.dir) {
                    Ok(()) => {}
                    Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                std::fs::create_dir_all(&alg.dir)?;
                for b in 0..BUCKETS {
                    alg.state[b] = (0, 0, 0);
                    alg.last[b] = 0;
                    alg.bufs[b].clear();
                    alg.buf_counts[b] = 0;
                }
                let mut sf = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&alg.state_path)?;
                write_state_to(&mut sf, &alg.state)?;
                alg.state_file = Some(sf);
                File::create(&alg.user_path)?;
                alg.counts = 0;
                alg.user_counts = 0;
                alg.bytes = (BUCKETS * STATE_REC) as u64;
                if alg.enabled {
                    alg.user_file = Some(
                        OpenOptions::new()
                            .append(true)
                            .open(&alg.user_path)?,
                    );
                }
            }
        }
        self.next = 0;
        self.last_index = None;
        self.last_text.clear();
        Ok(())
    }

    /// Hash user text, save it (text included, it cannot be derived) and return
    /// the hex digests.
    pub fn encode(&mut self, text: &[u8]) -> io::Result<[String; 4]> {
        let d = [
            sha::sha1(text).to_vec(),
            sha::sha256(text).to_vec(),
            sha::sha384(text).to_vec(),
            sha::sha512(text).to_vec(),
        ];

        for a in 0..4 {
            if !self.algs[a].enabled {
                continue;
            }
            let mut rec = Vec::with_capacity(2 + text.len() + d[a].len());
            rec.extend_from_slice(&(text.len() as u16).to_le_bytes());
            rec.extend_from_slice(text);
            rec.extend_from_slice(&d[a]);
            match self.algs[a].user_file.as_mut() {
                Some(f) => f.write_all(&rec)?,
                None => return Err(io::Error::other("user db file is not open")),
            }
            self.algs[a].bytes += rec.len() as u64;
            self.algs[a].user_counts += 1;
        }

        Ok([
            sha::hex(&d[0]),
            sha::hex(&d[1]),
            sha::hex(&d[2]),
            sha::hex(&d[3]),
        ])
    }

    /// Pairs of entries that share a 55-bit shard key. Full digests are always
    /// re-checked on lookup, so these are index crossovers, not digest
    /// collisions. Returns (pair count, examples).
    pub fn scan_crossovers(&self, alg: usize, limit: usize) -> io::Result<(u64, Vec<Crossover>)> {
        let a = &self.algs[alg];
        let mut pairs = 0u64;
        let mut out: Vec<Crossover> = Vec::new();
        let mut entries: Vec<(u64, u64)> = Vec::new();

        for b in 0..BUCKETS {
            entries.clear();
            let f = match File::open(&a.paths[b]) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut r = BufReader::with_capacity(1 << 20, f);
            let mut hdr = [0u8; FILE_HDR];
            if r.read_exact(&mut hdr).is_err() {
                continue;
            }
            let mut idx = 0u64;
            let mut kbuf = [0u8; KEY_BYTES];
            while r.read_exact(&mut kbuf).is_ok() {
                let (d, _) = match read_varint(&mut r) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                idx += d;
                entries.push((key_from(&kbuf), idx));
            }
            if entries.len() < 2 {
                continue;
            }
            entries.sort_unstable_by_key(|e| e.0);
            let mut i = 0;
            while i + 1 < entries.len() {
                if entries[i].0 != entries[i + 1].0 {
                    i += 1;
                    continue;
                }
                let mut j = i;
                while j + 1 < entries.len() && entries[j + 1].0 == entries[i].0 {
                    j += 1;
                }
                let run = (j - i + 1) as u64;
                pairs += run * (run - 1) / 2;
                let key = entries[i].0;
                for k in i..j {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(Crossover {
                        shard: b,
                        a_index: entries[k].1,
                        b_index: entries[k + 1].1,
                        key,
                    });
                }
                i = j + 1;
            }
        }
        Ok((pairs, out))
    }

    /// Look many digests up at once: the encode-tab file first, then every
    /// touched shard exactly once.
    pub fn find_matches(
        &self,
        alg: usize,
        targets: &[Vec<u8>],
    ) -> io::Result<Vec<Option<Found>>> {
        let a = &self.algs[alg];
        let hlen = HLEN[alg];
        let mut out: Vec<Option<Found>> = (0..targets.len()).map(|_| None).collect();

        if let Ok(f) = File::open(&a.user_path) {
            let mut r = BufReader::with_capacity(1 << 16, f);
            let mut lenbuf = [0u8; 2];
            let mut text = Vec::new();
            let mut hash = [0u8; 64];
            loop {
                if r.read_exact(&mut lenbuf).is_err() {
                    break;
                }
                let tlen = u16::from_le_bytes(lenbuf) as usize;
                text.resize(tlen, 0);
                if r.read_exact(&mut text).is_err() || r.read_exact(&mut hash[..hlen]).is_err() {
                    break;
                }
                for (i, t) in targets.iter().enumerate() {
                    if out[i].is_none() && t.len() == hlen && hash[..hlen] == t[..] {
                        out[i] = Some(Found {
                            index: u64::MAX,
                            text: text.clone(),
                        });
                    }
                }
            }
        }

        let mut by_bucket: Vec<Vec<(u64, usize)>> = vec![Vec::new(); BUCKETS];
        for (i, t) in targets.iter().enumerate() {
            if out[i].is_some() || t.len() < PREFIX {
                continue;
            }
            let mut p = [0u8; 8];
            p.copy_from_slice(&t[..PREFIX]);
            let prefix = u64::from_be_bytes(p);
            by_bucket[bucket_of(prefix)].push((key_of(prefix), i));
        }

        let mut tbuf = [0u8; 16];
        for b in 0..BUCKETS {
            if by_bucket[b].is_empty() {
                continue;
            }
            let f = match File::open(&a.paths[b]) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut r = BufReader::with_capacity(1 << 20, f);
            let mut hdr = [0u8; FILE_HDR];
            if r.read_exact(&mut hdr).is_err() {
                continue;
            }
            let mut idx = 0u64;
            let mut kbuf = [0u8; KEY_BYTES];
            while r.read_exact(&mut kbuf).is_ok() {
                let (d, _) = match read_varint(&mut r) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                idx += d;
                let key = key_from(&kbuf);
                if by_bucket[b].iter().all(|&(k, _)| k != key) {
                    continue;
                }
                let tlen = text_at(idx, &mut tbuf);
                let text = &tbuf[..tlen];
                let d = digest(alg, text);
                for &(k, ti) in &by_bucket[b] {
                    if k == key && out[ti].is_none() && d == targets[ti] {
                        out[ti] = Some(Found {
                            index: idx,
                            text: text.to_vec(),
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    /// Find the text for one hash: the encode-tab file first (exact digests),
    /// then the single shard that can hold it.
    pub fn lookup(&self, alg: usize, target: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let a = &self.algs[alg];
        let hlen = HLEN[alg];

        // Text typed into the encode tab: stored with its full digest.
        if let Ok(f) = File::open(&a.user_path) {
            let mut r = BufReader::with_capacity(1 << 16, f);
            let mut lenbuf = [0u8; 2];
            let mut text = Vec::new();
            let mut hash = [0u8; 64];
            loop {
                if r.read_exact(&mut lenbuf).is_err() {
                    break;
                }
                let tlen = u16::from_le_bytes(lenbuf) as usize;
                text.resize(tlen, 0);
                if r.read_exact(&mut text).is_err() || r.read_exact(&mut hash[..hlen]).is_err() {
                    break;
                }
                if &hash[..hlen] == target {
                    return Ok(Some(text));
                }
            }
        }

        let mut p = [0u8; 8];
        p.copy_from_slice(&target[..PREFIX]);
        let prefix = u64::from_be_bytes(p);
        let bucket = bucket_of(prefix);
        let key = key_of(prefix);
        let mut tbuf = [0u8; 16];

        // Records still sitting in the write buffer are newer than the file.
        let mut idx = a.state[bucket].0;
        let buf = &a.bufs[bucket];
        let mut i = 0;
        while i + KEY_BYTES <= buf.len() {
            let k = key_from(&buf[i..i + KEY_BYTES]);
            i += KEY_BYTES;
            let (d, n) = match get_varint(&buf[i..]) {
                Some(v) => v,
                None => break,
            };
            i += n;
            idx += d;
            if k == key {
                let tlen = text_at(idx, &mut tbuf);
                let text = &tbuf[..tlen];
                if digest(alg, text) == target {
                    return Ok(Some(text.to_vec()));
                }
            }
        }

        let f = match File::open(&a.paths[bucket]) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let mut r = BufReader::with_capacity(1 << 20, f);
        let mut hdr = [0u8; FILE_HDR];
        if r.read_exact(&mut hdr).is_err() {
            return Ok(None);
        }
        let mut idx = 0u64;
        let mut kbuf = [0u8; KEY_BYTES];
        loop {
            if r.read_exact(&mut kbuf).is_err() {
                break;
            }
            let (d, _) = match read_varint(&mut r) {
                Ok(v) => v,
                Err(_) => break,
            };
            idx += d;
            if key_from(&kbuf) == key {
                let tlen = text_at(idx, &mut tbuf);
                let text = &tbuf[..tlen];
                if digest(alg, text) == target {
                    return Ok(Some(text.to_vec()));
                }
            }
        }
        Ok(None)
    }
}

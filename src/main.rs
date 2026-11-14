mod sha;
mod store;

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use store::{Store, HLEN, NAMES};

const INDEX_HTML: &str = include_str!("../web/index.html");
const MAX_TEXT: usize = 4096;

struct Session {
    started: Instant,
    produced_at_start: u64,
}

#[derive(Clone, Default)]
struct Limits {
    storage_bytes: u64,
    per_db: u64,
    total_entries: u64,
}

impl Limits {
    /// Returns a human reason when any configured stop rule is met.
    fn exceeded(&self, counts: &[u64; 4], sizes: &[u64; 4]) -> Option<String> {
        let total_bytes: u64 = sizes.iter().sum();
        if self.storage_bytes > 0 && total_bytes >= self.storage_bytes {
            return Some(format!(
                "total storage reached {:.2} GB",
                total_bytes as f64 / 1e9
            ));
        }
        if self.per_db > 0 {
            if let Some(a) = (0..4).find(|&a| counts[a] >= self.per_db) {
                return Some(format!(
                    "{} reached {} entries",
                    NAMES[a], self.per_db
                ));
            }
        }
        if self.total_entries > 0 {
            let total: u64 = counts.iter().sum();
            if total >= self.total_entries {
                return Some(format!("total entries reached {}", self.total_entries));
            }
        }
        None
    }

    fn describe(&self) -> String {
        if self.storage_bytes == 0 && self.per_db == 0 && self.total_entries == 0 {
            return "no stop rule set".to_string();
        }
        let mut parts = Vec::new();
        if self.storage_bytes > 0 {
            parts.push(format!("storage {:.2} GB", self.storage_bytes as f64 / 1e9));
        }
        if self.per_db > 0 {
            parts.push(format!("per db {}", self.per_db));
        }
        if self.total_entries > 0 {
            parts.push(format!("total {}", self.total_entries));
        }
        parts.join(" · ")
    }
}

/// Rolling measurement of how fast the databases are growing.
struct Rate {
    last: Instant,
    bytes: u64,
    bps: f64,
}

impl Default for Rate {
    fn default() -> Self {
        Rate {
            last: Instant::now(),
            bytes: 0,
            bps: 0.0,
        }
    }
}

struct App {
    store: Mutex<Store>,
    running: AtomicBool,
    produced: AtomicU64,
    session: Mutex<Option<Session>>,
    gen: Mutex<Option<thread::JoinHandle<()>>>,
    limits: Mutex<Limits>,
    stopped_by: Mutex<Option<String>>,
    rate: Mutex<Rate>,
}

impl App {
    fn status_json(&self) -> String {
        let (counts, sizes, enabled, next, last_text, next_text) = {
            let s = self.store.lock().unwrap();
            (
                s.counts(),
                s.sizes(),
                s.enabled(),
                s.next_index(),
                s.last_text().to_vec(),
                s.next_text(),
            )
        };
        let running = self.running.load(Ordering::Relaxed);
        let produced = self.produced.load(Ordering::Relaxed);
        let limits = self.limits.lock().unwrap().clone();
        let stopped_by = self.stopped_by.lock().unwrap().clone().unwrap_or_default();
        let write_bps = if running {
            self.rate.lock().unwrap().bps
        } else {
            0.0
        };

        let rate = {
            let g = self.session.lock().unwrap();
            match &*g {
                Some(s) if running => {
                    let secs = s.started.elapsed().as_secs_f64();
                    if secs > 0.001 {
                        (produced - s.produced_at_start) as f64 / secs
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            }
        };

        let total: u64 = counts.iter().sum();
        let mut j = String::with_capacity(512);
        j.push('{');
        j.push_str(&format!("\"running\":{},", running));
        j.push_str(&format!("\"produced\":{},", produced));
        j.push_str(&format!("\"total\":{},", total));
        j.push_str(&format!("\"rate\":{:.0},", rate));
        j.push_str(&format!("\"next_index\":{},", next));
        j.push_str(&format!("\"next_text\":\"{}\",", json_escape(&String::from_utf8_lossy(&next_text))));
        j.push_str(&format!("\"last_text\":\"{}\",", json_escape(&String::from_utf8_lossy(&last_text))));
        j.push_str("\"sizes\":{");
        for (i, n) in NAMES.iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            j.push_str(&format!("\"{}\":{}", n, sizes[i]));
        }
        j.push_str(&format!("}},\"size_total\":{},", sizes.iter().sum::<u64>()));
        j.push_str(&format!("\"write_bps\":{:.0},", write_bps));
        j.push_str("\"enabled\":{");
        for (i, n) in NAMES.iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            j.push_str(&format!("\"{}\":{}", n, enabled[i]));
        }
        j.push_str("},");
        j.push_str("\"entries\":{");
        for (i, n) in NAMES.iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            j.push_str(&format!("\"{}\":{}", n, counts[i]));
        }
        j.push_str("},");
        j.push_str(&format!(
            "\"limits\":{{\"storage_bytes\":{},\"per_db\":{},\"total_entries\":{}}},",
            limits.storage_bytes, limits.per_db, limits.total_entries
        ));
        j.push_str(&format!("\"limits_text\":\"{}\",", json_escape(&limits.describe())));
        j.push_str(&format!(
            "\"stopped_by\":\"{}\"}}",
            json_escape(&stopped_by)
        ));
        j
    }

    fn start(self: &Arc<Self>) {
        let mut slot = self.gen.lock().unwrap();
        // One generator at a time: if a worker is already running, this is a no-op.
        // Otherwise make sure the previous one really exited before spawning again,
        // so two workers can never append the same sequence positions.
        if self.running.load(Ordering::SeqCst) {
            return;
        }
        if let Some(h) = slot.take() {
            let _ = h.join();
        }
        self.running.store(true, Ordering::SeqCst);
        *self.stopped_by.lock().unwrap() = None;
        {
            let bytes: u64 = self.store.lock().unwrap().sizes().iter().sum();
            *self.rate.lock().unwrap() = Rate {
                last: Instant::now(),
                bytes,
                bps: 0.0,
            };
        }
        *self.session.lock().unwrap() = Some(Session {
            started: Instant::now(),
            produced_at_start: self.produced.load(Ordering::Relaxed),
        });
        let app = self.clone();
        *slot = Some(thread::spawn(move || generate(app)));
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        let h = self.gen.lock().unwrap().take();
        if let Some(h) = h {
            let _ = h.join();
        }
        *self.session.lock().unwrap() = None;
    }
}

fn generate(app: Arc<App>) {
    let nthreads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16);

    const BATCH: u64 = 32_768;
    let per = BATCH / nthreads as u64;

    while app.running.load(Ordering::Relaxed) {
        let (base, enabled) = {
            let s = app.store.lock().unwrap();
            (s.next_index(), s.enabled())
        };
        if !enabled.iter().any(|e| *e) {
            app.running.store(false, Ordering::SeqCst);
            *app.stopped_by.lock().unwrap() = Some("no algorithms enabled".to_string());
            break;
        }

        let mut parts: Vec<[Vec<u8>; 4]> = Vec::with_capacity(nthreads);
        thread::scope(|sc| {
            let mut handles = Vec::with_capacity(nthreads);
            for t in 0..nthreads {
                let start = base + t as u64 * per;
                handles.push(sc.spawn(move || store::build_chunk(start, per, enabled)));
            }
            for h in handles {
                parts.push(h.join().unwrap_or_else(|_| std::array::from_fn(|_| Vec::new())));
            }
        });

        let total: u64 = parts.len() as u64 * per;
        if total == 0 {
            break;
        }

        let (hit, bytes_now) = {
            let mut s = app.store.lock().unwrap();
            if let Err(e) = s.write_gen(&parts, base, total) {
                eprintln!("db write failed: {e}");
                app.running.store(false, Ordering::SeqCst);
                break;
            }
            let sizes = s.sizes();
            let lim = app.limits.lock().unwrap();
            (
                lim.exceeded(&s.counts(), &sizes),
                sizes.iter().sum::<u64>(),
            )
        };
        app.produced.fetch_add(total, Ordering::Relaxed);

        {
            let mut r = app.rate.lock().unwrap();
            let now = Instant::now();
            let dt = now.duration_since(r.last).as_secs_f64();
            if dt >= 0.2 {
                r.bps = bytes_now.saturating_sub(r.bytes) as f64 / dt;
                r.last = now;
                r.bytes = bytes_now;
            }
        }

        if let Some(reason) = hit {
            app.running.store(false, Ordering::SeqCst);
            *app.stopped_by.lock().unwrap() = Some(reason);
            break;
        }
    }

    // Stopping flushes every buffered shard, so a restart resumes exactly here.
    if let Ok(mut s) = app.store.lock() {
        if let Err(e) = s.flush() {
            eprintln!("flushing the databases failed: {e}");
        }
    }
}

// ------------------------------------------------------------------ http ----

/// Server-sent events: one long-lived response pushing status frames. Unlike
/// timers, this keeps flowing when the tab is in the background.
fn stream_status(stream: &mut TcpStream, app: &Arc<App>) -> io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    stream.set_read_timeout(Some(Duration::from_millis(1)))?;
    stream.set_nodelay(true)?;

    loop {
        let msg = format!("data: {}\n\n", app.status_json());
        if stream.write_all(msg.as_bytes()).is_err() || stream.flush().is_err() {
            break;
        }

        // Bail out if the browser went away (read of 0 = clean close).
        let mut probe = [0u8; 64];
        match stream.read(&mut probe) {
            Ok(0) => break,
            Ok(_) => {}
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                {}
            }
            Err(_) => break,
        }

        let running = app.running.load(Ordering::Relaxed);
        thread::sleep(Duration::from_millis(if running { 25 } else { 400 }));
    }
    Ok(())
}

fn field(body: &[u8], key: &str) -> String {
    let text = String::from_utf8_lossy(body);
    for pair in text.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == key {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

fn parse_limits(body: &[u8]) -> Limits {
    let text = String::from_utf8_lossy(body);
    let mut l = Limits::default();
    for pair in text.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let num: f64 = v.trim().parse().unwrap_or(0.0);
        let num = if num.is_finite() && num > 0.0 { num } else { 0.0 };
        match k.trim() {
            "storage_gb" => l.storage_bytes = (num * 1e9) as u64,
            "per_db" => l.per_db = num as u64,
            "total_entries" => l.total_entries = num as u64,
            _ => {}
        }
    }
    l
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        ctype,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn json(stream: &mut TcpStream, body: &str) -> io::Result<()> {
    respond(stream, "200 OK", "application/json", body.as_bytes())
}

fn bad_request(stream: &mut TcpStream, msg: &str) -> io::Result<()> {
    json(
        stream,
        &format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(msg)),
    )
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn handle(mut stream: TcpStream, app: &Arc<App>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_nodelay(true)?;

    let mut data = Vec::with_capacity(1024);
    let mut tmp = [0u8; 8192];
    let mut header_end: Option<usize> = None;
    let mut content_len = 0usize;

    loop {
        if header_end.is_none() {
            if let Some(p) = find(&data, b"\r\n\r\n") {
                header_end = Some(p + 4);
                let head = String::from_utf8_lossy(&data[..p]);
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
        }
        if let Some(he) = header_end {
            if data.len() >= he + content_len {
                break;
            }
            if content_len > MAX_TEXT * 8 {
                return bad_request(&mut stream, "request body too large");
            }
        }
        if data.len() > 1 << 20 {
            return bad_request(&mut stream, "request too large");
        }
        let n = match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                break
            }
            Err(e) => return Err(e),
        };
        data.extend_from_slice(&tmp[..n]);
    }

    let he = match header_end {
        Some(he) => he,
        None => return respond(&mut stream, "400 Bad Request", "text/plain", b"bad request"),
    };
    let head = String::from_utf8_lossy(&data[..he]);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let end = (he + content_len).min(data.len());
    let body = &data[he.min(end)..end];

    match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes())
        }
        ("GET", "/api/status") => json(&mut stream, &app.status_json()),
        ("GET", "/api/stream") => stream_status(&mut stream, app),
        ("POST", "/api/start") => {
            app.start();
            json(&mut stream, &app.status_json())
        }
        ("POST", "/api/stop") => {
            app.stop();
            json(&mut stream, &app.status_json())
        }
        ("POST", "/api/toggle") => {
            let alg = field(body, "alg");
            let on = field(body, "on") != "0";
            match NAMES.iter().position(|n| *n == alg) {
                Some(i) => {
                    let r = {
                        let mut s = app.store.lock().unwrap();
                        s.set_enabled(i, on)
                    };
                    match r {
                        Ok(()) => json(&mut stream, &app.status_json()),
                        Err(e) => bad_request(&mut stream, &format!("Could not toggle: {e}")),
                    }
                }
                None => bad_request(&mut stream, "Unknown algorithm."),
            }
        }
        ("POST", "/api/remove") => {
            let alg = field(body, "alg");
            match NAMES.iter().position(|n| *n == alg) {
                Some(i) => {
                    let r = {
                        let mut s = app.store.lock().unwrap();
                        s.remove(i)
                    };
                    match r {
                        Ok(()) => json(&mut stream, &app.status_json()),
                        Err(e) => bad_request(&mut stream, &format!("Could not delete: {e}")),
                    }
                }
                None => bad_request(&mut stream, "Unknown algorithm."),
            }
        }
        ("POST", "/api/limits") => {
            let l = parse_limits(body);
            *app.limits.lock().unwrap() = l;
            json(&mut stream, &app.status_json())
        }
        ("POST", "/api/clear") => {
            app.stop();
            let cleared = {
                let mut s = app.store.lock().unwrap();
                s.clear()
            };
            match cleared {
                Ok(()) => {
                    app.produced.store(0, Ordering::Relaxed);
                    *app.stopped_by.lock().unwrap() = None;
                    *app.session.lock().unwrap() = None;
                    json(&mut stream, &app.status_json())
                }
                Err(e) => bad_request(&mut stream, &format!("Could not empty the databases: {e}")),
            }
        }
        ("POST", "/api/encode") => {
            if body.is_empty() {
                return bad_request(&mut stream, "Type something to encode first.");
            }
            if body.len() > MAX_TEXT {
                return bad_request(&mut stream, "Text is too long (max 4096 bytes).");
            }
            let hashes = {
                let mut s = app.store.lock().unwrap();
                s.encode(body)
            };
            match hashes {
                Ok(h) => json(
                    &mut stream,
                    &format!(
                        "{{\"ok\":true,\"saved\":true,\"sha1\":\"{}\",\"sha256\":\"{}\",\"sha384\":\"{}\",\"sha512\":\"{}\"}}",
                        h[0], h[1], h[2], h[3]
                    ),
                ),
                Err(e) => bad_request(&mut stream, &format!("Could not save: {e}")),
            }
        }
        ("POST", "/api/decode") => {
            let text = String::from_utf8_lossy(body);
            let hex = text.trim();
            let alg = match hex.len() {
                40 => 0usize,
                64 => 1,
                96 => 2,
                128 => 3,
                _ => {
                    return bad_request(
                        &mut stream,
                        "That is not a hash. Expected 40, 64, 96 or 128 hex characters.",
                    )
                }
            };
            let target = match sha::from_hex(hex) {
                Some(t) if t.len() == HLEN[alg] => t,
                _ => return bad_request(&mut stream, "That hash contains non-hex characters."),
            };
            let found = app.store.lock().unwrap().lookup(alg, &target);
            match found {
                Ok(Some(found)) => json(
                    &mut stream,
                    &format!(
                        "{{\"ok\":true,\"found\":true,\"alg\":\"{}\",\"text\":\"{}\"}}",
                        NAMES[alg],
                        json_escape(&String::from_utf8_lossy(&found))
                    ),
                ),
                Ok(None) => json(
                    &mut stream,
                    &format!("{{\"ok\":true,\"found\":false,\"alg\":\"{}\"}}", NAMES[alg]),
                ),
                Err(e) => bad_request(&mut stream, &format!("Could not read db: {e}")),
            }
        }
        ("GET", "/favicon.ico") => respond(&mut stream, "204 No Content", "text/plain", b""),
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn main() {
    let mut port: u16 = 8080;
    let mut data_dir = PathBuf::from("data");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                if let Some(p) = args.get(i).and_then(|v| v.parse().ok()) {
                    port = p;
                }
            }
            "--data" | "-d" => {
                i += 1;
                if let Some(d) = args.get(i) {
                    data_dir = PathBuf::from(d);
                }
            }
            other => {
                if let Ok(p) = other.parse::<u16>() {
                    port = p;
                }
            }
        }
        i += 1;
    }

    let store = match Store::open(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open data folder {}: {e}", data_dir.display());
            std::process::exit(1);
        }
    };

    let resume_at: u64 = store.next_index();
    let last = store.last_text().to_vec();
    let counts = store.counts();

    let app = Arc::new(App {
        store: Mutex::new(store),
        running: AtomicBool::new(false),
        produced: AtomicU64::new(0),
        session: Mutex::new(None),
        gen: Mutex::new(None),
        limits: Mutex::new(Limits::default()),
        stopped_by: Mutex::new(None),
        rate: Mutex::new(Rate::default()),
    });

    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {addr}: {e}");
            std::process::exit(1);
        }
    };

    println!("sha-decoder running at http://{addr}");
    println!("db files in {}", abs(&data_dir).display());
    println!(
        "entries: sha1={} sha256={} sha384={} sha512={}",
        counts[0], counts[1], counts[2], counts[3]
    );
    if let Some(_) = last.first() {
        println!(
            "resume point: index {resume_at} (last generated text {:?})",
            String::from_utf8_lossy(&last)
        );
    } else {
        println!("resume point: index 0 (empty databases)");
    }

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let app = app.clone();
                thread::spawn(move || {
                    let _ = handle(s, &app);
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
}

fn abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

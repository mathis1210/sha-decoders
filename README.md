# sha-decoder

Enumerate every text in sequence — `a`, `b`, `c`, … `z`, `aa`, `ab`, … — hash each one with
SHA-1, SHA-256, SHA-384 and SHA-512 into four databases, and turn any of those hashes back into
the text it came from.

It is a single Rust binary with **no dependencies**, and it ships its own web UI. No Node, no
SQLite, no build toolchain beyond `cargo`.

```
text  ──►  sha1  ──►  data/sha1/      ──►  look up a hash, get the text back
      ──►  sha256 ──►  data/sha256/
      ──►  sha384 ──►  data/sha384/
      ──►  sha512 ──►  data/sha512/
```

## Run it

You need a recent stable [Rust toolchain](https://rustup.rs) (developed and tested on 1.100;
`cargo` and `rustc` on your `PATH`).

```bash
git clone https://github.com/<your-username>/sha-decoder.git
cd sha-decoder
cargo run --release
```

Then open **http://127.0.0.1:8080**.

The first build takes a few seconds; after that the binary starts instantly. Stop it with
`Ctrl+C`.

### Options

```
sha-decoder [--port PORT] [--data DIR]

  --port, -p   port to listen on (default 8080, binds 127.0.0.1 only)
  --data, -d   folder for the databases (default ./data)
```

```bash
cargo run --release -- --port 9000 --data /tmp/sha-dbs
```

## Using the page

Everything lives on one page, in three tabs.

**Status** — Start and Stop generation, live entry counts, storage used, write speed, the text
the next start will resume from, and the database controls. Every database has a power button
(keep writing to it or not) and a trash button (delete its data and switch it off).

**Encode / Decode** — Type any text to hash it and save it into the enabled databases, or paste
a hash (40, 64, 96 or 128 hex characters) to get its text back.

**Storage** — Size of each database, entries per database, and the stop rules.

The page updates from a Server-Sent Events stream, so counts and sizes move continuously while
generation is running.

### Stop rules

Three optional rules, each independently switched off by leaving the field empty:

| field | stops generation when… |
|---|---|
| Total storage (GB) | the four databases together reach that size |
| Entries per db | any single database reaches that many entries |
| Total entries | all four databases together reach that many entries |

The generator stops with a reason shown in the status bar, and flushes what it wrote.

## Command-line output

On start the server prints what it found:

```
sha-decoder running at http://127.0.0.1:8080
db files in /path/to/data
entries: sha1=1376257 sha256=1376257 sha384=1376257 sha512=1376257
resume point: index 1376256 (last generated text "a4T]")
```

Stopping writes everything to disk, and starting again continues from the next text in the
sequence — restarting the process picks up exactly where it left off.

## The sequence

Texts are generated in blocks of increasing length. Within a block the order is the character
order: `a`…`z`, `A`…`Z`, `0`…`9`, punctuation on a US keyboard, then space — 95 characters in
all, i.e. base-95 counting:

```
a b c … z A … Z 0 … 9 ! " # … ~ space aa ab ac … az aA … zz aaa …
```

## How storage works

The text at any position is a pure function of that position, so no database stores the text it
hashed — only enough information to find it again.

Each database is a folder of **512 shards**, split by the first 9 bits of the digest. The
remaining 55 bits of the first 8 digest bytes are stored, followed by the distance back to the
previous entry in that shard, as a variable-length integer:

```
record = [ 7 bytes: digest bits 9..63 ][ 1–5 bytes: index delta ]
```

That works out to **~8.8 bytes per entry per database** (and a fixed 12 KB bookkeeping file):

| entries per db | one database | all four |
|---|---|---|
| 1 million | ~9 MB | ~35 MB |
| 100 million | ~880 MB | ~3.5 GB |
| 1 billion | ~8.8 GB | ~35 GB |

With the naive "store the text and the hash" layout the same billion entries take roughly
200 GB.

Splitting by prefix is what keeps decoding fast: a lookup reads **one shard**, about 1/512th of
the database, instead of the whole file. A candidate whose stored bits match is always re-hashed
in full and compared before it is returned, so the shortened key can never produce a wrong
answer.

Texts you type into the Encode tab cannot be derived from a position, so they are kept in
`{alg}.user.db` with their text and full digest.

```
data/
├── algs.conf            which algorithms are switched on
├── sha1/
│   ├── 0000.bkt … 0511.bkt
│   └── state.bin        per-shard position bookkeeping
├── sha1.user.db         text saved from the Encode tab
├── sha256/ …
├── sha384/ …
└── sha512/ …
```

Shards are created when data first arrives and opened lazily, so startup reads a few kilobytes
of bookkeeping rather than every file — even with hundreds of millions of entries stored.

## Performance

Measured on a Windows desktop with the four databases enabled, this build:

| | |
|---|---|
| generation | ~3.0 million entries/s, ~118 MB/s written |
| storage | 8.78 bytes per entry per database |
| lookup, 368 M entries stored (3.0 GB) | 13–16 ms |
| lookup, 39 M entries stored | ~2.5 ms |
| startup with 368 M entries stored | under 1 s |

Generation is parallelised across the CPU cores, and a disabled algorithm costs no hashing at
all.

## HTTP API

The page is a thin client over these endpoints; they are handy for scripting too.

| method | path | body | returns |
|---|---|---|---|
| GET | `/api/status` | — | counts, sizes, speeds, limits, resume point |
| GET | `/api/stream` | — | Server-Sent Events with the same status |
| POST | `/api/start` | — | starts generation |
| POST | `/api/stop` | — | stops generation |
| POST | `/api/limits` | `storage_gb=&per_db=&total_entries=` | updated status |
| POST | `/api/clear` | — | empties every database, sequence back to `a` |
| POST | `/api/toggle` | `alg=sha256&on=0` | updated status |
| POST | `/api/remove` | `alg=sha256` | updated status |
| POST | `/api/encode` | raw UTF-8 text | `{ok, saved, sha1, sha256, sha384, sha512}` |
| POST | `/api/decode` | raw hex hash | `{ok, found, alg, text}` |

```bash
curl -X POST --data-binary "hello" http://127.0.0.1:8080/api/encode
curl -X POST --data-binary "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d" \
     http://127.0.0.1:8080/api/decode
```

## Project layout

```
src/main.rs     HTTP/1.1 server, generator threads, stop rules, SSE
src/sha.rs      SHA-1, SHA-256, SHA-384 and SHA-512, written from scratch
src/store.rs    sequence, shard files, encode/decode storage
web/index.html  the entire UI (HTML, CSS, JS in one file, embedded in the binary)
```

## Notes

- The server binds to `127.0.0.1` only. There is no authentication; do not expose it to a
  network without putting something in front of it.
- Both databases and bookkeeping are just files: delete `data/` to start over, or point
  `--data` at another folder.
- Inserts are appended and flushed in 32 KB chunks per shard. If the process is killed hard,
  each shard loses at most its unflushed 32 KB; the next start repairs the shards and carries
  on from the last complete record.
- Databases written by earlier versions of this project (single `.db` files with `SHADB01` or
  `SHD2` headers, for example `data/sha1.db`) are ignored by this version. Delete them to
  reclaim the space.

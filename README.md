# dngeat

Convert camera RAW files (Sony ARW and friends) to DNG in place, verify the
conversion with an independent decoder, then **delete the originals** to save
space.

Think of it as `dnglab convert` (it uses [rawler], dnglab's engine, as a
library) followed by a very paranoid `rm` — designed to run for hours against a
NAS over slow wifi without breaking anything.

[rawler]: https://crates.io/crates/rawler

## How it works

Two phases:

1. **Scan** — walks the tree once, sequentially and in sorted order (gentle on
   network shares), and builds the work plan. Files more recent than
   `--min-age-days` (default 180) are excluded. Stale `.dng.part` files from a
   previous interrupted run are cleaned up.
2. **Process** — one file at a time, no I/O parallelization, with a byte-based
   progress bar. For each RAW:

   1. If the source is on network storage (auto-detected via `statfs`, or
      forced with `--assume-network`/`--assume-local`), copy the RAW to a
      local temp dir and verify the copy with SHA-256.
   2. Convert to DNG locally with rawler (lossless compression, preview +
      thumbnail, original RAW *not* embedded by default).
   3. Decode both the RAW and the DNG with **LibRaw** — a C library sharing no
      code with rawler — and compare the sensor data bit for bit. A systematic
      decoding bug can't agree with itself across two independent
      implementations.
   4. Upload the DNG next to the RAW as `.dng.part`, read it back and compare
      checksums (skippable with `--no-remote-verify`).
   5. Atomically rename `.dng.part` → `.dng` (mtime preserved from the RAW).
   6. Only then delete the original RAW.
   7. Clean up temp files.

### Crash safety / resumability

The ordering guarantees that at any point in time every picture exists either
as its original RAW or as a fully verified DNG. Kill the process, unplug the
NAS, let the SMB share die — nothing is lost, and the next run picks up where
the previous one left:

- RAW without DNG → converted normally.
- RAW **and** DNG side by side (killed between upload and deletion) → the
  existing DNG is verified against the RAW with LibRaw; if it matches, the RAW
  is deleted, otherwise both files are kept and the pair is reported.
- Leftover `.dng.part` → deleted at scan time, the RAW is re-processed.
- Failures on individual files (network error, undecodable file...) are
  reported and skipped; the run continues and exits non-zero at the end.

Ctrl-C stops gracefully after the current step; a second Ctrl-C force-quits.

## Building

You need a Rust toolchain and the LibRaw development headers (used by the
verification step, linked at build time):

```sh
# Debian/Ubuntu
sudo apt install libraw-dev

# Fedora
sudo dnf install LibRaw-devel

# Arch
sudo pacman -S libraw

# macOS
brew install libraw
```

Then:

```sh
cargo build --release
```

The binary lands in `target/release/dngeat`.

## Usage

```sh
# See what would happen, touch nothing
dngeat --dry-run /mnt/nas/photos

# The real thing
dngeat --artist 'Rémy Sanchez' /mnt/nas/photos

# Convert everything regardless of age, but keep the RAWs
dngeat --min-age-days 0 --keep-raw /mnt/nas/photos
```

All options:

```
Usage: dngeat [OPTIONS] <ROOT>

Arguments:
  <ROOT>  Directory to scan for RAW files

Options:
      --min-age-days <MIN_AGE_DAYS>  Only process files whose modification time is older than this many days [default: 180]
      --artist <ARTIST>              Artist tag to embed in the DNG
      --embed-raw                    Embed the original RAW file inside the DNG (defeats the space saving)
      --tmp-dir <TMP_DIR>            Local fast temp directory (default: $TMPDIR/dngeat)
      --ext <EXT>                    RAW extensions to process, comma separated, case-insensitive [default: arw,cr2,cr3,nef,nrw,orf,raf,rw2,pef,srw]
      --assume-network               Treat the source as network storage even if detection says local
      --assume-local                 Treat the source as local storage even if detection says network
      --no-remote-verify             Skip the read-back checksum of the destination DNG after upload
      --dry-run                      Scan and print what would be done, without touching anything
      --keep-raw                     Never delete the original RAW files (conversion only)
```

## Supported formats

Anything rawler can convert **and** LibRaw can decode for verification. ARW is
the tested, primary target; the default extension list also covers CR2, CR3,
NEF, NRW, ORF, RAF, RW2, PEF and SRW. If either library rejects a file, that
file is skipped with an error and its RAW is left untouched — the tool never
deletes anything it could not verify.

## License

MIT. Note that rawler is LGPL-2.1 and LibRaw is LGPL-2.1/CDDL, which is fine
for this use but worth knowing if you redistribute binaries.

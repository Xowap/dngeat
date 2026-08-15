//! Phase 2: per-file processing pipeline.
//!
//! Every step is ordered so that a crash at any point leaves the tree in a
//! state the next run can recover from:
//!
//! 1. (network) copy RAW to local temp, verify the copy with a checksum
//! 2. convert to DNG in the temp dir (rawler, same engine as dnglab)
//! 3. decode both files with two independent libraries and compare sensor data
//! 4. copy DNG next to the RAW as `.dng.part`, read it back to checksum it
//! 5. atomically rename `.dng.part` -> `.dng`
//! 6. delete the original RAW
//! 7. clean the temp files
//!
//! The RAW is only ever deleted after the final DNG is fully in place, so the
//! invariant "every picture exists as RAW or as verified DNG" always holds.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use filetime::FileTime;
use rawler::dng::convert::{convert_raw_file, ConvertParams};
use rawler::dng::{CropMode, DngCompression, DngPhotometricConversion};
use sha2::{Digest, Sha256};

use crate::scan::{Candidate, Kind};
use crate::{check_stop, Args};

pub struct Ctx<'a> {
    pub args: &'a Args,
    pub network: bool,
    pub tmp: &'a Path,
}

pub enum Outcome {
    Converted { saved: i64 },
    Resolved { saved: i64 },
    SkippedMismatch,
}

const COPY_BUF: usize = 4 << 20; // 4 MiB: large sequential reads, NAS-friendly

/// Copy `src` to `dst` sequentially and return the SHA-256 of what was read.
fn copy_hashed(src: &Path, dst: &Path) -> Result<[u8; 32]> {
    let mut reader = BufReader::with_capacity(
        COPY_BUF,
        File::open(src).with_context(|| format!("open {}", src.display()))?,
    );
    let mut writer = BufWriter::with_capacity(
        COPY_BUF,
        File::create(dst).with_context(|| format!("create {}", dst.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        check_stop()?;
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
    }
    writer.flush()?;
    writer.get_ref().sync_all().context("fsync destination")?;
    Ok(hasher.finalize().into())
}

/// SHA-256 of a file, read sequentially.
fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut reader = BufReader::with_capacity(
        COPY_BUF,
        File::open(path).with_context(|| format!("open {}", path.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        check_stop()?;
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Build the rawler conversion parameters from CLI flags. Mirrors dnglab's
/// defaults (lossless compression, preview + thumbnail) except that embedding
/// the original RAW is off by default since the whole point is saving space.
fn convert_params(args: &Args) -> ConvertParams {
    ConvertParams {
        embedded: args.embed_raw,
        compression: DngCompression::Lossless,
        photometric_conversion: DngPhotometricConversion::Original,
        apply_scaling: false,
        crop: CropMode::Best,
        predictor: 1,
        preview: true,
        thumbnail: true,
        artist: args.artist.clone(),
        software: format!("dngeat {} (rawler)", env!("CARGO_PKG_VERSION")),
        index: 0,
        keep_mtime: true,
    }
}

/// Guard that removes temp files on drop.
struct TmpGuard(Vec<PathBuf>);
impl Drop for TmpGuard {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Process one work item, dispatching on its kind. `step` reports the current
/// sub-step to the progress bar.
pub fn process(item: &Candidate, ctx: &Ctx, step: &dyn Fn(&str)) -> Result<Outcome> {
    match item.kind {
        Kind::Convert => convert_one(item, ctx, step),
        Kind::ResolveExisting => resolve_existing(item, ctx, step),
    }
}

/// Full conversion of a RAW that has no DNG yet.
fn convert_one(item: &Candidate, ctx: &Ctx, step: &dyn Fn(&str)) -> Result<Outcome> {
    check_stop()?;
    let raw_meta =
        std::fs::metadata(&item.raw).with_context(|| format!("stat {}", item.raw.display()))?;
    let raw_mtime = FileTime::from_last_modification_time(&raw_meta);

    let file_stem = item
        .raw
        .file_name()
        .context("raw file has no file name")?
        .to_string_lossy()
        .into_owned();

    // -- 1. Stage the RAW locally when the source is on the network ---------
    let mut guard = TmpGuard(Vec::new());
    let (local_raw, _staged) = if ctx.network {
        step("downloading RAW to local temp");
        let local = ctx.tmp.join(&file_stem);
        guard.0.push(local.clone());
        let copied_hash = copy_hashed(&item.raw, &local)
            .with_context(|| format!("staging {} to temp", item.raw.display()))?;
        // Paranoia: make sure the local copy is what landed on disk.
        let local_hash = hash_file(&local).context("hashing staged RAW")?;
        if copied_hash != local_hash {
            bail!("staged RAW copy is corrupted (checksum mismatch)");
        }
        (local, true)
    } else {
        (item.raw.clone(), false)
    };

    // -- 2. Convert in the temp dir ------------------------------------------
    check_stop()?;
    step("converting to DNG");
    let local_dng = ctx.tmp.join(format!("{file_stem}.dng"));
    guard.0.push(local_dng.clone());
    {
        let mut out = BufWriter::new(
            File::create(&local_dng).with_context(|| format!("create {}", local_dng.display()))?,
        );
        convert_raw_file(&local_raw, &mut out, &convert_params(ctx.args))
            .map_err(|e| anyhow::anyhow!("rawler conversion failed: {e}"))?;
        out.flush()?;
        out.get_ref().sync_all()?;
    }

    // -- 3. Cross-verify sensor data with an independent decoder ------------
    check_stop()?;
    step("verifying DNG against RAW (independent decode)");
    if !crate::verify::same_sensor_data(&local_raw, &local_dng)? {
        bail!("verification failed: DNG sensor data differs from RAW");
    }

    // -- 4. Upload as .part and read back to checksum ------------------------
    check_stop()?;
    let part = PathBuf::from(format!("{}.part", item.dng.display()));
    step("uploading DNG");
    let up_res = (|| -> Result<()> {
        let sent_hash = copy_hashed(&local_dng, &part).context("uploading DNG")?;
        if !ctx.args.no_remote_verify {
            check_stop()?;
            step("verifying remote DNG checksum");
            let remote_hash = hash_file(&part).context("reading back remote DNG")?;
            if sent_hash != remote_hash {
                bail!("remote DNG checksum mismatch after upload");
            }
        }
        Ok(())
    })();
    if let Err(err) = up_res {
        let _ = std::fs::remove_file(&part);
        return Err(err);
    }

    // Preserve the shooting date on the DNG file itself.
    let _ = filetime::set_file_mtime(&part, raw_mtime);

    // -- 5. Atomic rename into place -----------------------------------------
    // NOTE: no check_stop() from here on -- finish the critical section.
    step("finalizing");
    std::fs::rename(&part, &item.dng)
        .with_context(|| format!("renaming {} into place", part.display()))?;

    // -- 6. Delete the original ----------------------------------------------
    let dng_size = std::fs::metadata(&item.dng).map(|m| m.len()).unwrap_or(0);
    if !ctx.args.keep_raw {
        step("deleting original RAW");
        std::fs::remove_file(&item.raw)
            .with_context(|| format!("deleting {}", item.raw.display()))?;
    }

    // -- 7. Temp cleanup happens via TmpGuard --------------------------------
    Ok(Outcome::Converted {
        saved: item.size as i64 - dng_size as i64,
    })
}

/// A DNG already exists next to the RAW: a previous run probably died between
/// upload and deletion. Verify the existing DNG and, if it matches, delete the
/// RAW. If it does not match, leave both files alone and report.
fn resolve_existing(item: &Candidate, ctx: &Ctx, step: &dyn Fn(&str)) -> Result<Outcome> {
    check_stop()?;

    // Stage both files locally if on network: verification decodes them fully.
    let mut guard = TmpGuard(Vec::new());
    let (raw, dng) = if ctx.network {
        step("downloading RAW + DNG for verification");
        let file_stem = item.raw.file_name().unwrap().to_string_lossy().into_owned();
        let lraw = ctx.tmp.join(&file_stem);
        let ldng = ctx.tmp.join(format!("{file_stem}.existing.dng"));
        guard.0.push(lraw.clone());
        guard.0.push(ldng.clone());
        copy_hashed(&item.raw, &lraw).context("staging RAW")?;
        check_stop()?;
        copy_hashed(&item.dng, &ldng).context("staging DNG")?;
        (lraw, ldng)
    } else {
        (item.raw.clone(), item.dng.clone())
    };

    check_stop()?;
    step("verifying existing DNG against RAW");
    if !crate::verify::same_sensor_data(&raw, &dng)? {
        return Ok(Outcome::SkippedMismatch);
    }

    if !ctx.args.keep_raw {
        step("deleting original RAW");
        std::fs::remove_file(&item.raw)
            .with_context(|| format!("deleting {}", item.raw.display()))?;
    }

    Ok(Outcome::Resolved {
        saved: item.size as i64,
    })
}

//! Phase 1: walk the tree and build the work plan.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use walkdir::WalkDir;

use crate::Args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// No DNG next to the RAW: full conversion needed.
    Convert,
    /// A DNG already exists next to the RAW (e.g. previous run killed after
    /// upload but before deletion). Verify it and delete the RAW if it checks
    /// out.
    ResolveExisting,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub raw: PathBuf,
    pub dng: PathBuf,
    pub size: u64,
    pub kind: Kind,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<Candidate>,
    pub skipped_recent: u64,
    pub removed_parts: u64,
}

/// Destination DNG path for a given RAW: same directory, same stem, `.dng`.
pub fn dng_path_for(raw: &Path) -> PathBuf {
    raw.with_extension("dng")
}

/// Walk the tree once, sequentially and in sorted order (NAS-friendly), and
/// build the work plan: which RAWs to convert, which existing RAW+DNG pairs
/// to resolve, and how many recent files were excluded by the age filter.
pub fn scan(args: &Args, progress: impl Fn(u64, u64)) -> Result<Plan> {
    let exts: Vec<String> = args.ext.iter().map(|e| e.to_lowercase()).collect();
    let cutoff = SystemTime::now() - Duration::from_secs(args.min_age_days * 24 * 3600);

    let mut plan = Plan::default();
    let mut seen: u64 = 0;

    // Sorted walk: deterministic order, and sequential directory access is
    // gentler on network shares than random hopping.
    for entry in WalkDir::new(&args.root)
        .follow_links(false)
        .sort_by_file_name()
    {
        crate::check_stop()?;
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!("scan: skipping unreadable entry: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        seen += 1;
        if seen.is_multiple_of(64) {
            progress(plan.items.len() as u64, seen);
        }

        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            // Clean up stale temp files from a previous interrupted run.
            continue;
        };
        let ext_lc = ext.to_lowercase();

        if path.to_string_lossy().ends_with(".dng.part") {
            // Stale partial upload from a killed run: remove it, the RAW is
            // still there and will be re-processed.
            if !args.dry_run && std::fs::remove_file(path).is_ok() {
                plan.removed_parts += 1;
            }
            continue;
        }

        if !exts.iter().any(|e| e == &ext_lc) {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                eprintln!("scan: cannot stat {}: {err}", path.display());
                continue;
            }
        };

        if meta.modified().map(|m| m > cutoff).unwrap_or(false) {
            plan.skipped_recent += 1;
            continue;
        }

        let dng = dng_path_for(path);
        let kind = if dng.exists() {
            Kind::ResolveExisting
        } else {
            Kind::Convert
        };

        plan.items.push(Candidate {
            raw: path.to_path_buf(),
            dng,
            size: meta.len(),
            kind,
        });
    }

    progress(plan.items.len() as u64, seen);
    Ok(plan)
}

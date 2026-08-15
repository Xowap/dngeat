mod netfs;
mod pipeline;
mod scan;
mod verify;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};

use crate::pipeline::{Ctx, Outcome};
use crate::scan::Kind;

/// Set by the SIGINT handler; checked between (and inside) pipeline steps.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// Bail out with an error if a graceful shutdown was requested, so the
/// pipeline unwinds cleanly between steps instead of dying mid-copy.
pub fn check_stop() -> Result<()> {
    if STOP.load(Ordering::SeqCst) {
        bail!("interrupted by user");
    }
    Ok(())
}

/// Non-failing variant of [`check_stop`], for loop conditions.
pub fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

#[derive(Parser, Debug)]
#[command(
    name = "dngeat",
    version,
    about = "Convert camera RAW files to DNG in place, then delete the originals to save space.\n\
             Designed to be resumable and gentle with NAS storage over slow links."
)]
pub struct Args {
    /// Directory to scan for RAW files
    pub root: PathBuf,

    /// Only process files whose modification time is older than this many days
    #[arg(long, default_value_t = 180)]
    pub min_age_days: u64,

    /// Artist tag to embed in the DNG
    #[arg(long)]
    pub artist: Option<String>,

    /// Embed the original RAW file inside the DNG (defeats the space saving)
    #[arg(long, default_value_t = false)]
    pub embed_raw: bool,

    /// Local fast temp directory (default: $TMPDIR/dngeat)
    #[arg(long)]
    pub tmp_dir: Option<PathBuf>,

    /// RAW extensions to process, comma separated, case-insensitive
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "arw,cr2,cr3,nef,nrw,orf,raf,rw2,pef,srw"
    )]
    pub ext: Vec<String>,

    /// Treat the source as network storage even if detection says local
    #[arg(long, conflicts_with = "assume_local")]
    pub assume_network: bool,

    /// Treat the source as local storage even if detection says network
    #[arg(long)]
    pub assume_local: bool,

    /// Skip the read-back checksum of the destination DNG after upload
    #[arg(long)]
    pub no_remote_verify: bool,

    /// Scan and print what would be done, without touching anything
    #[arg(long)]
    pub dry_run: bool,

    /// Never delete the original RAW files (conversion only)
    #[arg(long)]
    pub keep_raw: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();

    ctrlc::set_handler(|| {
        if STOP.swap(true, Ordering::SeqCst) {
            // Second Ctrl-C: user really means it.
            std::process::exit(130);
        }
        eprintln!("\nStopping after the current step... (Ctrl-C again to force quit)");
    })
    .context("failed to install Ctrl-C handler")?;

    if !args.root.is_dir() {
        bail!("{} is not a directory", args.root.display());
    }

    // ---- Phase 1: scan ----------------------------------------------------
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(ProgressStyle::with_template("{spinner} scanning: {msg}").unwrap());
    spinner.enable_steady_tick(Duration::from_millis(120));

    let plan = scan::scan(&args, |found, seen| {
        spinner.set_message(format!("{found} candidates ({seen} files seen)"));
    })?;

    spinner.finish_and_clear();

    let n_convert = plan
        .items
        .iter()
        .filter(|c| c.kind == Kind::Convert)
        .count();
    let n_resolve = plan.items.len() - n_convert;
    let total_bytes: u64 = plan.items.iter().map(|c| c.size).sum();

    println!(
        "Found {} file(s) to process ({}): {} to convert, {} with an existing DNG to resolve.",
        plan.items.len(),
        HumanBytes(total_bytes),
        n_convert,
        n_resolve,
    );
    if plan.skipped_recent > 0 {
        println!(
            "Skipped {} file(s) more recent than {} days.",
            plan.skipped_recent, args.min_age_days
        );
    }
    if plan.removed_parts > 0 {
        println!("Cleaned up {} stale .dng.part file(s).", plan.removed_parts);
    }

    if plan.items.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }

    if args.dry_run {
        for c in &plan.items {
            let action = match c.kind {
                Kind::Convert => "convert",
                Kind::ResolveExisting => "resolve",
            };
            println!("{action}  {}  ({})", c.raw.display(), HumanBytes(c.size));
        }
        println!("Dry run: nothing was modified.");
        return Ok(());
    }

    // ---- Setup ------------------------------------------------------------
    let network = if args.assume_network {
        true
    } else if args.assume_local {
        false
    } else {
        match netfs::is_network_fs(&args.root) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("Could not detect filesystem type ({err}); assuming network storage.");
                true
            }
        }
    };
    println!(
        "Source storage detected as {}. Verification decoder: LibRaw {}.",
        if network {
            "NETWORK (staging files through local temp)"
        } else {
            "local"
        },
        verify::libraw_version(),
    );

    let tmp = args
        .tmp_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("dngeat"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .with_context(|| format!("cannot create temp dir {}", tmp.display()))?;

    let ctx = Ctx {
        args: &args,
        network,
        tmp: &tmp,
    };

    // ---- Phase 2: process, one file at a time ------------------------------
    let bar = ProgressBar::new(total_bytes);
    bar.set_style(
        ProgressStyle::with_template(
            "{wide_bar} {binary_bytes}/{binary_total_bytes} eta {eta}\n{msg}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    bar.enable_steady_tick(Duration::from_millis(250));

    let mut converted = 0usize;
    let mut resolved = 0usize;
    let mut mismatched = 0usize;
    let mut failed = 0usize;
    let mut saved: i64 = 0;

    let total = plan.items.len();
    for (i, item) in plan.items.iter().enumerate() {
        if stopping() {
            break;
        }
        let name = item.raw.file_name().unwrap_or_default().to_string_lossy();
        let label = format!("[{}/{}] {}", i + 1, total, name);
        bar.set_message(format!("{label}: starting"));

        let step = |s: &str| bar.set_message(format!("{label}: {s}"));
        match pipeline::process(item, &ctx, &step) {
            Ok(Outcome::Converted { saved: s }) => {
                converted += 1;
                saved += s;
            }
            Ok(Outcome::Resolved { saved: s }) => {
                resolved += 1;
                saved += s;
            }
            Ok(Outcome::SkippedMismatch) => {
                mismatched += 1;
                bar.println(format!(
                    "SKIP {}: existing DNG does not match the RAW pixel data; keeping both",
                    item.raw.display()
                ));
            }
            Err(err) if stopping() => {
                bar.println(format!("INTERRUPTED {}: {err:#}", item.raw.display()));
                break;
            }
            Err(err) => {
                failed += 1;
                bar.println(format!("FAIL {}: {err:#}", item.raw.display()));
            }
        }
        bar.inc(item.size);
    }

    bar.finish_and_clear();
    let _ = std::fs::remove_dir_all(&tmp);

    println!(
        "Done: {converted} converted, {resolved} resolved, {mismatched} mismatched, {failed} failed."
    );
    if saved > 0 {
        println!("Space saved: {}", HumanBytes(saved as u64));
    }
    if stopping() {
        println!("Interrupted; run again to resume where it left off.");
        std::process::exit(130);
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

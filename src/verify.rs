//! Verify that a DNG contains the same raw pixel data as the original RAW.
//!
//! Both files are decoded with LibRaw, a C library completely independent
//! from rawler (the Rust library that produced the DNG). If an independent
//! decoder extracts identical sensor data from both files, the conversion is
//! lossless and it is safe to delete the original.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};

unsafe extern "C" {
    fn dngeat_libraw_open(path: *const c_char, err: *mut c_int) -> *mut c_void;
    fn dngeat_libraw_dims(
        h: *mut c_void,
        raw_width: *mut u16,
        raw_height: *mut u16,
        top_margin: *mut u16,
        left_margin: *mut u16,
        width: *mut u16,
        height: *mut u16,
    );
    fn dngeat_libraw_raw_pixels(h: *mut c_void) -> *const u16;
    fn dngeat_libraw_close(h: *mut c_void);
    fn dngeat_libraw_strerror(code: c_int) -> *const c_char;
    fn dngeat_libraw_version() -> *const c_char;
}

/// Version string of the LibRaw library we are linked against, for display.
pub fn libraw_version() -> String {
    unsafe { CStr::from_ptr(dngeat_libraw_version()) }
        .to_string_lossy()
        .into_owned()
}

/// Decoded sensor frame: full raw dimensions plus active area geometry.
struct Frame {
    raw_width: usize,
    raw_height: usize,
    top: usize,
    left: usize,
    width: usize,
    height: usize,
    /// raw_width * raw_height u16 sensor values
    data: Vec<u16>,
}

struct LibRaw(*mut c_void);

impl Drop for LibRaw {
    fn drop(&mut self) {
        unsafe { dngeat_libraw_close(self.0) }
    }
}

fn decode(path: &Path) -> Result<Frame> {
    let cpath = CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    let mut err: c_int = 0;
    let handle = unsafe { dngeat_libraw_open(cpath.as_ptr(), &mut err) };
    if handle.is_null() {
        let msg = unsafe { CStr::from_ptr(dngeat_libraw_strerror(err)) }.to_string_lossy();
        bail!("libraw cannot decode {}: {msg}", path.display());
    }
    let h = LibRaw(handle);

    let (mut rw, mut rh, mut top, mut left, mut w, mut hgt) = (0u16, 0u16, 0u16, 0u16, 0u16, 0u16);
    unsafe { dngeat_libraw_dims(h.0, &mut rw, &mut rh, &mut top, &mut left, &mut w, &mut hgt) };

    let pixels = unsafe { dngeat_libraw_raw_pixels(h.0) };
    if pixels.is_null() {
        bail!(
            "libraw did not produce 16-bit bayer data for {} (unsupported layout)",
            path.display()
        );
    }
    let n = rw as usize * rh as usize;
    let data = unsafe { std::slice::from_raw_parts(pixels, n) }.to_vec();

    Ok(Frame {
        raw_width: rw as usize,
        raw_height: rh as usize,
        top: top as usize,
        left: left as usize,
        width: w as usize,
        height: hgt as usize,
        data,
    })
}

/// Iterate a rectangular window of a frame row by row.
fn window_rows(
    f: &Frame,
    top: usize,
    left: usize,
    w: usize,
    h: usize,
) -> impl Iterator<Item = &[u16]> {
    (top..top + h).map(move |row| {
        let start = row * f.raw_width + left;
        &f.data[start..start + w]
    })
}

/// Decode both files with LibRaw and compare the sensor data.
///
/// The comparison happens on the RAW's active (visible) window: the DNG may
/// keep or drop masked/calibration border pixels and may declare a different
/// active area (e.g. Sony ARW marks 32 padding columns inactive while the DNG
/// converted from it keeps the full frame active). All that matters is that
/// every pixel the RAW considers visible is bit-identical in the DNG.
pub fn same_sensor_data(raw: &Path, dng: &Path) -> Result<bool> {
    let a = decode(raw).context("decoding original RAW with libraw")?;
    crate::check_stop()?;
    let b = decode(dng).context("decoding produced DNG with libraw")?;

    // Fast path: bit-identical full sensor frames.
    if (a.raw_width, a.raw_height) == (b.raw_width, b.raw_height) && a.data == b.data {
        return Ok(true);
    }

    // Locate the RAW's active window inside the DNG frame.
    let (w, h) = (a.width, a.height);
    let (b_top, b_left) = if (b.raw_width, b.raw_height) == (a.raw_width, a.raw_height) {
        // Same full frame: same coordinates.
        (a.top, a.left)
    } else if (b.raw_width, b.raw_height) == (w, h) {
        // DNG was cropped to exactly the RAW's active area.
        (0, 0)
    } else if (b.width, b.height) == (w, h) {
        // Active areas agree; use the DNG's own margins.
        (b.top, b.left)
    } else {
        log::warn!(
            "geometry mismatch: raw frame {}x{} active {}x{}@{},{} vs dng frame {}x{} active {}x{}@{},{}",
            a.raw_width, a.raw_height, a.width, a.height, a.left, a.top,
            b.raw_width, b.raw_height, b.width, b.height, b.left, b.top,
        );
        return Ok(false);
    };

    Ok(window_rows(&a, a.top, a.left, w, h).eq(window_rows(&b, b_top, b_left, w, h)))
}

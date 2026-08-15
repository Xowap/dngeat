/* Minimal shim over the LibRaw C API.
 *
 * Compiled against the system libraw headers at build time, so struct
 * layouts always match the installed library (unlike pre-generated Rust
 * bindings, which break across libraw versions).
 */

#include <libraw/libraw.h>
#include <stddef.h>

/* Open a raw file and unpack its sensor data. Returns NULL on failure and
 * stores a libraw error code in *err. */
void *dngeat_libraw_open(const char *path, int *err) {
  libraw_data_t *lr = libraw_init(0);
  if (!lr) {
    *err = LIBRAW_UNSUFFICIENT_MEMORY;
    return NULL;
  }
  int rc = libraw_open_file(lr, path);
  if (rc != LIBRAW_SUCCESS) {
    libraw_close(lr);
    *err = rc;
    return NULL;
  }
  rc = libraw_unpack(lr);
  if (rc != LIBRAW_SUCCESS) {
    libraw_close(lr);
    *err = rc;
    return NULL;
  }
  *err = LIBRAW_SUCCESS;
  return lr;
}

/* Full sensor frame dimensions and active area geometry. */
void dngeat_libraw_dims(void *h, unsigned short *raw_width,
                        unsigned short *raw_height, unsigned short *top_margin,
                        unsigned short *left_margin, unsigned short *width,
                        unsigned short *height) {
  libraw_data_t *lr = (libraw_data_t *)h;
  *raw_width = lr->sizes.raw_width;
  *raw_height = lr->sizes.raw_height;
  *top_margin = lr->sizes.top_margin;
  *left_margin = lr->sizes.left_margin;
  *width = lr->sizes.width;
  *height = lr->sizes.height;
}

/* Pointer to the unpacked bayer sensor data (raw_width * raw_height u16), or
 * NULL if the file did not decode to a single-plane 16-bit bayer frame. */
const unsigned short *dngeat_libraw_raw_pixels(void *h) {
  libraw_data_t *lr = (libraw_data_t *)h;
  return lr->rawdata.raw_image;
}

void dngeat_libraw_close(void *h) { libraw_close((libraw_data_t *)h); }

const char *dngeat_libraw_strerror(int code) { return libraw_strerror(code); }

const char *dngeat_libraw_version(void) { return libraw_version(); }

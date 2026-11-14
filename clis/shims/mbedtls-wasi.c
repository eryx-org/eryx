/* WASI alt-hooks for mbedTLS on wasm32-wasip2 (curl TLS).
 *
 * Compiled by the eryx-build C lane with the dep's cflags (which enable these
 * alts) + the mbedTLS include dir, and linked into the curl component. mbedTLS
 * can't get these from WASI itself:
 *   - MBEDTLS_ENTROPY_HARDWARE_ALT  → mbedtls_hardware_poll() via getentropy()
 *   - MBEDTLS_PLATFORM_MS_TIME_ALT  → mbedtls_ms_time() via CLOCK_MONOTONIC
 *     (WASI advertises neither the /dev/urandom nor the POSIX-timer macros
 *      mbedTLS probes for, though getentropy()/clock_gettime() both work).
 */
#include <stddef.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>            /* getentropy */

#include "mbedtls/build_info.h"
#include "mbedtls/platform_time.h"

int mbedtls_hardware_poll(void *data, unsigned char *output,
                          size_t len, size_t *olen) {
    (void) data;
    size_t off = 0;
    while (off < len) {
        size_t chunk = len - off;
        if (chunk > 256) chunk = 256;        /* getentropy() max per call */
        if (getentropy(output + off, chunk) != 0) {
            return -0x003C;                   /* MBEDTLS_ERR_ENTROPY_SOURCE_FAILED */
        }
        off += chunk;
    }
    *olen = len;
    return 0;
}

mbedtls_ms_time_t mbedtls_ms_time(void) {
    struct timespec tv;
    if (clock_gettime(CLOCK_MONOTONIC, &tv) != 0) {
        return (mbedtls_ms_time_t) time(NULL) * 1000;
    }
    return (mbedtls_ms_time_t) tv.tv_sec * 1000 + tv.tv_nsec / 1000000;
}

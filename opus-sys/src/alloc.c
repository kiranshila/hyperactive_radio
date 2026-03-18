/* Static scratch buffer for the opus NONTHREADSAFE_PSEUDOSTACK allocator.
 * Replaces the default opus_alloc_scratch which calls malloc.
 * GLOBAL_STACK_SIZE is 120000 bytes (arch.h). */
#include <stddef.h>

static unsigned char opus_scratch_buf[120000];

void *opus_alloc_scratch(size_t size)
{
    (void)size;
    return opus_scratch_buf;
}

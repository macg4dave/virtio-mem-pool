/* Model tests; these do not emulate the Windows hot-remove implementation. */
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define IN
#define OUT
#define TRUE 1
#define FALSE 0
#define STATUS_SUCCESS 0
#define NT_SUCCESS(status) ((status) == 0)
#define PAGE_SIZE 4096
#define MAXULONG UINT32_MAX
#define MM_ALLOCATE_REQUIRE_CONTIGUOUS_CHUNKS 0x20
#define MM_ALLOCATE_AND_HOT_REMOVE 0x100
#define MmCached 0
#define NonPagedPool 0
#define VIRTIO_MEM_POOL_TAG 0
#ifndef min
#define min(a, b) ((a) < (b) ? (a) : (b))
#endif
#define TRACE_LEVEL_INFORMATION 0
#define TRACE_LEVEL_ERROR 1
#define DBG_PNP 0

typedef int BOOLEAN;
typedef int NTSTATUS;
typedef uint32_t ULONG;
typedef ULONG *PULONG;
typedef int64_t LONGLONG;
typedef uint64_t ULONGLONG;
typedef uint16_t __virtio16;
typedef size_t SIZE_T;
typedef struct { LONGLONG QuadPart; } PHYSICAL_ADDRESS;
typedef struct {
    PHYSICAL_ADDRESS BaseAddress, NumberOfBytes;
} PHYSICAL_MEMORY_RANGE, *PPHYSICAL_MEMORY_RANGE;
typedef struct { ULONG SizeOfBitMap; uint64_t bits; } RTL_BITMAP, *PRTL_BITMAP;
typedef struct { RTL_BITMAP memoryBitmapHandle; } DEVICE_CONTEXT, *PDEVICE_CONTEXT;
typedef PDEVICE_CONTEXT WDFOBJECT;
typedef struct {
    uint64_t addr, region_size, block_size, plugged_size, requested_size;
} virtio_mem_config;
typedef struct { ULONG count; PHYSICAL_MEMORY_RANGE ranges[64]; } MDL, *PMDL;

static virtio_mem_config config;
static DEVICE_CONTEXT device;
static MDL mdl;
static uint64_t removable, unplugged, restored, total_requested;
static PHYSICAL_ADDRESS observed_low, observed_high;
static ULONG allocation_calls;
static BOOLEAN reject_unplug;

static void TraceEvents(int level, int category, const char *format, ...)
{
    (void)level; (void)category; (void)format;
}

static PDEVICE_CONTEXT GetDeviceContext(WDFOBJECT object) { return object; }
/* Also used when testing the unpatched source. */
BOOLEAN RtlCheckBit(PRTL_BITMAP bitmap, ULONG index)
{
    return (bitmap->bits >> index) & 1;
}

static PMDL MmAllocateNodePagesForMdlEx(PHYSICAL_ADDRESS low, PHYSICAL_ADDRESS high,
                                       PHYSICAL_ADDRESS skip, SIZE_T total,
                                       int cache, int node, ULONG flags)
{
    assert(cache == MmCached && node == 0);
    assert(flags == (MM_ALLOCATE_REQUIRE_CONTIGUOUS_CHUNKS | MM_ALLOCATE_AND_HOT_REMOVE));
    assert((uint64_t)skip.QuadPart == config.block_size);
    assert(total % config.block_size == 0);
    allocation_calls++;
    observed_low = low;
    observed_high = high;
    total_requested = total;
    memset(&mdl, 0, sizeof(mdl));
    uint64_t remaining = total;
    for (ULONG i = 0; i < device.memoryBitmapHandle.SizeOfBitMap; i++)
    {
        uint64_t bit = UINT64_C(1) << i;
        uint64_t start = config.addr + i * config.block_size;
        if ((device.memoryBitmapHandle.bits & removable & bit) &&
            start >= (uint64_t)low.QuadPart &&
            start + config.block_size - 1 <= (uint64_t)high.QuadPart &&
            remaining >= config.block_size)
        {
            PHYSICAL_MEMORY_RANGE *range = &mdl.ranges[mdl.count++];
            range->BaseAddress.QuadPart = (LONGLONG)start;
            range->NumberOfBytes.QuadPart = (LONGLONG)config.block_size;
            remaining -= config.block_size;
        }
    }
    return mdl.count ? &mdl : NULL;
}

static ULONG GetMemoryRangesCountFromMdl(PMDL value) { return value->count; }
static void GetMemoryRangesFromMdl(PMDL value, PHYSICAL_MEMORY_RANGE *ranges)
{
    memcpy(ranges, value->ranges, value->count * sizeof(*ranges));
}
static void *ExAllocatePoolZero(int pool, size_t bytes, int tag)
{
    (void)pool; (void)tag;
    return calloc(1, bytes);
}
static void ExFreePoolWithTag(void *memory, int tag) { (void)tag; free(memory); }
static BOOLEAN SendUnPlugRequest(WDFOBJECT object, LONGLONG address, __virtio16 blocks)
{
    (void)object;
    ULONG index = (ULONG)(((uint64_t)address - config.addr) / config.block_size);
    assert(index + blocks <= device.memoryBitmapHandle.SizeOfBitMap);
    if (reject_unplug) return FALSE;
    for (ULONG i = index; i < index + blocks; i++) unplugged |= UINT64_C(1) << i;
    return TRUE;
}
static void DeallocateMemoryRangeInMemoryBitmap(LONGLONG base, PHYSICAL_MEMORY_RANGE *range,
                                                RTL_BITMAP *bitmap, ULONG block)
{
    ULONG index = (ULONG)((range->BaseAddress.QuadPart - base) / block);
    ULONG count = (ULONG)(range->NumberOfBytes.QuadPart / block);
    for (ULONG i = index; i < index + count; i++) bitmap->bits &= ~(UINT64_C(1) << i);
}
static NTSTATUS MmAddPhysicalMemory(PHYSICAL_ADDRESS *base, PHYSICAL_ADDRESS *bytes)
{
    restored += (uint64_t)bytes->QuadPart;
    assert((uint64_t)base->QuadPart >= config.addr);
    return STATUS_SUCCESS;
}

/* REMOVAL_FUNCTIONS */

static void setup(uint64_t bits, uint64_t candidates, uint64_t block, ULONG count, ULONG reduction)
{
    config = (virtio_mem_config){.addr = UINT64_C(0x280000000),
        .region_size = count * block, .block_size = block};
    for (ULONG i = 0; i < count; i++) config.plugged_size += ((bits >> i) & 1) * block;
    config.requested_size = config.plugged_size - reduction * block;
    device.memoryBitmapHandle = (RTL_BITMAP){count, bits};
    removable = candidates;
    unplugged = restored = total_requested = 0;
    allocation_calls = 0;
    reject_unplug = FALSE;
}

static void run(void)
{
    assert(VirtioMemRemovePhysicalMemory(&device, &config));
    assert(allocation_calls == 1);
    assert((uint64_t)observed_low.QuadPart == config.addr);
    assert((uint64_t)observed_high.QuadPart == config.addr + config.region_size - 1);
    assert(total_requested <= config.plugged_size - config.requested_size);
    assert(total_requested <= UINT64_C(0x100000000) - PAGE_SIZE);
}

int main(void)
{
    const uint64_t mib = UINT64_C(1024) * 1024;
    setup(0xff, 0x80, 2 * mib, 8, 1);
    run();
    assert(unplugged == 0x80 && device.memoryBitmapHandle.bits == 0x7f);
    puts("PASS: blocked first block does not exclude removable last block");

    setup(0xc3, 0x03, 8 * mib, 8, 1);
    run();
    assert(unplugged == 0x01 && device.memoryBitmapHandle.bits == 0xc2);
    puts("PASS: fragmented bitmap keeps lower removable blocks eligible");

    setup(0x55, 0x55, 64 * mib, 8, 3);
    run();
    assert(unplugged == 0x15 && device.memoryBitmapHandle.bits == 0x40);
    puts("PASS: disjoint chunks satisfy one bounded reduction");

    setup(0xff, 0x80, 2 * mib, 8, 4);
    run();
    assert(unplugged == 0x80);
    puts("PASS: partial result updates only returned blocks");

    setup(0xff, 0, 2 * mib, 8, 1);
    run();
    assert(unplugged == 0 && device.memoryBitmapHandle.bits == 0xff);
    puts("PASS: NULL allocation leaves plugged bitmap unchanged");

    setup(0xff, 0x80, 2 * mib, 8, 1);
    reject_unplug = TRUE;
    run();
    assert(unplugged == 0 && restored == 2 * mib && device.memoryBitmapHandle.bits == 0xff);
    puts("PASS: rejected UNPLUG re-adds returned memory");

    setup(0xff, 0, 1024 * mib, 8, 8);
    run();
    assert(total_requested == 3 * 1024 * mib);
    puts("PASS: large reduction is capped to a whole-block MDL allocation");

    setup(0xff, 0, PAGE_SIZE, 8, 0);
    assert(VirtioMemRemovePhysicalMemory(&device, &config));
    assert(allocation_calls == 0);
    config.requested_size += PAGE_SIZE;
    assert(VirtioMemRemovePhysicalMemory(&device, &config));
    assert(allocation_calls == 0);
    puts("PASS: converged and growth targets do not allocate removal pages");
    return 0;
}

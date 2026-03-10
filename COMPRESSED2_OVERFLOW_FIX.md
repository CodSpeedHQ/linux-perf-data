# PERF_RECORD_COMPRESSED2 header.size Overflow — Fix Progress

## Bug Summary

perf's `builtin-record.c:record__pushfn()` computes:
```c
event->header.size = PERF_ALIGN(compressed, sizeof(u64));  // u16 field
padding = event->header.size - compressed;
```
When the zstd-compressed output is large enough, the aligned value overflows u16:
- **Single-record overflow**: compressed ∈ [65529,65535] → aligned = 65536 → u16 wraps to 0
- **Multi-record overflow**: compressed > 65535 → aligned wraps modulo 65536 (e.g. 68264 → 2728)

The padding write also fails silently (underflow), so only the raw bytes are written with no alignment padding.

Reference: `tools/perf/builtin-record.c` line ~672, introduced in kernel 6.16 via commit `208c0e168344`.

## Test Files

| File | Overflow Type | Location |
|------|--------------|----------|
| `~/repro-perf-event-size/out.pipedata` | Single-record (`data_size=65519`, `header.size=0`) | 1 corrupted record |
| `~/cod-2314/profile.pPMUwlf7Pu.out/perf.pipedata` | Both types | `data_size=65519, header.size=0` at offset `0x8b14cd00` AND `data_size=68246, header.size=2728` at offset `0x8cbc8a57` |

Run with: `cargo run --release --example perfpipeinfo < <file>`

## Current Fix State

### What Works
- **Overflow detection**: in `src/file_reader.rs`, all COMPRESSED2 records are checked by comparing
  `expected_aligned = PERF_ALIGN(8 + 8 + data_size, 8)` against `header.size`. If `expected_aligned > size`, overflow is detected.
- **Single-record overflow recovery** (`data_size <= 65519`): reads the correct number of bytes using `data_size`,
  decompresses successfully. Tested on `out.pipedata`.
- **Multi-record overflow** (`data_size > 65519`): currently **skipped** (samples in that record are lost, but parsing continues).
- **Partial record discard**: `ZstdDecompressor::discard_partial_record()` added to `src/decompression.rs`
  to clear stale buffered data when a corrupted record is encountered.

### What's Broken — Current Failure

The real-world file (`cod-2314`) still crashes with `InvalidPerfEventSize` after the two overflow records are handled.

**Last debug output:**
```
[ERR] InvalidPerfEventSize in decompressed data: type=4097476414, size=0, offset_in_decomp=0
```

This means a **decompressed** chunk starts with garbage data (type=4097476414 is clearly not a valid perf record type).

### Root Cause of Remaining Failure

The `discard_partial_record()` call is placed correctly, but the problem is likely:

1. **The partial record from the chunk BEFORE the overflow is still being prepended.**
   The sequence is: valid COMPRESSED2 chunk N decompresses and saves a partial record →
   overflow COMPRESSED2 chunk N+1 is detected → we call `discard_partial_record()` → we decompress
   chunk N+1 successfully. But the decompressed output of chunk N+1 starts mid-record (it was
   supposed to complete the partial from chunk N). So the first bytes of the decompression are
   the tail of an event, not a valid header.

2. **Fix approach**: After discarding the partial and decompressing the overflow record,
   we need to **skip forward** in the decompressed data to find the first valid record boundary.
   This could be done by scanning for a valid `perf_event_header` (valid type, reasonable size)
   in `decompress_and_process_compressed2`.

3. Alternatively, after decompressing the overflow record, we could scan the decompressed
   bytes for the first record whose `type` is a known perf record type and whose `size` is
   sane (>= 8, <= decompressed.len()).

### Files Modified

| File | Changes |
|------|---------|
| `src/file_reader.rs` | COMPRESSED2 overflow detection and recovery in `next_record_inner()` (~line 489). Moved COMPRESSED2 handling before the generic `size < 8` check. |
| `src/decompression.rs` | Added `discard_partial_record()` method to `ZstdDecompressor`. |

### Temporary Debug Lines (Remove Before Merging)

- `src/file_reader.rs:597-600` — eprintln for InvalidPerfEventSize in main loop
- `src/file_reader.rs:730-733` — eprintln for InvalidPerfEventSize in decompressed data

### Next Steps

1. **Fix the "garbage at start of decompressed data" issue**: when an overflow record is
   decompressed after a partial discard, the first N bytes of the decompressed output are
   the tail of the previous event. Need to skip past them to find the first valid record header.

2. **Implement multi-record overflow recovery** (optional, for `data_size > 65519`):
   the corrupted record contains multiple concatenated COMPRESSED2 sub-records. The first
   sub-record's header is corrupted but subsequent ones have valid headers written by
   `process_comp_header()`. Need to scan the payload for sub-record boundaries.
   Note: sub-records 2+ have `data_size` field **uninitialized** — compute zstd payload
   size from `header.size` instead.

3. **Add regression test**: copy a truncated version of `out.pipedata` (just the region around
   the corrupted record) as a test fixture, or generate one in CI.

4. **Remove debug eprintln lines** before merging.

### Relevant Upstream Code

- Writer bug: `~/projects/linux/tools/perf/builtin-record.c` → `record__pushfn()` line ~672
- Reader (C): `~/projects/linux/tools/perf/util/tool.c` → `perf_session__process_compressed_event()` — note that this code uses `event->pack2.data_size` (not `header.size`) for COMPRESSED2, but the pipe reader in `session.c:2080` rejects `header.size=0` before reaching the decompressor.
- Repro: `~/repro-perf-event-size/` — generates corrupted pipe-mode data with `bench.c` workload

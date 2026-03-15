# Read Throughput Notes

These notes summarize the read-throughput investigation as of March 15, 2026.
They are based on the `file_read` benchmark in
[`benches/file_read.rs`](../benches/file_read.rs), which mounts each transport
read-only, performs one timed read pass, and unmounts before the next sample.

## Results Summary

| Configuration | Throughput | vs Loop |
|---------------|-----------|---------|
| loop (native) | 4.56 GiB/s | 1.00x |
| tcmu (before, default queue) | 2.30 GiB/s | 0.50x |
| tcmu (ra=8192k, scheduler=none, max_sectors_kb=16384) | **3.87 GiB/s** | **0.85x** |

Overall improvement: **+68%**, from 50% to 85% of native loop speed.

## Optimization History

### Phase 1: Eliminate crate bugs (2.30 GiB/s)

1. **Removed accidental data_out copy for READs.** The UIO event loop copied
   READ command I/O vectors into a temporary `data_out` buffer even though READ
   commands never carry initiator payload. Fixing this was the dominant win,
   moving from ~670 MiB/s to ~2.20 GiB/s.

2. **Added zero-copy scatter/gather read path.** `BlockDevice::read_exact_at`
   and `read_exact_vectored_at` plus `TcmuDevice::execute_into` let READ
   commands write directly into the TCMU IOV buffers (kernel bio pages),
   eliminating an intermediate `Vec<u8>` allocation and scatter copy. Small
   additional gain to ~2.30 GiB/s.

### Phase 2: Block device queue tuning (3.15 GiB/s)

3. **Switched I/O scheduler to `none`.** The default `mq-deadline` scheduler
   adds overhead for what is already a sequential workload. Setting
   `scheduler=none` via sysfs improved throughput ~10-15%.

4. **Raised `max_sectors_kb` to 16384.** The kernel default of 1280 KiB
   artificially caps request size. The SCSI host allows up to 32767 KiB.

5. **Raised `read_ahead_kb` to 8192.** The kernel default of 128 KiB causes
   many small READ commands. Higher readahead yields 1-2 MiB requests (capped
   by the kernel readahead algorithm internals). Diminishing returns above
   8192 KiB — the kernel caps individual readahead I/Os at ~2 MiB regardless
   of the setting.

### Phase 3: Reduce per-command overhead (3.87 GiB/s)

6. **Replaced `preadv` with mmap + memcpy.** The benchmark's file-backed
   `BlockDevice` now uses `mmap(MAP_PRIVATE | MAP_POPULATE)` with
   `MADV_SEQUENTIAL`. This eliminates the per-command `preadv` syscall,
   replacing it with a user-space `memcpy` from mapped pages. Small but
   measurable gain (~2-3%).

7. **Relaxed `SeqCst` fence to `Release`.** The `cmd_tail` volatile write only
   needs prior stores (response data, sense buffer) to be visible before the
   tail update. `Release` ordering suffices; `SeqCst` was unnecessarily strong.

8. **Added `preadv_exact` helper.** For non-benchmark file-backed `BlockDevice`
   implementations (loopback example, integration test), `preadv_exact` in
   `lib.rs` collapses N per-iovec `pread64` syscalls into a single `preadv`.

### Approaches that did NOT help

- **Per-command UIO notification** instead of per-batch: the kernel already
  reads `cmd_tail` directly without waiting for a UIO interrupt. Extra `write`
  syscalls add overhead without improving pipeline utilization.
- **`madvise(MADV_HUGEPAGE)`**: no measurable effect on the mmap'd backing
  file.
- **Readahead above 8192 KiB**: the kernel's readahead algorithm caps
  individual I/O submissions at ~2 MiB regardless of `read_ahead_kb`.

## Remaining Gap Analysis

The ~15% gap to native loop is inherent to the TCMU user-space architecture:

- **User-space round-trip**: each SCSI command requires a kernel→user→kernel
  context switch pair (UIO poll return + UIO notification write).
- **Data copy**: `memcpy` from mmap'd backing file into TCMU shared memory
  (bio pages). `perf` profiling shows `_copy_to_iter` / `memcpy` dominating
  CPU time. The loop device avoids this by remapping bios directly to the
  backing file in kernel context.
- **Ring protocol overhead**: entry parsing, volatile tail writes, and release
  fences per command. Small but non-zero at ~4000 commands per 4 GiB pass.

`bpftrace` tracing confirmed most READ commands are 1-2 MiB at `ra_8192k`,
with ~4000 commands per 4 GiB pass. At 3.87 GiB/s, per-command overhead is
~12 µs above what the loop device achieves.

## Further Directions

1. **io_uring for backend I/O.** Batching multiple `preadv` submissions into a
   single `io_uring_enter` syscall could reduce per-command overhead. This is
   a significant architectural change.

2. **Parallel command dispatch.** A thread pool for READ I/O could overlap
   backend reads. Requires careful ring tail management.

3. **Larger `hw_max_sectors`.** Now configurable via
   `TcmuTargetBuilder::hw_max_sectors()`. Didn't help on this local loopback
   workload (kernel readahead caps at ~2 MiB I/Os), but may improve
   throughput on iSCSI or other fabrics.

4. **Raw block benchmark.** Add a raw sequential read benchmark (no ext4)
   to isolate transport overhead from filesystem caching behavior.

## Tuning Reference

### TCMU configfs (before device enable)

`hw_max_sectors` controls the maximum SCSI transfer size in 512-byte sectors.
The kernel default is 128 (64 KiB). It must be set via the configfs `control`
file before the device is enabled — the `attrib/hw_max_sectors` file is
read-only.

```rust
TcmuTarget::builder()
    .name("mydev")
    .size_bytes(size)
    .hw_max_sectors(8192) // 4 MiB
    .with_loopback()
    .build()?;
```

On this workload, raising `hw_max_sectors` did not improve throughput because
the kernel readahead algorithm caps individual I/O submissions at ~2 MiB
regardless. It may matter more on iSCSI or other fabrics where the SCSI-level
limit is the effective cap.

### Block device sysfs (after device appears)

```sh
echo none > /sys/block/sdX/queue/scheduler
echo 16384 > /sys/block/sdX/queue/max_sectors_kb
echo 8192 > /sys/block/sdX/queue/read_ahead_kb
```

```rust
tcmu::target::set_scheduler(&block_dev, "none")?;
tcmu::target::set_max_sectors_kb(&block_dev, 16384)?;
tcmu::target::set_read_ahead_kb(&block_dev, 8192)?;
```

## Validation Commands

```sh
make test
make clippy
make bench
make bench-large   # large_file workload only
```

# Device Lifecycle and Recovery

These notes document TCMU device creation, teardown, and recovery from
handler crashes, based on investigation of the Linux 6.12 kernel.

## Creation Path

1. `mkdir` the configfs device directory
2. Write `dev_size` to `attrib/dev_size`
3. Optionally write `hw_max_sectors=N` and `cmd_time_out=N` to `control`
4. Write `1` to `enable` — kernel creates the UIO device
5. Open the UIO fd and start the event loop
6. Create loopback fabric (nexus, LUN symlink, rescan) — the kernel sends
   SCSI INQUIRY immediately, so the event loop must be running first

## Teardown Path (normal)

1. Stop the event loop (set stop flag, wait for thread to exit)
2. `unlink` the LUN symlink in configfs (NOT `rm -f` or `rmdir`)
3. `rmdir` the `lun_0`, `tpgt_1`, and WWN directories
4. `rmdir` the TCMU device directory

## What Happens When a Handler Crashes

When the user-space TCMU handler dies (UIO fd closed):

- In-flight SCSI commands remain on the ring, holding `lun_ref` references
- The kernel's `cmd_time_out` timer (default 30s) fires and completes each
  expired command with `SAM_STAT_CHECK_CONDITION`
- Once all commands complete, `lun_ref` drops to zero and LUN teardown can
  proceed

If `cmd_time_out` is set to 0, commands **never time out** and the system
is permanently stuck — cleanup blocks forever in `transport_clear_lun_ref`.

## Recovery Without Reboot

If devices are stuck after a handler crash:

```sh
# 1. Force-complete all in-flight commands (value 2 = hard failure)
echo 2 > /sys/kernel/config/target/core/user_0/<device>/action/reset_ring

# 2. Delete SCSI devices to remove block device references
echo 1 > /sys/class/scsi_device/<H:C:T:L>/device/delete

# 3. Remove LUN symlinks (must use unlink, not rm or rmdir)
unlink /sys/kernel/config/target/loopback/<wwn>/tpgt_1/lun/lun_0/<device>

# 4. Remove configfs directories in order
rmdir /sys/kernel/config/target/loopback/<wwn>/tpgt_1/lun/lun_0
rmdir /sys/kernel/config/target/loopback/<wwn>/tpgt_1
rmdir /sys/kernel/config/target/loopback/<wwn>
rmdir /sys/kernel/config/target/core/user_0/<device>
```

If a kernel thread is already blocked in `target_fabric_port_unlink`
(waiting on `lun_ref` to drain), `reset_ring` will release those refs and
unblock it. However, if the blocked thread was killed (e.g. the process was
SIGKILLed), it leaves a zombie kernel task in D state that persists until
reboot.

## Prevention

### Set `cmd_time_out` before export

The `cmd_time_out` attribute **cannot be changed after LUN exports exist**.
It must be written to the `control` file before creating any loopback LUNs:

```
echo "cmd_time_out=10" > /sys/kernel/config/target/core/user_0/<device>/control
```

Or via the builder:

```rust
TcmuTarget::builder()
    .name("mydev")
    .size_bytes(size)
    .cmd_time_out(Duration::from_secs(10))
    .with_loopback()
    .build()?;
```

A short timeout (5-10s) ensures that handler crashes are recoverable within
seconds rather than the default 30s.

### Use `reset_ring` in Drop

If the event loop thread has died or is unresponsive, `Drop` should write to
`action/reset_ring` before attempting LUN unlink. This force-completes all
in-flight commands and allows cleanup to proceed.

### Graceful shutdown

Always call `target.stop()` and join the event loop thread before dropping.
If the thread panicked or the process is shutting down, `reset_ring` is the
fallback.

## Benchmark Findings

On a clean system with the `device_lifecycle` benchmark:

| Operation | Latency |
|-----------|---------|
| create + destroy (no loopback) | ~10ms |
| create + destroy (with loopback) | TBD (re-run on clean system) |

The loopback path is dominated by:
- 50ms initial sleep in `LoopbackStarter::run()` (lets event loop start)
- 50ms polling interval in `wait_for_tcm_loop_host()`
- Kernel SCSI host discovery latency after `rescan`
- `rescan_tcm_loop_hosts()` rescans ALL tcm_loop hosts, not just the new one

## Key Kernel Files

- `drivers/target/target_core_user.c` — TCMU backstore (timeout, reset_ring)
- `drivers/target/target_core_tpg.c` — `transport_clear_lun_ref` (blocking wait)
- `drivers/target/target_core_fabric_configfs.c` — `target_fabric_port_unlink`

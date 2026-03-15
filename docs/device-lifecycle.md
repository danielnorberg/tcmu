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

### `cmd_time_out`

The library defaults `cmd_time_out` to 10 seconds (the kernel default is 30s).
This ensures handler crashes are recoverable within 10s. Override with
`.cmd_time_out(Duration::from_secs(N))` if needed. Setting to zero disables
timeouts — **do not do this in production**.

### Graceful shutdown

Call `target.stop()` before dropping. This tears down the loopback fabric
while the event loop is still running (so SCSI teardown commands are serviced),
then signals the event loop to exit. `Drop` handles `reset_ring` and configfs
cleanup as a fallback if `stop()` was not called (e.g. handler crash).

## Benchmark Findings

On a clean system with the `device_lifecycle` benchmark:

| Operation | Latency |
|-----------|---------|
| create + destroy (no loopback) | ~14ms |
| create + destroy (with loopback, sequential) | ~170ms typical |
| concurrent create 4 devices | 75ms wall (19ms/device) |
| concurrent create 8 devices | 93ms wall (12ms/device) |

Occasional outliers (~30s) occur when kernel SCSI teardown waits for
command timeouts. Setting `cmd_time_out` to a short value (5s) limits
the worst case.

The loopback path is dominated by:
- 50ms initial sleep in `LoopbackStarter::run()` (lets event loop start)
- 50ms polling interval in `wait_for_tcm_loop_host()`
- Kernel SCSI host discovery latency after `rescan`
- `rescan_tcm_loop_hosts()` rescans ALL tcm_loop hosts, not just the new one

## Key Kernel Files

- `drivers/target/target_core_user.c` — TCMU backstore (timeout, reset_ring)
- `drivers/target/target_core_tpg.c` — `transport_clear_lun_ref` (blocking wait)
- `drivers/target/target_core_fabric_configfs.c` — `target_fabric_port_unlink`

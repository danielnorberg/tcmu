# tcmu

A generic SCSI/TCMU command processor for implementing read-only user-space block devices in Rust.

## Overview

[TCMU](https://www.kernel.org/doc/html/latest/driver-api/target/tcmu-design.html) (Target Core Module Userspace) is a Linux kernel facility that lets user-space processes handle SCSI commands for block devices. This crate handles the SCSI command layer — CDB parsing, response encoding, sense data, VPD pages — so you only need to implement two things:

1. A `BlockDevice` trait for your backing store
2. A `TcmuDeviceConfig` describing how the device identifies itself to the SCSI initiator

The resulting `TcmuDevice` processes raw CDB bytes and returns a `TcmuResponse` (status + data + sense). Wiring that into the actual TCMU kernel interface is left to the caller — or you can use the optional `linux-target` feature (see below) to get that for free.

## Supported SCSI commands

| Command                    | Opcode    | Behaviour                             |
|----------------------------|-----------|---------------------------------------|
| TEST UNIT READY            | 0x00      | Always succeeds                       |
| REQUEST SENSE              | 0x03      | Returns NO SENSE                      |
| INQUIRY                    | 0x12      | Standard + VPD pages 0x00/0x80/0x83   |
| READ(6)                    | 0x08      | Delegates to `BlockDevice::read_at`   |
| READ(10)                   | 0x28      | Delegates to `BlockDevice::read_at`   |
| READ(12)                   | 0xa8      | Delegates to `BlockDevice::read_at`   |
| READ(16)                   | 0x88      | Delegates to `BlockDevice::read_at`   |
| READ CAPACITY(10)          | 0x25      | Derived from `BlockDevice::size_bytes`|
| SERVICE ACTION IN(16)      | 0x9e      | READ CAPACITY(16) service action      |
| MODE SENSE(6)/(10)         | 0x1a/0x5a | Caching page (read-only bit set)      |
| SYNCHRONIZE CACHE(10)/(16) | 0x35/0x91 | No-op (always succeeds)               |
| WRITE(6/10/12/16)          | —         | WRITE PROTECTED check condition       |
| WRITE SAME(10/16)          | —         | WRITE PROTECTED check condition       |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
tcmu = { git = "https://github.com/danielnorberg/tcmu" }
```

### Implement `BlockDevice`

```rust
use tcmu::BlockDevice;

struct MyDevice {
    data: Vec<u8>,
}

impl BlockDevice for MyDevice {
    fn size_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>> {
        let start = offset as usize;
        Ok(self.data[start..start + len].to_vec())
    }

    fn id_bytes(&self) -> Vec<u8> {
        // Any bytes that uniquely identify this device instance.
        // They are hex-encoded to form the SCSI serial number.
        b"my-device-v1".to_vec()
    }
}
```

### Wrap it in `TcmuDevice`

```rust
use tcmu::{TcmuDevice, TcmuDeviceConfig};

let device = TcmuDevice::new(
    MyDevice { data: vec![0u8; 4096] },
    TcmuDeviceConfig {
        vendor_id:        *b"MYVENDOR",
        product_id:       *b"MYPRODUCT       ",
        product_revision: *b"0001",
        device_id_prefix: "my-device".to_string(),
    },
);
```

### Handle CDBs

Call `execute` with the raw CDB bytes received from the TCMU interface:

```rust
// Example: READ(10) for LBA 0, 1 block
let cdb: [u8; 10] = [0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0];
let response = device.execute(&cdb, &[]);

assert_eq!(response.status, 0x00); // SAM_STAT_GOOD
// response.data contains the 512-byte block
// response.sense is empty on success
```

On error, `status` is `0x02` (CHECK CONDITION) and `sense` contains fixed-format sense data.

## Optional: Linux target management (`linux-target` feature)

Enable the `linux-target` feature to get `tcmu::target::TcmuTarget`, which handles the full kernel integration automatically:

- Creates and tears down the TCMU configfs entries
- Discovers the resulting `/dev/uioN` device
- Optionally sets up a `tcm_loop` fabric so a `/dev/sdX` block device appears
- Runs the UIO ring-buffer event loop

```toml
[dependencies]
tcmu = { git = "https://github.com/danielnorberg/tcmu", features = ["linux-target"] }
```

```rust
use tcmu::target::TcmuTarget;

let target = TcmuTarget::builder()
    .name("mydev")
    .size_bytes(64 << 20)
    .with_loopback()   // also creates a tcm_loop LUN → /dev/sdX
    .build()?;

eprintln!("UIO device: {}", target.uio_path().display());

// Blocks until error or signal; cleans up configfs on drop.
target.run(&device)?;
```

Without the feature, the crate has no Linux-specific dependencies and compiles on any platform.

## Examples

See the [`examples/`](examples/) directory:

- [`ram_disk.rs`](examples/ram_disk.rs) — an in-memory block device that exercises the SCSI command layer without any kernel involvement. No feature flags required.

- [`loopback.rs`](examples/loopback.rs) — serves a filesystem image file as a mountable kernel block device using `TcmuTarget`. Requires the `linux-target` feature and root on Linux.
  ```sh
  sudo cargo run --example loopback --features linux-target -- /tmp/ext4.img
  ```

## Benchmarks

The crate includes a Criterion benchmark that measures file-read performance
through a TCMU loopback block device and compares it with a normal read-only
loop mount of the same ext4 image.

It benchmarks two workloads:

- Reading a 4 GiB large file (`large_file/{tcmu,loop}`)
- Reading many small files (`small_files/{tcmu,loop}`)

Run it on Linux as root with the `linux-target` feature enabled:

```sh
sudo cargo bench --features linux-target --bench file_read -- --noplot
```

The benchmark prepares one ext4 image, populates it once, keeps the backing
image warm in the host cache, and then measures single-pass read throughput by
mounting each transport read-only, reading the workload once, and unmounting
before the next sample.

Current findings and follow-up directions for large-file read throughput are
tracked in [`docs/read-throughput.md`](docs/read-throughput.md).

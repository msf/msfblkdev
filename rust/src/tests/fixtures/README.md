# Version-1 checkpoint fixture

`v1-empty-roots.bin` contains the first 8,192 bytes of an image produced by the
version-1 public formatter at commit `217719f`. The logical volume has one 4 KiB
block; the backing has 32 blocks. The remaining image bytes are zero.

Captured on 2026-09-18 with:

```sh
cargo run --manifest-path rust/Cargo.toml --bin block-storage-ublk -- \
  format <new-temporary-file> --size-bytes 4096 --prealloc-bytes 131072
```

SHA-256: `467a7d020069638ae65f7b611c51c432e90a0fa4ef81aa78bd9ed1ba06912518`.
Keep this fixture unchanged when the public formatter switches to version 3.

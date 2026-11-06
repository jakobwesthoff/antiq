# Flushing an extracted tree

Research on what it costs to make a freshly extracted folder durable before
anything that replaced it is deleted. It records measurements and sources,
not decisions.

## Machine and method

All numbers come from one MacBook Pro (Apple M4 Pro, arm64, macOS 27.0.1)
on its internal solid-state APFS volume (`diskutil info /`: File System
Personality APFS, Device Location Internal, Solid State Yes), from a release
build of a throwaway crate that is not committed. The crate uses `std::fs` to
write the tree, with xorshift64 data so the payload is not compressible, and
`libc` for `fsync` and `fcntl(F_FULLFSYNC)`. It does not use antiq's
extraction. Each run is one process that writes the tree, flushes it, prints
its times and deletes the tree. Every figure is a single run, as planned, so
the numbers carry run-to-run noise that was not measured.
In total about 0.9 GB was written.

Trees:

| Tree | Shape |
|------|-------|
| T1 | 48 211 files of 4 KiB in 200 directories (about 200 MB) |
| T2 | 1 000 files of 4 KiB in 10 directories |
| T3 | one file of 100 MiB |

Strategies:

- **control**: write and close, no flush.
- **A**: `File::sync_all` (which is `F_FULLFSYNC` on macOS) on every file
  before it is closed, then on every directory opened read-only, the tree
  root, and the parent of the tree root.
- **B**: plain `fsync(2)` on every file before it is closed and on every
  directory and the tree root, then one `F_FULLFSYNC` on the first file
  written.

"Total" is wall-clock time from the first create to the last flush. "Flush"
is the time spent in flush calls (per file, plus the directory and final
flushes). "Write" is total minus flush. Removing the tree afterwards is not
timed.

## Results

Seconds.

| Tree | Strategy | Total | Flush | Write |
|------|----------|------:|------:|------:|
| T1 | control | 2.453 | 0.001 | 2.452 |
| T1 | A | 225.432 | 212.249 | 13.184 |
| T1 | B | 3.140 | 1.045 | 2.095 |
| T2 | control | 0.058 | 0.000 | 0.058 |
| T2 | A | 4.761 | 4.451 | 0.309 |
| T2 | B | 0.067 | 0.026 | 0.042 |
| T3 | control | 0.052 | 0.000 | 0.052 |
| T3 | A | 0.079 | 0.034 | 0.044 |
| T3 | B | 0.077 | 0.029 | 0.048 |

Ratios and differences against the control, on total time:

| Tree | A / control | A - control | B / control | B - control |
|------|------------:|------------:|------------:|------------:|
| T1 | 91.9 | 223.0 s | 1.28 | 0.69 s |
| T2 | 82.1 | 4.70 s | 1.16 | 0.01 s |
| T3 | 1.5 | 0.03 s | 1.5 | 0.03 s |

Observations:

- A costs about 4.4 ms per file on T1 (212.2 s over 48 211 files) and about
  4.5 ms per file on T2 (4.45 s over 1 000 files). Its cost follows the file
  count, not the byte count: T3 with one file of 100 MiB flushes in 0.034 s.
- B costs 1.0 s on T1 and 0.03 s on T2 and T3. Plain `fsync` per file is
  cheap on APFS here, and the single `F_FULLFSYNC` at the end took part of
  that time.
- The write phase of A on T1 (13.2 s) is longer than the control's (2.5 s).
- B's total on T1 is lower in its write phase (2.1 s) than the control's
  (2.5 s). That is noise between single runs.

## Sources for the flush calls

`fcntl(2)` on macOS 27.0.1 (`man fcntl`), on `F_FULLFSYNC`:

> Does the same thing as fsync(2) then asks the drive to flush all buffered
> data to the permanent storage device (arg is ignored). As this drains the
> entire queue of the device and acts as a barrier, data that had been
> fsync'd on the same device before is guaranteed to be persisted when this
> call returns. This is currently implemented on HFS, MS-DOS (FAT),
> Universal Disk Format (UDF) and APFS file systems. The operation may take
> quite a while to complete. Certain FireWire drives have also been known to
> ignore the request to flush their buffered data.

`syncfs(2)` on Linux (man7.org, not measured here): since Linux 5.8,
`syncfs` reports writeback errors, and only those that happened since the
file descriptor passed to it was opened. On Linux `File::sync_all` is
`fsync`, so strategies A and B are the same call there. This note has no
Linux measurement.

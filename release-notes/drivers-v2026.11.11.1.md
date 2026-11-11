## Drivers

ClickHouse 0.1.2. Update it in **Driver Manager**, then reconnect.

## Fixes

- Results from a ClickHouse server that answers with LZ4-compressed data (its default, when a server profile or proxy keeps it) are read instead of failing with "lz4 IPC decompression requires the lz4 feature".
- A reply the driver cannot decode is reported as a failure, not as an interruption to run again.

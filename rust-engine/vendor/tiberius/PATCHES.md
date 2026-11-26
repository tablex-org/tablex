# Vendored tiberius 0.13.0

An unmodified copy of `tiberius` 0.13.0 from crates.io (MIT OR Apache-2.0, licences in this
directory) minus its tests, examples, Docker files and docs, with these changes (the first two are the reason for the copy). The
workspace uses it through `[patch.crates-io]` in `rust-engine/Cargo.toml`.

1. **No aws-lc.** The `rustls` feature fell back to the aws-lc-rs provider, which pulls
   `aws-lc-sys` (a C library built with CMake) into the driver. The copy uses `ring`, like every
   other driver (`src/client/tls_stream/rustls_tls_stream.rs`, and `tokio-rustls` takes the
   `ring` feature without its default `aws_lc_rs` one in `Cargo.toml`).
2. **Cancel keeps the session.** `Client::cancel_query` sent the attention signal but stopped
   reading at the first end of message, and SQL Server acknowledges an attention in a message of
   its own after the interrupted response (`DONE` with the error bit, then `DONE_ATTN`). The
   acknowledgement stayed unread, `cancel_query` failed, and every later response was read one
   message late. The copy keeps reading until the acknowledgement
   (`src/client/connection.rs`, `Connection::cancel_request`).

3. **Exact money.** `money` and `smallmoney` were decoded into an `f64` (`ColumnData::F64`),
   which rounds the last digits of any value above about 9e11. They now decode into an exact
   `ColumnData::Numeric` of scale 4 (`src/tds/codec/column_data/money.rs`). Writing money
   parameters is unchanged.

4. **Column types of nullable money and datetime.** `Column::column_type()` said `Money` for
   a nullable `smallmoney` and `Datetimen` for a nullable `datetime` or `smalldatetime`; the
   column's length tells them apart, so they now say `Money4`, `Datetime4` or `Datetime`
   (`src/row.rs`).

All four are small and could go upstream; drop this directory and the `[patch]` entry when they do.

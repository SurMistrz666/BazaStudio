# SQLx 0.9.0 TCP destination override

Only the PostgreSQL and MySQL driver packages are vendored. Source: the exact
crates.io published `sqlx-postgres` and `sqlx-mysql` 0.9.0 archives, upstream
commit `003b698e99e024f3621b8043a2426fde5b741171`. The original MIT and Apache-2.0
license files, registry-normalized manifests and original manifests are retained.

The archive SHA256 values match the application's pre-patch Cargo.lock:

| Archive | SHA256 |
| --- | --- |
| sqlx-postgres-0.9.0.crate | `87a2bdd6e83f6b3ea525ca9fee568030508b58355a43d0b2c1674d5f79dcd65e` |
| sqlx-mysql-0.9.0.crate | `90b8020fe17c5f2c245bfa2505d7ef59c5604839527c740266ad2214acebea27` |

`UPSTREAM-SHA256.json` records the exact original bytes of all 195 copied package
files. Cache marker `.cargo-ok` and the per-package Cargo.lock are not copied.
`DIAL-ADDRESS.patch` contains the complete local source change.

## Reason

Both stock drivers use `options.host` for both TCP resolution and TLS hostname
verification. The PostgreSQL URL parser's `hostaddr` property replaces the same
`host` value; it does not implement libpq's independent `host`/`hostaddr` behavior.
Using a local tunnel hostname would therefore verify the certificate against the
wrong server identity. Neither driver exposes a public custom-socket constructor
for the complete authenticated connection.

## Narrow API change

`PgConnectOptions::dial_address(SocketAddr)` and
`MySqlConnectOptions::dial_address(SocketAddr)` override only the TCP destination.
The optional override defaults to `None`, is preserved by cloning, forces TCP, and
is not parsed from URLs or environment variables. `get_dial_address()` returns it.
The logical host and port remain unchanged, including the host passed to rustls
for SNI and certificate SAN verification. All existing TLS modes, root certificate
loading, authentication, protocol handshakes and query implementations are left
unchanged. No TLS verifier or certificate policy is modified.

The application applies this option only to `127.0.0.1:<owned SSH listener>` after
the gateway host-key pin has been validated. Direct connections use no override.
This is a transport feature, not a certificate bypass.

Only four upstream files are changed: each driver's options module, PostgreSQL
connection stream construction, and MySQL connection establishment. Two options
unit tests check that the original hostname, remote port, strict TLS mode and CA
path survive setting and cloning the override. These tests do not replace actual
TLS acceptance tests. Release validation must also connect to an owned TLS
PostgreSQL and MySQL/MariaDB fixture through SSH and prove that correct SAN+CA
succeed, wrong SAN and untrusted CA refuse, and no plaintext downgrade occurs.

Primary source references:

- [SQLx PostgreSQL stream](https://github.com/transact-rs/sqlx/blob/v0.9.0/sqlx-postgres/src/connection/stream.rs)
- [SQLx PostgreSQL URL parsing](https://github.com/transact-rs/sqlx/blob/v0.9.0/sqlx-postgres/src/options/parse.rs)
- [SQLx MySQL establishment](https://github.com/transact-rs/sqlx/blob/v0.9.0/sqlx-mysql/src/connection/establish.rs)
- [SQLx rustls handshake](https://github.com/transact-rs/sqlx/blob/v0.9.0/sqlx-core/src/net/tls/tls_rustls.rs)
- [libpq host and hostaddr](https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNECT-HOSTADDR)

When updating SQLx, re-evaluate upstream socket/hostname APIs first. Remove the
patch if upstream exposes a suitable equivalent. Re-vendor only verified
published archives, regenerate the original manifest and isolated diff, and rerun
strict TLS acceptance on both desktop platforms.

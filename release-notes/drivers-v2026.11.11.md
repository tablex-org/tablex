## Drivers

PostgreSQL 0.1.1, MySQL 0.1.1, MariaDB 0.1.1, ClickHouse 0.1.1, MongoDB 0.2.3. Update them in **Driver Manager**.

## Fixes

- A statement on a server that stopped answering no longer waits forever: about 10 seconds after **Cancel** it is dropped and the connection is reported lost, so TableX reconnects.
- A second statement on a busy connection gives up after 30 seconds instead of waiting forever.
- PostgreSQL and MySQL report a closed connection as lost, so TableX reconnects after a server restart; a dead connection is noticed sooner (TCP keepalive every 30 seconds).
- MongoDB queries can be cancelled.

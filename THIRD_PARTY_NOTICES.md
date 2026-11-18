# Third-party notices

tritone is MIT licensed (see `LICENSE`). Code adapted from other projects is listed here, with the project, its license and the files it went into.

## Serein

Dual licensed MIT or Apache-2.0.

- `src/discord/login.rs` (`validate_pasted`): the checks on a pasted token follow Serein's `crates/client-core/src/auth.rs`.
- `src/discord/gateway/conn.rs` (`Timing::default`): the connect and HELLO timeouts are Serein's.

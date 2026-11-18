# tritone

Local MCP server that lets Claude triage and answer your Discord, as you.

> **Heads-up:** tritone drives your own Discord **user account** through a browser-like session. That's not an official API: automating a user account goes against Discord's Terms of Service and can get the account terminated. It may also break whenever Discord changes its web client. tritone paces every call, queues every send, never solves CAPTCHAs, and only writes after you approve the exact action, but the risk stays yours. Use it on your own account only, and at your own responsibility.

**Status:** under construction.

## Building

tritone talks to Discord through [wreq](https://crates.io/crates/wreq), which builds BoringSSL from source so its TLS looks like Chrome's. Besides Rust (edition 2024), the build needs:

- **cmake**;
- **a C/C++ compiler**;
- **libclang**, for the generated bindings.

On macOS, the Xcode Command Line Tools (`xcode-select --install`) plus `brew install cmake` cover it. On Debian/Ubuntu: `sudo apt install cmake clang libclang-dev`.

```sh
cargo build --release
```

The first build compiles BoringSSL and takes a few minutes.

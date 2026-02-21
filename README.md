# Any Player Shared Rust

Shared Rust workspace for Any Player platform integrations (desktop + Android), including:

- portable domain/provider contracts (`any_player_core`)
- Spotify session/playback engine primitives (`any_player_spotify_engine`)
- Android JNI bridge crate (`any_player_ffi_android`)

## Workspace crates

- `crates/any_player_core`
  - Shared models, provider contracts, and provider API/client modules.
  - Platform-agnostic by design (no UI/runtime shell assumptions).
- `crates/any_player_spotify_engine`
  - Runtime-agnostic Spotify session engine and librespot-backed player wrapper.
  - Exposes session lifecycle and playback-facing primitives used by platform adapters.
- `crates/any_player_ffi_android`
  - JNI-facing bridge for Android.
  - Exports token/session/auth/playback entry points as JSON-based interfaces.

## Why `patches/librespot-core` exists

This workspace patches `librespot-core` via `[patch.crates-io]` in the root `Cargo.toml` to avoid a build-script dependency conflict. The patch keeps source behavior intact while adjusting build-time dependency wiring.

## Prerequisites

- Rust stable toolchain
- On Linux (for native desktop-style builds that pull audio deps):
  - `pkg-config`
  - `libasound2-dev`

Example:

```bash
sudo apt-get update
sudo apt-get install -y pkg-config libasound2-dev
```

## Common commands

From repository root:

```bash
cargo check --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Build the Android bridge crate for host (sanity checks):

```bash
cargo build -p any_player_ffi_android
```

## Android cross-compilation

Add Android targets (once):

```bash
rustup target add \
  aarch64-linux-android \
  armv7-linux-androideabi \
  i686-linux-android \
  x86_64-linux-android
```

Build JNI library for a specific ABI:

```bash
cargo build -p any_player_ffi_android --target aarch64-linux-android --release
```

Resulting shared object path pattern:

- `target/<target-triple>/release/libany_player_ffi_android.so`

## CI

GitHub Actions workflow: `.github/workflows/ci.yml`

Current pipeline runs:

- `cargo check --workspace`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`

## Notes

- This repository is intended to be consumed by platform shells (desktop and Android), not as a standalone end-user application.
- Platform-owned OAuth orchestration and app-specific UX remain outside this workspace.

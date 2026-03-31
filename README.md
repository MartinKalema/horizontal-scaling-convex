# horizontal-scaling-convex

Making the Convex open-source backend horizontally scalable via a primary-replica architecture.

## Prerequisites

- **Rust** (nightly-2026-02-18) — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Just** — `brew install just`
- **Node.js 20.19.5** — `nvm install 20.19.5`

The Rust nightly toolchain installs automatically from `rust-toolchain` when you first run `cargo`.

## Development

```sh
# Build the database crate (where our changes live)
cargo build -p database

# Run database crate tests
cargo test -p database

# Run a specific test
cargo test -p database "test_name"

# Format code
cargo fmt -p database

# Build the full backend
cargo build

# Run all tests (slow, ~12min first run)
cargo test

# Run the full backend locally
just run-local-backend
```

## JS Dependencies (only needed for full backend)

```sh
npm clean-install --prefix scripts
just rush install
```

## Documentation

See `docs/` for architecture analysis and implementation plans.

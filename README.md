# mcbot

Minecraft bot built with [Azalea](https://github.com/azalea-rs/azalea) (Rust).

## Build

```bash
cargo build --release
```

## Run

```bash
MC_USERNAME=bot MC_SERVER=play.example.com cargo run --release
```

Environment variables:

- `MC_USERNAME` — bot username (default `mcbot`). Use an email for Microsoft accounts.
- `MC_SERVER` — server address (default `localhost`).
- `MC_PASSWORD` — optional password for auto-`/register` and auto-`/login` on AuthMe-style servers.

## CI

GitHub Actions builds and tests on every push/PR (`.github/workflows/ci.yml`).
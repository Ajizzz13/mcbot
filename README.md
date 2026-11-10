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

- `MC_USERNAME` — bot username (default `RakaPratama`). Use an email for Microsoft accounts.
- `MC_SERVER` — server address (default `localhost`).
- `MC_PASSWORD` — optional password for auto-`/register` and auto-`/login` on AuthMe-style servers.

In-game AI behaviors: auto-defend against melee mobs, evade creepers, eat food from hotbar when hungry, explore around spawn/home, look at nearby players, and respond to `!help`, `!status`, `!sini`, `!ikut`, `!diam`, `!jalan` or natural mentions.

## CI

GitHub Actions builds and tests on every push/PR (`.github/workflows/ci.yml`).
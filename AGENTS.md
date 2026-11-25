# Agent notes

## Before every push

Run `scripts/check.sh` (about 210 s from a fresh clone on an Apple-silicon Mac, a few seconds once `target/` is built). It runs what CI runs on pull requests and main. CI uses the current stable Rust, so run `rustup update stable` first if yours is older.

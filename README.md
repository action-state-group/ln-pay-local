# ln-pay-local

A small command-line program that makes and pays Lightning invoices between two local
[Lexe](https://lexe.app) wallets. It needs no node of your own and no mesh-llm. It exists so a demo can run a real
payment on one machine; it is also a usable example of the Lexe Rust SDK.

> **Real money.** It talks to Lexe's mainnet. Use small amounts and wallets you made for this. Each wallet
> directory holds the wallet's recovery seed in `seedphrase.txt`: back it up, never commit it, never share it.

```text
ln-pay-local init    --wallet <dir>                          make a new wallet (or finish provisioning one)
ln-pay-local balance --wallet <dir>                          spendable Lightning balance, in msat
ln-pay-local invoice <msat> --wallet <dir>                   an invoice for exactly <msat>, to receive a payment
ln-pay-local pay <bolt11> --wallet <dir> [--max-fee-msat <n>]  pay an invoice (default fee cap: 1000 msat)
```

Every command prints one JSON object and exits 0, or prints `{"error": "..."}` on stderr and exits 1. Examples:

```text
$ ln-pay-local invoice 289 --wallet ~/wallets/seller
{"invoice": "lnbc2890p1...", "amount_msat": 289}
$ ln-pay-local pay lnbc2890p1... --wallet ~/wallets/buyer
{"status": "succeeded", "amount_msat": 289, "fee_msat": 0, "payment_hash": "..."}
```

## What it does and does not do

- `init` is the only command that creates a wallet. With no seed in the directory it makes one (the SDK writes the
  seed, mode 0600; the program never prints, copies or logs it) and provisions it with Lexe. With a seed already
  there it keeps that seed and only repeats the provisioning, which the SDK makes idempotent, so an `init` that was
  cut short can be run again. Every other command opens an existing wallet and refuses a directory without
  `seedphrase.txt`, so a wrong path never makes a new wallet.
- `pay` asks Lexe for the route first and refuses a route whose amount is not exactly the invoice's or whose fee is
  over `--max-fee-msat`, then pays and waits (up to 90 s) for that payment to settle. It reports `succeeded` or
  `failed`; a payment that was submitted but not seen to settle in time is reported `pending` with its
  `payment_hash`: check the wallet before paying again. If Lexe's answer to the submit itself is uncertain it says
  so and exits 1.
- A wallet directory is locked while a command runs (`wallet.lock`); a second process on the same directory is
  refused. Lexe's local cache is not a multi-process ledger, so do not point two programs at one wallet.
- It is mainnet only and keeps no state of its own beyond what the Lexe SDK writes in the wallet directory.

## Build

```bash
cargo install --locked --git https://github.com/action-state-group/ln-pay-local
```

or `cargo build --release` in a checkout. The pinned `Cargo.lock` matters: the `lexe` crates are released together
and a mixed set of versions does not compile.

## Try it with two wallets

```bash
ln-pay-local init --wallet ~/wallets/buyer
ln-pay-local init --wallet ~/wallets/seller
# fund the buyer: make an invoice on the buyer and pay it from any Lightning wallet you control
ln-pay-local invoice 20000 --wallet ~/wallets/buyer
ln-pay-local balance --wallet ~/wallets/buyer
# then a payment between them
INV=$(ln-pay-local invoice 289 --wallet ~/wallets/seller | python3 -c 'import sys,json; print(json.load(sys.stdin)["invoice"])')
ln-pay-local pay "$INV" --wallet ~/wallets/buyer
```

A new Lexe wallet may need a first receive before it can send, and Lexe may charge to open channel liquidity on that
first receive; the amounts above are only an example. These steps were not all run on a fresh pair of wallets when
this README was written.

## Use in the evidencebook ARD demo

The [ARD two-seller demo](https://github.com/action-state-group/evidencebook/tree/main/python/examples/ard-commerce)
has a `local` payment rail that calls this program: set `ARD_PAY_LOCAL` to the binary, `ARD_WALLET_BUYER` and
`ARD_WALLET_SELLER` to two wallet directories, and run the demo with `--rail local`.

## License

Apache-2.0. Contributions need a DCO sign-off (`git commit -s`).

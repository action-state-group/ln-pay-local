# ln-pay-local

A small command-line program that makes and pays Lightning invoices between two local
[Lexe](https://lexe.app) wallets. It needs no node of your own and no mesh-llm. It exists so a demo can run a real
payment on one machine; it is also a usable example of the Lexe Rust SDK.

> **Real money, example software.** It talks to Lexe's mainnet. Use small amounts and wallets you made for this. It is
> example code: not audited, no warranty (Apache-2.0), and unofficial, not affiliated with Lexe. Each wallet
> directory holds the wallet's seed in `seedphrase.txt` as an unencrypted file (mode 0600 on Unix): anyone who has the file
> controls the funds. Back it up, keep wallet directories outside any repository, never share the file. Whether a seed
> restores a wallet is Lexe's matter, not this program's.

```text
ln-pay-local init    --wallet <dir>                          make a new wallet (or finish provisioning one)
ln-pay-local balance --wallet <dir>                          spendable Lightning balance, in msat
ln-pay-local invoice <msat> --wallet <dir>                   an invoice for exactly <msat>, to receive a payment
ln-pay-local pay <bolt11> --wallet <dir> [--max-fee-msat <n>]  pay an invoice (default fee cap: 1000 msat)
```

A command that completes prints one JSON object and exits 0; an error prints `{"error": "..."}` on stderr and exits 1. A payment that did not succeed is not an error here: `pay` still exits 0 and says so in `status` (`failed` or `pending`), so read `status`, not the exit code. Examples:

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
  `payment_hash`: look the payment up in the Lexe app (or with the Lexe SDK) before paying again. Paying the same
  invoice again is refused by Lexe's preflight when this wallet already tried it. If Lexe's answer to the submit
  itself is uncertain it says so and exits 1.
- A wallet directory is locked while a command runs (`wallet.lock`); a second process on the same directory is
  refused. Lexe's local cache is not a multi-process ledger, so do not point two programs at one wallet.
- It is mainnet only and keeps no state of its own beyond what the Lexe SDK writes in the wallet directory.

## Build

Needs a current stable Rust toolchain (the crate uses edition 2024).

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
ln-pay-local invoice <amount> --wallet ~/wallets/buyer
ln-pay-local balance --wallet ~/wallets/buyer
# then a payment between them
INV=$(ln-pay-local invoice 289 --wallet ~/wallets/seller | jq -r .invoice)   # or parse the JSON with any tool
ln-pay-local pay "$INV" --wallet ~/wallets/buyer
```

How much a new Lexe wallet needs for its first receive, and what Lexe charges for that, is Lexe's to say: look up its
current minimums and fees and choose `<amount>` accordingly. The two-wallet payment above has not been run end to end on
a fresh pair of wallets yet.

## Planned use in the evidencebook ARD demo

[evidencebook#483](https://github.com/action-state-group/evidencebook/pull/483) (open, not merged) adds a `local`
payment rail to the ARD two-seller demo that will call this program: `ARD_PAY_LOCAL` for the binary,
`ARD_WALLET_BUYER` and `ARD_WALLET_SELLER` for two wallet directories, and `--rail local`. Until it merges, the demo
on `main` has no such rail.

## License

Apache-2.0. Please sign off commits (`git commit -s`); the sign-off is requested, not yet enforced by a check.

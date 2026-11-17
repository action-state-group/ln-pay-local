// SPDX-License-Identifier: Apache-2.0
//! `ln-pay-local`: create and pay Lightning invoices with a local Lexe wallet.
//!
//! ```text
//! ln-pay-local init --wallet <dir>
//! ln-pay-local invoice <msat> --wallet <dir>
//! ln-pay-local pay <bolt11> --wallet <dir> [--max-fee-msat <n>]
//! ln-pay-local balance --wallet <dir>
//! ln-pay-local payments --wallet <dir> [--limit <n>]
//! ```
//!
//! Each command prints one JSON object and exits 0, or prints `{"error": "..."}` on stderr and exits 1.
//! A wallet directory is one the Lexe SDK (and `mesh-wallet-lexe`) already keeps: it holds `seedphrase.txt`.
//! Only `init` creates a wallet, and only when the directory holds no seed: the SDK writes the seed file
//! (mode 0600) and this program never prints, copies or logs it. `init` on a directory that already holds a seed
//! never replaces it; it only repeats the (idempotent) provisioning, so an interrupted `init` can be finished.
//! Every other command opens an existing wallet and refuses a directory without one, so a wrong path never
//! provisions a wallet. Mainnet only, real money.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use lexe::config::WalletEnvConfig;
use lexe::types::auth::{CredentialsRef, RootSeed};
use lexe::types::bitcoin::Amount;
use lexe::types::command::CreateInvoiceRequest;
use lexe::types::payment::{
    PaymentCreatedIndex, PaymentDirection, PaymentFilter, PaymentKind, PaymentStatus,
};
use lexe::wallet::LexeWallet;
use lexe_api_core::def::UserNodeRunApi;
use lexe_api_core::models::command::{PayInvoicePreflightRequest, PayInvoiceRequest};

/// An invoice made for a demo sale lives this long.
const INVOICE_EXPIRY_SECS: u32 = 3600;
/// The most a payment's fee may be unless the caller says otherwise (the demo's own cap).
const DEFAULT_MAX_FEE_MSAT: u64 = 1000;
/// How long `pay` waits for the payment to settle before it reports `pending`.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);

struct Opened {
    wallet: LexeWallet,
    // Lexe's local cache is not a multi-process ledger: hold the directory's lock (the same file the
    // mesh-llm wallet plugin takes) for as long as the wallet is open.
    _lock: std::fs::File,
}

/// Take the directory's exclusive lock, creating the lock file. The directory must exist.
fn lock(dir: &Path) -> Result<std::fs::File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("wallet.lock"))
        .context("could not open the wallet lock")?;
    lock.try_lock_exclusive().context(
        "the wallet is open in another process (a running mesh-llm node, or another command)",
    )?;
    Ok(lock)
}

fn open(dir: &Path) -> Result<Opened> {
    ensure!(
        dir.is_dir(),
        "wallet directory {} does not exist",
        dir.display()
    );
    let seed_path = dir.join("seedphrase.txt");
    ensure!(
        seed_path.is_file(),
        "no wallet in {}: this program opens an existing wallet and never creates one",
        dir.display()
    );
    let lock = lock(dir)?;
    let seed = RootSeed::read_from_path(&seed_path)
        .map_err(|_| anyhow::anyhow!("could not read the wallet seed"))?
        .context("the wallet seed file is empty")?;
    let wallet = load(&seed, dir)?;
    Ok(Opened {
        wallet,
        _lock: lock,
    })
}

fn load(seed: &RootSeed, dir: &Path) -> Result<LexeWallet> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    LexeWallet::load_or_fresh(
        WalletEnvConfig::mainnet(),
        CredentialsRef::from(seed),
        Some(dir.to_path_buf()),
    )
    .map_err(|_| anyhow::anyhow!("could not load the Lexe wallet"))
}

/// Make a wallet in a directory that holds none and provision it with Lexe. With a seed already there it keeps the
/// seed and only repeats the provisioning, which the SDK makes idempotent: that finishes an `init` that was cut
/// short. The seed is written by the SDK and is never printed: back the directory up, it is the only recovery
/// material.
async fn init(dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir).context("could not create the wallet directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .context("could not restrict the wallet directory")?;
    }
    let _lock = lock(dir)?;
    let seed_path = dir.join("seedphrase.txt");
    let created = !seed_path.exists();
    let seed = if created {
        let seed = RootSeed::generate();
        seed.write_to_path(&seed_path)
            .map_err(|_| anyhow::anyhow!("could not write the wallet seed"))?;
        OpenOptions::new()
            .write(true)
            .open(&seed_path)?
            .sync_all()?;
        seed
    } else {
        RootSeed::read_from_path(&seed_path)
            .map_err(|_| anyhow::anyhow!("could not read the wallet seed"))?
            .context("the wallet seed file is empty")?
    };
    let wallet = load(&seed, dir)?;
    wallet.signup(&seed, None).await.map_err(|_| {
        anyhow::anyhow!(
            "Lexe wallet provisioning failed; the seed is kept, run init again to retry"
        )
    })?;
    Ok(format!(
        "{{\"initialized\": true, \"created\": {created}, \"user_pk\": \"{}\"}}",
        wallet.user_config().user_pk
    ))
}

async fn balance(dir: &Path) -> Result<String> {
    let o = open(dir)?;
    let info = o.wallet.node_info().await.map_err(|_| {
        anyhow::anyhow!("Lexe balance query failed (is the wallet provisioned? try init)")
    })?;
    Ok(format!(
        "{{\"spendable_msat\": {}}}",
        info.lightning_sendable_balance.msat()
    ))
}

/// The wallet's newest payments, read-only: direction, status, amount, fee, time and payment hash. The preimage
/// (proof of payment, a secret for inbound payments) is never printed.
async fn payments(dir: &Path, limit: usize) -> Result<String> {
    ensure!(
        (1..=200).contains(&limit),
        "the limit must be between 1 and 200"
    );
    let o = open(dir)?;
    o.wallet
        .sync_payments()
        .await
        .map_err(|_| anyhow::anyhow!("Lexe payment synchronization failed"))?;
    let listed = o
        .wallet
        .list_payments(&PaymentFilter::All, None, Some(limit), None)
        .map_err(|_| anyhow::anyhow!("Lexe payment query failed"))?;
    let rows: Vec<String> = listed
        .payments
        .iter()
        .map(|p| {
            let direction = match p.direction {
                PaymentDirection::Inbound => "inbound",
                PaymentDirection::Outbound => "outbound",
                PaymentDirection::Info => "info",
            };
            let status = match p.status {
                PaymentStatus::Completed => "succeeded",
                PaymentStatus::Failed => "failed",
                PaymentStatus::Pending => "pending",
            };
            let amount = p
                .amount
                .map_or("null".to_string(), |a| a.msat().to_string());
            let hash = p
                .hash
                .map_or("null".to_string(), |h| format!("\"{h}\""));
            format!(
                "{{\"direction\": \"{direction}\", \"status\": \"{status}\", \"amount_msat\": {amount}, \"fee_msat\": {}, \"created_at_ms\": {}, \"payment_hash\": {hash}}}",
                p.fees.msat(),
                p.created_at.to_millis()
            )
        })
        .collect();
    Ok(format!("{{\"payments\": [{}]}}", rows.join(", ")))
}

async fn invoice(dir: &Path, amount_msat: u64) -> Result<String> {
    ensure!(amount_msat > 0, "the amount must be more than zero");
    let o = open(dir)?;
    let made = o
        .wallet
        .create_invoice(CreateInvoiceRequest {
            expiration_secs: Some(INVOICE_EXPIRY_SECS),
            amount: Some(Amount::from_msat(amount_msat)),
            description: Some("ard-demo".into()),
            ..Default::default()
        })
        .await
        .map_err(|_| anyhow::anyhow!("Lexe invoice creation failed"))?;
    Ok(format!(
        "{{\"invoice\": \"{}\", \"amount_msat\": {amount_msat}}}",
        made.invoice
    ))
}

/// The preflighted route may be refused: it must carry exactly the invoice's amount (Lexe may raise it to reach an
/// intermediate hop's minimum) and its fee must be within the cap.
fn check_route(
    invoice_msat: Option<u64>,
    route_amount_msat: u64,
    route_fee_msat: u64,
    max_fee_msat: u64,
) -> Result<()> {
    let stated = invoice_msat.context("the invoice states no amount: not paying it")?;
    ensure!(
        route_amount_msat == stated,
        "the route would send {route_amount_msat} msat for an invoice of {stated} msat: not paying it"
    );
    ensure!(
        route_fee_msat <= max_fee_msat,
        "the fee {route_fee_msat} msat is over the allowed {max_fee_msat}"
    );
    Ok(())
}

async fn pay(dir: &Path, bolt11: &str, max_fee_msat: u64) -> Result<String> {
    let o = open(dir)?;
    let parsed: lexe::types::bitcoin::Invoice = bolt11
        .parse()
        .map_err(|_| anyhow::anyhow!("not a valid invoice"))?;
    let route = o
        .wallet
        .node_client()
        .pay_invoice_preflight(PayInvoicePreflightRequest {
            invoice: parsed.clone(),
            fallback_amount: None,
            kind: PaymentKind::Invoice,
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Lexe payment preflight failed (own invoice, already paid, no route or no balance)"
            )
        })?;
    check_route(
        parsed.amount().map(|a| a.msat()),
        route.amount.msat(),
        route.fees.msat(),
        max_fee_msat,
    )?;
    let amount = route.amount.msat();
    let hash = parsed.payment_hash().to_string();
    let submitted = o
        .wallet
        .node_client()
        .pay_invoice(PayInvoiceRequest {
            invoice: parsed.clone(),
            fallback_amount: None,
            message: None,
            personal_note: None,
            kind: PaymentKind::Invoice,
            ldk_route: Some(route.ldk_route),
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("Lexe payment outcome uncertain: check the wallet before paying again")
        })?;
    // From here the payment is submitted: whatever goes wrong while watching it is `pending` with its hash, never a
    // plain error, so a caller cannot read "not paid" into "could not look".
    let index = PaymentCreatedIndex {
        created_at: submitted.created_at,
        id: parsed.payment_id(),
    };
    // The SDK checks its deadline only between requests, so a request that starts near the deadline could run past it
    // while this process holds the wallet lock: bound the whole wait.
    let waited = tokio::time::timeout(
        SETTLE_TIMEOUT,
        o.wallet.wait_for_payment(index, Some(SETTLE_TIMEOUT)),
    )
    .await;
    let (status, fee) = match waited {
        Ok(Ok(p)) => (
            match p.status {
                PaymentStatus::Completed => "succeeded",
                PaymentStatus::Failed => "failed",
                PaymentStatus::Pending => "pending",
            },
            p.fees.msat(),
        ),
        Ok(Err(_)) | Err(_) => ("pending", route.fees.msat()),
    };
    Ok(format!(
        "{{\"status\": \"{status}\", \"amount_msat\": {amount}, \"fee_msat\": {fee}, \"payment_hash\": \"{hash}\"}}"
    ))
}

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "usage: ln-pay-local init --wallet <dir> | invoice <msat> --wallet <dir> | pay <bolt11> --wallet <dir> [--max-fee-msat <n>] | balance --wallet <dir> | payments --wallet <dir> [--limit <n>]"
    )
}

/// A JSON string literal for `s` (quotes, backslashes and control characters escaped as JSON requires).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(out) => println!("{out}"),
        Err(e) => {
            eprintln!("{{\"error\": {}}}", json_string(&format!("{e:#}")));
            std::process::exit(1);
        }
    }
}

async fn run(args: &[String]) -> Result<String> {
    let mut positional = Vec::new();
    let mut wallet: Option<PathBuf> = None;
    let mut max_fee = DEFAULT_MAX_FEE_MSAT;
    let mut limit = 20usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--wallet" => wallet = Some(PathBuf::from(it.next().ok_or_else(usage)?)),
            "--max-fee-msat" => {
                max_fee = it.next().ok_or_else(usage)?.parse().map_err(|_| usage())?
            }
            "--limit" => limit = it.next().ok_or_else(usage)?.parse().map_err(|_| usage())?,
            _ => positional.push(a.clone()),
        }
    }
    let dir = wallet.ok_or_else(usage)?;
    match positional.as_slice() {
        [c] if c == "init" => init(&dir).await,
        [c] if c == "balance" => balance(&dir).await,
        [c] if c == "payments" => payments(&dir, limit).await,
        [c, n] if c == "invoice" => invoice(&dir, n.parse().map_err(|_| usage())?).await,
        [c, b] if c == "pay" => pay(&dir, b, max_fee).await,
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ln-pay-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn a_wrong_directory_is_refused_and_left_untouched() {
        let missing = temp("missing");
        let err = balance(&missing).await.unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!missing.exists());

        let empty = temp("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = balance(&empty).await.unwrap_err().to_string();
        assert!(err.contains("never creates one"), "{err}");
        assert_eq!(
            std::fs::read_dir(&empty).unwrap().count(),
            0,
            "a refused open leaves no file behind"
        );
        std::fs::remove_dir_all(&empty).unwrap();
    }

    #[tokio::test]
    async fn init_never_replaces_an_existing_seed() {
        let dir = temp("existing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seedphrase.txt"), b"placeholder").unwrap();
        // The placeholder is not a seed the SDK reads, so init fails; it must not have replaced it.
        assert!(init(&dir).await.is_err());
        assert_eq!(
            std::fs::read(dir.join("seedphrase.txt")).unwrap(),
            b"placeholder"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_process_on_one_wallet_is_refused() {
        let dir = temp("locked");
        std::fs::create_dir_all(&dir).unwrap();
        let first = lock(&dir).unwrap();
        let err = format!("{:#}", lock(&dir).unwrap_err());
        assert!(err.contains("open in another process"), "{err}");
        drop(first);
        assert!(lock(&dir).is_ok(), "the lock is released when dropped");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_route_must_match_the_invoice_and_the_fee_cap() {
        assert!(check_route(Some(289), 289, 3, 1000).is_ok());
        let raised = check_route(Some(289), 300, 0, 1000)
            .unwrap_err()
            .to_string();
        assert!(
            raised.contains("300 msat for an invoice of 289"),
            "{raised}"
        );
        let dear = check_route(Some(289), 289, 1001, 1000)
            .unwrap_err()
            .to_string();
        assert!(dear.contains("over the allowed 1000"), "{dear}");
        let open = check_route(None, 289, 0, 1000).unwrap_err().to_string();
        assert!(open.contains("states no amount"), "{open}");
    }

    #[test]
    fn error_strings_are_valid_json() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("x\u{1b}y\n"), "\"x\\u001by\\n\"");
    }

    #[tokio::test]
    async fn commands_need_a_wallet_and_a_known_verb() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(
            run(&args(&["balance"]))
                .await
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
        assert!(
            run(&args(&["frobnicate", "--wallet", "x"]))
                .await
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
        assert!(
            run(&args(&["invoice", "abc", "--wallet", "x"]))
                .await
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
        assert!(
            run(&args(&["pay", "--wallet"]))
                .await
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
    }

    #[tokio::test]
    async fn payments_limits_are_checked_before_any_wallet_is_opened() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let zero = run(&args(&[
            "payments",
            "--wallet",
            "/nonexistent",
            "--limit",
            "0",
        ]))
        .await
        .unwrap_err()
        .to_string();
        assert!(zero.contains("between 1 and 200"), "{zero}");
        let missing = run(&args(&["payments", "--wallet", "/nonexistent"]))
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("does not exist"), "{missing}");
    }
}

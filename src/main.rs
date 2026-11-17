// SPDX-License-Identifier: Apache-2.0
//! `ln-pay-local`: create and pay Lightning invoices with a local Lexe wallet.
//!
//! ```text
//! ln-pay-local init --wallet <dir>
//! ln-pay-local invoice <msat> --wallet <dir>
//! ln-pay-local pay <bolt11> --wallet <dir> [--max-fee-msat <n>]
//! ln-pay-local balance --wallet <dir>
//! ```
//!
//! Each command prints one JSON object and exits 0, or prints `{"error": "..."}` on stderr and exits 1.
//! A wallet directory is one the Lexe SDK (and `mesh-wallet-lexe`) already keeps: it holds `seedphrase.txt`.
//! Only `init` creates a wallet, and only in a directory that holds none: the SDK writes the seed file (mode 0600)
//! and this program never prints, copies or logs it. Every other command opens an existing wallet and refuses a
//! directory without one, so a wrong path never provisions a wallet. Mainnet only, real money.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use lexe::config::WalletEnvConfig;
use lexe::types::auth::{CredentialsRef, RootSeed};
use lexe::types::bitcoin::Amount;
use lexe::types::command::CreateInvoiceRequest;
use lexe::types::payment::{
    PaymentDirection, PaymentFilter, PaymentHash, PaymentKind, PaymentStatus,
};
use lexe::wallet::LexeWallet;
use lexe_api_core::def::UserNodeRunApi;
use lexe_api_core::models::command::{PayInvoicePreflightRequest, PayInvoiceRequest};

/// An invoice made for a demo sale lives this long.
const INVOICE_EXPIRY_SECS: u32 = 3600;
/// The most a payment's fee may be unless the caller says otherwise (the demo's own cap).
const DEFAULT_MAX_FEE_MSAT: u64 = 1000;
/// How long `pay` waits for the payment to settle; Lexe is polled, not pushed, so keep the pace gentle.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_EVERY: Duration = Duration::from_secs(2);

struct Opened {
    wallet: LexeWallet,
    // Lexe's local cache is not a multi-process ledger: hold the directory's lock (the same file the
    // mesh-llm wallet plugin takes) for as long as the wallet is open.
    _lock: std::fs::File,
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

/// Make a new wallet in a directory that holds none, and provision it with Lexe. The seed is written by the SDK and
/// is never printed: back the directory up, it is the only recovery material.
async fn init(dir: &Path) -> Result<String> {
    let seed_path = dir.join("seedphrase.txt");
    ensure!(
        !seed_path.exists(),
        "a wallet already exists in {}: not touching it",
        dir.display()
    );
    std::fs::create_dir_all(dir).context("could not create the wallet directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .context("could not restrict the wallet directory")?;
    }
    let seed = RootSeed::generate();
    seed.write_to_path(&seed_path)
        .map_err(|_| anyhow::anyhow!("could not write the wallet seed"))?;
    OpenOptions::new()
        .write(true)
        .open(&seed_path)?
        .sync_all()?;
    let wallet = load(&seed, dir)?;
    wallet.signup(&seed, None).await.map_err(|_| anyhow::anyhow!("Lexe wallet provisioning failed (the wallet seed is kept: run init again is refused, open it with balance)"))?;
    Ok(format!(
        "{{\"initialized\": true, \"user_pk\": \"{}\"}}",
        wallet.user_config().user_pk
    ))
}

async fn balance(dir: &Path) -> Result<String> {
    let o = open(dir)?;
    let info =
        o.wallet.node_info().await.map_err(|_| {
            anyhow::anyhow!("Lexe balance query failed (is the wallet provisioned?)")
        })?;
    Ok(format!(
        "{{\"spendable_msat\": {}}}",
        info.lightning_sendable_balance.msat()
    ))
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
        "{{\"invoice\": \"{}\", \"amount_msat\": {}}}",
        made.invoice, amount_msat
    ))
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
    ensure!(
        route.fees.msat() <= max_fee_msat,
        "the fee {} msat is over the allowed {max_fee_msat}",
        route.fees.msat()
    );
    let amount = route.amount.msat();
    o.wallet
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
    let hash: PaymentHash = parsed
        .payment_hash()
        .to_string()
        .parse()
        .context("invalid payment hash")?;
    let started = Instant::now();
    loop {
        o.wallet
            .sync_payments()
            .await
            .map_err(|_| anyhow::anyhow!("Lexe payment synchronization failed"))?;
        let listed = o
            .wallet
            .list_payments(&PaymentFilter::All, None, Some(50), None)
            .map_err(|_| anyhow::anyhow!("Lexe payment query failed"))?;
        if let Some(p) = listed
            .payments
            .into_iter()
            .find(|p| p.hash == Some(hash) && p.direction == PaymentDirection::Outbound)
        {
            let status = match p.status {
                PaymentStatus::Completed => "succeeded",
                PaymentStatus::Failed => "failed",
                PaymentStatus::Pending => "pending",
            };
            if status != "pending" || started.elapsed() > SETTLE_TIMEOUT {
                return Ok(format!(
                    "{{\"status\": \"{status}\", \"amount_msat\": {}, \"fee_msat\": {}, \"payment_hash\": \"{hash}\"}}",
                    p.amount.map_or(amount, |a| a.msat()),
                    p.fees.msat()
                ));
            }
        } else if started.elapsed() > SETTLE_TIMEOUT {
            bail!(
                "the payment is not in the wallet's list after {} s: check the wallet",
                SETTLE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "usage: ln-pay-local init --wallet <dir> | invoice <msat> --wallet <dir> | pay <bolt11> --wallet <dir> [--max-fee-msat <n>] | balance --wallet <dir>"
    )
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(out) => println!("{out}"),
        Err(e) => {
            eprintln!("{{\"error\": {:?}}}", format!("{e:#}"));
            std::process::exit(1);
        }
    }
}

async fn run(args: &[String]) -> Result<String> {
    let mut positional = Vec::new();
    let mut wallet: Option<PathBuf> = None;
    let mut max_fee = DEFAULT_MAX_FEE_MSAT;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--wallet" => wallet = Some(PathBuf::from(it.next().ok_or_else(usage)?)),
            "--max-fee-msat" => {
                max_fee = it.next().ok_or_else(usage)?.parse().map_err(|_| usage())?
            }
            _ => positional.push(a.clone()),
        }
    }
    let dir = wallet.ok_or_else(usage)?;
    match positional.as_slice() {
        [c] if c == "init" => init(&dir).await,
        [c] if c == "balance" => balance(&dir).await,
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
        assert!(
            err.contains("never creates one") || err.contains("existing wallet"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(&empty).unwrap().count(),
            0,
            "a refused open leaves no file behind"
        );
        std::fs::remove_dir_all(&empty).unwrap();
    }

    #[tokio::test]
    async fn init_refuses_a_directory_that_already_holds_a_wallet() {
        let dir = temp("existing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seedphrase.txt"), b"placeholder").unwrap();
        let err = init(&dir).await.unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("seedphrase.txt")).unwrap(),
            b"placeholder"
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
}

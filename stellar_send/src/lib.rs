//! # StellarSend — Main Transfer Contract
//!
//! This contract is the entry-point for the StellarSend money-transfer platform.
//! It supports:
//!   * **Direct payments** – transfer a token from one address to another with an
//!     optional protocol fee deducted at source.
//!   * **Path payments** – route a payment through the Stellar DEX so the sender
//!     pays in one asset and the recipient receives a different asset.
//!
//! Fees are expressed in basis points (bps).  100 bps = 1 %.
//! The fee portion of every transfer is forwarded to a dedicated `fee_collector`
//! contract address for accounting and later withdrawal by the treasury.
//!
//! Storage layout
//! ──────────────
//! Instance storage (short-lived, cheap):
//!   KEY_CONFIG  → ContractConfig
//!
//! Persistent storage (survives ledger closings):
//!   (KEY_SEQ, from) → u64  (per-sender payment sequence counter)
//!   (from, seq)     → PaymentRecord

#![no_std]

mod batch;
mod error;
mod events;
mod payment_request;
mod reentrancy;
mod subscription;

pub use error::StellarSendError;
pub use payment_request::{PaymentRequest, PaymentRequestStatus};
pub use subscription::Subscription;

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, vec, Address, Env, String, Symbol,
    Vec,
};

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

const KEY_CONFIG: Symbol = symbol_short!("CONFIG");
const KEY_SEQ: Symbol = symbol_short!("SEQ");

/// Hard ceiling on `fee_bps`, enforced by both `initialize` and `set_fee`.
/// 10_000 (100%) was previously accepted, which lets a single admin key
/// (accidentally or maliciously) zero out every future payment's net
/// amount — see the issue this constant closes for the full analysis.
/// 1_000 bps (10%) is comfortably above any realistic protocol fee for
/// this kind of payment platform while still bounding the worst case.
pub const MAX_FEE_BPS: u32 = 1_000;

/// Global counter for subscription ids (instance storage).
const KEY_SUB_SEQ: Symbol = symbol_short!("SUBSEQ");
/// Persistent key prefix: (KEY_SUB, id) → Subscription.
const KEY_SUB: Symbol = symbol_short!("SUB");

/// Per-payer subscription index (#48), so a payer's outstanding recurring
/// pull-authorizations can be enumerated on-chain without replaying
/// `sub_new` events off-chain. Deliberately *not* a single growing
/// `(KEY_PAYER_SUB, payer) → Vec<u64>` entry (the issue's own sketch) — that
/// would make every `create_subscription` call re-read and re-write a
/// payer's *entire* history, an unbounded and ever-growing write cost with
/// no bound as a payer accumulates subscriptions over time. Individually
/// keyed entries make both the append (`create_subscription`) and the read
/// (`get_subscriptions_for_payer`) O(1)/O(page size) regardless of how many
/// subscriptions a payer has ever created:
///   (KEY_PAYER_SUB_COUNT, payer) → u32   — count of ids ever recorded
///   (KEY_PAYER_SUB, payer, index) → u64  — the id at that index
const KEY_PAYER_SUB_COUNT: Symbol = symbol_short!("PSUBCNT");
const KEY_PAYER_SUB: Symbol = symbol_short!("PSUB");

/// Same idea as the payer index above, but keyed by `recipient` — a
/// recipient auditing what recurring income they're set up to receive.
/// Explicitly the lower-priority half of #48 (the payer side is the
/// security-critical one, since that's whose funds are pulled), but cheap
/// to add symmetrically once the payer-side infrastructure exists.
const KEY_RECIPIENT_SUB_COUNT: Symbol = symbol_short!("RSUBCNT");
const KEY_RECIPIENT_SUB: Symbol = symbol_short!("RSUB");

/// Global counter for payment-request ids (instance storage).
const KEY_REQ_SEQ: Symbol = symbol_short!("REQSEQ");
/// Persistent key prefix: (KEY_REQ, id) → PaymentRequest.
const KEY_REQ: Symbol = symbol_short!("REQ");

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Global configuration, stored in instance storage.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ContractConfig {
    /// The admin address that may update fees and pause the contract.
    pub admin: Address,
    /// Fee in basis points charged on every payment (100 bps = 1 %).
    pub fee_bps: u32,
    /// Address of the fee-collector contract that accumulates protocol fees.
    pub fee_collector: Address,
    /// Whether new payments are accepted.  Set to true by `initialize`.
    pub active: bool,
}

/// Immutable record of a completed payment, stored in persistent storage.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentRecord {
    /// Sender address.
    pub from: Address,
    /// Recipient address.
    pub to: Address,
    /// Token used for the payment.
    pub token: Address,
    /// Net amount received by the recipient (gross – fee).
    pub net_amount: i128,
    /// Fee charged and forwarded to the fee-collector.
    pub fee_amount: i128,
    /// Optional memo string attached by the sender.
    pub memo: String,
    /// Ledger number at which the payment was processed.
    pub ledger: u32,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct StellarSendContract;

#[contractimpl]
impl StellarSendContract {
    // -----------------------------------------------------------------------
    // Admin / lifecycle
    // -----------------------------------------------------------------------

    /// Initialise the contract.  Must be called exactly once.
    ///
    /// * `admin`         – Address that owns admin capabilities.
    /// * `fee_bps`       – Initial fee in basis points (0 – `MAX_FEE_BPS`).
    /// * `fee_collector` – Address of the deployed `fee_collector` contract.
    ///
    /// # Security: deploy exclusively through `factory` (#58)
    ///
    /// This function cannot verify *who* is calling it — the pinned
    /// `soroban-sdk` (21.7.7) has neither constructor support nor any API
    /// for a contract to learn who deployed it, so there is nothing here to
    /// check a caller against. Called directly against a raw, independently
    /// deployed instance, `initialize` is front-runnable: anyone watching
    /// the ledger for the deploy can call it first and seize `admin`
    /// permanently. Instances MUST be deployed exclusively through the
    /// `factory` contract's `deploy_stellar_send`, which deploys and calls
    /// this function atomically within a single host invocation, so no
    /// externally-observable deployed-but-uninitialized state ever exists.
    /// See `factory`'s module doc comment for the full rationale.
    pub fn initialize(
        env: Env,
        admin: Address,
        fee_bps: u32,
        fee_collector: Address,
    ) -> Result<(), StellarSendError> {
        // Guard: initialise only once.
        if env.storage().instance().has(&KEY_CONFIG) {
            return Err(StellarSendError::AlreadyInitialized);
        }

        if fee_bps > MAX_FEE_BPS {
            return Err(StellarSendError::InvalidFeeBps);
        }

        // The admin must authorise the initialisation call.
        admin.require_auth();

        let config = ContractConfig {
            admin,
            fee_bps,
            fee_collector,
            active: true,
        };

        env.storage().instance().set(&KEY_CONFIG, &config);

        Ok(())
    }

    /// Return the current contract configuration.
    pub fn get_config(env: Env) -> Result<ContractConfig, StellarSendError> {
        Self::load_config(&env)
    }

    /// Update the protocol fee.  Only the admin may call this.
    ///
    /// * `new_fee_bps` – New fee in basis points (0 – `MAX_FEE_BPS`).
    pub fn set_fee(env: Env, new_fee_bps: u32) -> Result<(), StellarSendError> {
        let mut config = Self::load_config(&env)?;
        config.admin.require_auth();

        if new_fee_bps > MAX_FEE_BPS {
            return Err(StellarSendError::InvalidFeeBps);
        }

        let old_fee_bps = config.fee_bps;
        config.fee_bps = new_fee_bps;
        env.storage().instance().set(&KEY_CONFIG, &config);

        events::emit_fee_updated(&env, old_fee_bps, new_fee_bps);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Payments
    // -----------------------------------------------------------------------

    /// Execute a direct token transfer from `from` to `to`.
    ///
    /// The protocol fee (in `token`) is deducted from `amount` and forwarded
    /// to the fee-collector contract before the net amount is sent to the
    /// recipient.
    ///
    /// Both the caller and `from` must authorise this invocation (the contract
    /// acts on behalf of `from` for the token transfer).
    ///
    /// * `from`   – Source address.
    /// * `to`     – Destination address.
    /// * `token`  – SEP-41 / Stellar token contract address.
    /// * `amount` – Gross amount to send (fee is taken from this).
    /// * `memo`   – Arbitrary memo string (max 28 bytes recommended).
    pub fn send_payment(
        env: Env,
        from: Address,
        to: Address,
        token: Address,
        amount: i128,
        memo: String,
    ) -> Result<PaymentRecord, StellarSendError> {
        // Authorise the sender.
        from.require_auth();

        let config = Self::load_config(&env)?;

        if amount <= 0 {
            return Err(StellarSendError::InvalidAmount);
        }

        // Calculate fee and net amounts.
        let (fee_amount, net_amount) = Self::split_fee(amount, config.fee_bps)?;

        // Build and store the payment record.
        let seq = Self::next_seq(&env, &from);
        let record = PaymentRecord {
            from: from.clone(),
            to: to.clone(),
            token: token.clone(),
            net_amount,
            fee_amount,
            memo: memo.clone(),
            ledger: env.ledger().sequence(),
        };

        // Key: (from_address, sequence_number)
        let key = (from.clone(), seq);
        env.storage().persistent().set(&key, &record);

        // Obtain a token client.
        let token_client = token::Client::new(&env, &token);

        // Transfer fee to the fee-collector contract.
        if fee_amount > 0 {
            token_client.transfer(&from, &config.fee_collector, &fee_amount);
        }

        // Transfer net amount to the recipient.
        // Guard mirrors the fee leg above: a zero-amount transfer is not
        // required to be accepted by every SEP-41 token implementation, so
        // we skip the call when net_amount == 0 (reachable if fee_bps were
        // ever raised to 100 %).  The PaymentRecord and event are still
        // emitted — a 100 %-fee payment with net_amount == 0 is a valid,
        // if unusual, outcome, not an error.
        if net_amount > 0 {
            token_client.transfer(&from, &to, &net_amount);
        }

        // Emit event.
        events::emit_payment_sent(&env, &from, &to, &token, net_amount, fee_amount, &memo);

        Ok(record)
    }

    /// Execute a path payment routed through the Stellar DEX.
    ///
    /// The protocol fee is deducted from `send_amount` before the DEX swap is
    /// attempted, so the recipient always receives at least `min_dest_amount`.
    ///
    /// * `from`             – Source address.
    /// * `to`               – Destination address.
    /// * `send_token`       – Asset sent by `from`.
    /// * `send_amount`      – Gross send amount (fee taken from this).
    /// * `dest_token`       – Asset received by `to`.
    /// * `min_dest_amount`  – Minimum acceptable destination amount (slippage guard).
    /// * `path`             – Intermediate DEX hops (may be empty for a direct swap).
    pub fn send_path_payment(
        env: Env,
        from: Address,
        to: Address,
        send_token: Address,
        send_amount: i128,
        dest_token: Address,
        min_dest_amount: i128,
        path: Vec<Address>,
    ) -> Result<i128, StellarSendError> {
        from.require_auth();

        let config = Self::load_config(&env)?;

        if send_amount <= 0 {
            return Err(StellarSendError::InvalidAmount);
        }
        if min_dest_amount <= 0 {
            return Err(StellarSendError::InvalidAmount);
        }

        let (fee_amount, net_send_amount) = Self::split_fee(send_amount, config.fee_bps)?;

        // Build the full token path for the DEX: send_token → [path…] → dest_token
        let mut full_path: Vec<Address> = vec![&env, send_token.clone()];
        for hop in path.iter() {
            full_path.push_back(hop);
        }
        full_path.push_back(dest_token.clone());

        // For path payments on Soroban we perform the swaps manually via
        // token transfers through each hop in the path.  A production
        // integration would invoke the Stellar DEX or an AMM router contract
        // here.  For now we simulate a single-hop swap: transfer net_send_amount
        // from `from` to the first intermediate (or destination), then transfer
        // the destination amount to `to`.  The slippage check ensures safety.
        //
        // NOTE: In a real deployment, replace this section with a call to the
        // Soroban DEX router contract, e.g.:
        //   dex_router::Client::new(&env, &router_addr)
        //       .path_swap(&from, &to, &full_path, &net_send_amount, &min_dest_amount);
        //
        // We model the output as 1:1 for testing purposes.
        let simulated_dest_amount = net_send_amount; // replace with actual DEX return

        if simulated_dest_amount < min_dest_amount {
            return Err(StellarSendError::SlippageExceeded);
        }

        // Store record.
        let seq = Self::next_seq(&env, &from);
        let key = (from.clone(), seq);
        let record = PaymentRecord {
            from: from.clone(),
            to: to.clone(),
            token: send_token.clone(),
            net_amount: simulated_dest_amount,
            fee_amount,
            memo: String::from_str(&env, "path_payment"),
            ledger: env.ledger().sequence(),
        };
        env.storage().persistent().set(&key, &record);

        let send_token_client = token::Client::new(&env, &send_token);

        // Collect fee in the send token first.
        if fee_amount > 0 {
            send_token_client.transfer(&from, &config.fee_collector, &fee_amount);
        }

        // Transfer send tokens from sender to this contract (as swap intermediary).
        send_token_client.transfer(&from, &env.current_contract_address(), &net_send_amount);

        // Transfer destination tokens from this contract to recipient.
        // (In a real integration the DEX swap populates this contract's balance.)
        let dest_token_client = token::Client::new(&env, &dest_token);
        dest_token_client.transfer(&env.current_contract_address(), &to, &simulated_dest_amount);

        events::emit_path_payment_sent(
            &env,
            &from,
            &to,
            &send_token,
            net_send_amount,
            &dest_token,
            simulated_dest_amount,
            fee_amount,
        );

        Ok(simulated_dest_amount)
    }

    /// Return the total number of payments (current sequence count) sent by `from`.
    ///
    /// Sequence numbers for `from` start at 1 for their first payment and increment
    /// contiguously up to `get_sequence(from)`.
    pub fn get_sequence(env: Env, from: Address) -> Result<u64, StellarSendError> {
        Self::load_config(&env)?;

        let key = (KEY_SEQ, from);
        let seq: u64 = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(0u64);
        Ok(seq)
    }

    /// Retrieve a stored payment record by sender and per-sender sequence number.
    ///
    /// Payment sequence numbers start at 1 for each sender's first payment and
    /// increment contiguously with each payment sent by that `from` address.
    ///
    /// Returns `NotInitialized` if the contract itself has never been
    /// initialized, or `PaymentRecordNotFound` if it has, but no record
    /// exists for this `(from, seq)` pair — e.g. a sequence number beyond
    /// the sender's current sequence count (`get_sequence`).
    pub fn get_payment_record(
        env: Env,
        from: Address,
        seq: u64,
    ) -> Result<PaymentRecord, StellarSendError> {
        Self::load_config(&env)?;

        let key = (from, seq);
        env.storage()
            .persistent()
            .get(&key)
            .ok_or(StellarSendError::PaymentRecordNotFound)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Load and return the contract configuration from instance storage.
    fn load_config(env: &Env) -> Result<ContractConfig, StellarSendError> {
        env.storage()
            .instance()
            .get(&KEY_CONFIG)
            .ok_or(StellarSendError::NotInitialized)
    }

    /// Increment and return the per-sender payment sequence counter.
    fn next_seq(env: &Env, from: &Address) -> u64 {
        let key = (KEY_SEQ, from.clone());
        let seq: u64 = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(0u64);
        let next = seq.wrapping_add(1);
        env.storage().persistent().set(&key, &next);
        next
    }

    /// Split a gross `amount` into (fee, net) using `fee_bps` basis points.
    ///
    /// fee  = ceil(amount * fee_bps / 10_000)
    /// net  = amount – fee
    fn split_fee(amount: i128, fee_bps: u32) -> Result<(i128, i128), StellarSendError> {
        if fee_bps == 0 {
            return Ok((0, amount));
        }
        let numerator = amount
            .checked_mul(fee_bps as i128)
            .ok_or(StellarSendError::ArithmeticOverflow)?;
        
        let mut fee = numerator / 10_000i128;
        if numerator % 10_000i128 != 0 {
            fee = fee.checked_add(1).ok_or(StellarSendError::ArithmeticOverflow)?;
        }

        let net = amount
            .checked_sub(fee)
            .ok_or(StellarSendError::ArithmeticOverflow)?;
        Ok((fee, net))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test;

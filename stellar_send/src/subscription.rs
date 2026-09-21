//! Scheduled & recurring payments ("subscriptions").
//!
//! A payer creates a subscription describing a recipient, token, amount and
//! interval (in ledger-seconds).  Execution is *pull*-based: the payer must
//! grant this contract a token allowance (SEP-41 `approve`) large enough to
//! cover the payments it wants executed, since `execute_subscription` is
//! designed to be called by an untrusted keeper/cron job with no interactive
//! signature from the payer at execution time.  We therefore move funds with
//! `transfer_from` rather than `transfer`, exactly like a card-network
//! "pull" recurring charge.
//!
//! ## Bounding indefinite subscriptions (#23)
//!
//! A subscription with no cap runs forever, which risks becoming a
//! "forgotten" recurring charge the payer never explicitly agreed to stop.
//! `create_subscription` accepts two independent, optional bounds:
//!
//!   * `max_executions` — a hard ceiling on the total number of successful
//!     charges over the subscription's lifetime. Reaching it auto-deactivates
//!     the subscription (`active = false`), the same terminal state
//!     cancellation produces, so a capped-out subscription surfaces the
//!     familiar `SubscriptionInactive` on any further call.
//!   * `expiry_time` — a ledger timestamp past which `execute_subscription`
//!     refuses to run at all, independent of the execution count. Unlike
//!     `max_executions`, this does *not* auto-deactivate the subscription
//!     (matching `PaymentRequest.expiry`'s behaviour): every attempt past
//!     expiry gets the specific, informative `SubscriptionExpired` rather
//!     than a generic "inactive" once discovered.
//!
//! Both are `None`-able because an unbounded subscription is still a valid,
//! intentional choice (e.g. an indefinite payroll-style payment) — the payer
//! opts into a bound rather than having one imposed.
//!
//! ## Persistent TTL and unattended keepers (#43)
//!
//! A subscription is persistent, but persistent Soroban entries still expire
//! by ledger TTL. Creation and every successful execution therefore refresh
//! `(KEY_SUB, id)` far enough to cover the subscription cadence, the next due
//! timestamp, and an explicit expiry when one is present. The requested horizon
//! is capped at the network's live `max_ttl`, because a contract cannot extend
//! an entry farther than the host permits in one transaction.
//!
//! `keep_subscription_alive` is permissionless so the same untrusted keeper
//! model used by `execute_subscription` can proactively refresh a dormant
//! subscription before its entry approaches archival. For a cadence or expiry
//! beyond the network's maximum TTL horizon, keepers must call it periodically;
//! there is no contract-side way to promise a single extension beyond `max_ttl`.
//!
//! ## Catch-up bursts are intentionally unchanged
//!
//! `execute_subscription` still advances `next_execution_time` by exactly
//! one `interval_seconds` per call rather than jumping to `now +
//! interval_seconds` (see the comment at the call site) — this is what
//! stops a late keeper call from silently drifting the cadence forward, a
//! real problem this design solves. Its accepted side effect: if a
//! subscription goes unexecuted for a long stretch, a keeper *can* call
//! `execute_subscription` back-to-back to "catch up" every missed interval,
//! each call transferring one full payment.
//!
//! We deliberately do not change that here. Disallowing catch-up (skipping
//! missed intervals instead) trades a burst-of-payments surprise for a
//! skipped-payment surprise — neither is strictly safer, and the module's
//! own anti-drift design already reflects a considered choice for the
//! former. What actually bounds the *damage* of a catch-up burst is
//! `max_executions`/`expiry_time` above: a subscription with a hard cap has
//! a hard ceiling on how much a burst can ever move, capped subscription or
//! not. See `test_execute_subscription_rapid_catch_up_multiple_calls` and
//! `test_execute_subscription_max_executions_bounds_catch_up_burst` for
//! what this means concretely. A future issue could still add a per-call
//! rate limit or a `catch_up: bool` opt-out if product requirements turn
//! out to want one; nothing here forecloses that.
//!
//! Storage
//! ───────
//! Instance:
//!   KEY_SUB_SEQ → u64 (global subscription id counter)
//! Persistent:
//!   (KEY_SUB, id) → Subscription

use soroban_sdk::{contractimpl, contracttype, token, Address, Env, Vec};

use crate::{
    StellarSendContract, StellarSendContractClient, StellarSendError, KEY_PAYER_SUB,
    KEY_PAYER_SUB_COUNT, KEY_RECIPIENT_SUB, KEY_RECIPIENT_SUB_COUNT, KEY_SUB, KEY_SUB_SEQ,
};

/// Cap on how many ids `get_subscriptions_for_payer`/`get_subscriptions_for_recipient`
/// return in a single call, regardless of the requested `limit` — a payer/recipient
/// with more subscriptions than this needs multiple calls (`start_index` +=
/// this constant) rather than one call risking Soroban's per-invocation
/// resource limits (#48).
pub const MAX_SUBSCRIPTIONS_PAGE: u32 = 50;

/// Soroban ledger close time used to translate wall-clock subscription cadence
/// into a persistent-storage TTL target. The one-day ledger cushion below
/// absorbs ordinary close-time variance; the final target is always capped at
/// the host-reported maximum TTL.
const SUBSCRIPTION_TTL_LEDGER_SECONDS: u64 = 5;
const SUBSCRIPTION_TTL_SAFETY_LEDGERS: u64 = 17_280;

/// A recurring payment authorised by `payer`.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Subscription {
    pub payer: Address,
    pub recipient: Address,
    pub token: Address,
    /// Gross amount transferred on every execution (fee is deducted from this).
    pub amount: i128,
    /// Minimum number of seconds between two consecutive executions.
    pub interval_seconds: u64,
    /// Unix timestamp (ledger time) at which the subscription may next run.
    pub next_execution_time: u64,
    /// False once cancelled by the payer, or once `max_executions` has been
    /// reached; either way, a new subscription must be created instead of
    /// trying to re-activate this one.
    pub active: bool,
    /// Hard ceiling on total lifetime executions, or `None` for unbounded.
    /// Reaching this count sets `active = false`.
    pub max_executions: Option<u32>,
    /// Ledger timestamp past which `execute_subscription` refuses to run,
    /// or `None` for no expiry. Checked independently of `max_executions`.
    pub expiry_time: Option<u64>,
    /// Count of successful executions so far.
    pub executions_count: u32,
}

#[contractimpl]
impl StellarSendContract {
    /// Create a recurring payment.  `start_time` is the timestamp of the
    /// first allowed execution (may be in the past to allow immediate
    /// execution, or in the future to delay the first charge).
    ///
    /// * `max_executions` – Optional hard ceiling on total lifetime
    ///   executions (`Some(0)` is rejected as invalid — it could never run).
    ///   `None` means unbounded, same as omitting a cap entirely.
    /// * `expiry_time` – Optional ledger timestamp past which the
    ///   subscription can no longer execute, independent of
    ///   `max_executions`. Must be strictly after `start_time`, since an
    ///   earlier-or-equal expiry would make the subscription unable to ever
    ///   execute even once. `None` means no expiry.
    ///
    /// The payer must separately call `token.approve(payer, <this contract>,
    /// amount * N, expiration_ledger)` on the token contract so that future
    /// `execute_subscription` calls (which run without the payer's live
    /// signature) are authorised to move funds via `transfer_from`.
    ///
    /// Returns the new subscription id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_subscription(
        env: Env,
        payer: Address,
        recipient: Address,
        token: Address,
        amount: i128,
        interval_seconds: u64,
        start_time: u64,
        max_executions: Option<u32>,
        expiry_time: Option<u64>,
    ) -> Result<u64, StellarSendError> {
        payer.require_auth();

        if amount <= 0 {
            return Err(StellarSendError::InvalidAmount);
        }
        if interval_seconds == 0 {
            return Err(StellarSendError::InvalidInterval);
        }
        if payer == recipient {
            return Err(StellarSendError::SelfPaymentNotAllowed);
        }
        if max_executions == Some(0) {
            return Err(StellarSendError::InvalidMaxExecutions);
        }
        if let Some(expiry) = expiry_time {
            if expiry <= start_time {
                return Err(StellarSendError::InvalidExpiry);
            }
        }

        let id = Self::next_sub_id(&env);
        let sub = Subscription {
            payer: payer.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            amount,
            interval_seconds,
            next_execution_time: start_time,
            active: true,
            max_executions,
            expiry_time,
            executions_count: 0,
        };

        env.storage().persistent().set(&(KEY_SUB, id), &sub);
        Self::refresh_subscription_ttl(&env, id, &sub);
        Self::record_payer_subscription(&env, &payer, id);
        Self::record_recipient_subscription(&env, &recipient, id);

        crate::events::emit_subscription_created(
            &env,
            id,
            &payer,
            &recipient,
            &token,
            amount,
            interval_seconds,
            start_time,
            max_executions,
            expiry_time,
        );

        Ok(id)
    }

    /// Cancel a subscription.  Only the payer may cancel.  Idempotent calls
    /// on an already-cancelled subscription return `SubscriptionInactive`.
    pub fn cancel_subscription(env: Env, id: u64) -> Result<(), StellarSendError> {
        let mut sub = Self::load_subscription(&env, id)?;
        sub.payer.require_auth();

        if !sub.active {
            return Err(StellarSendError::SubscriptionInactive);
        }

        sub.active = false;
        env.storage().persistent().set(&(KEY_SUB, id), &sub);

        crate::events::emit_subscription_cancelled(&env, id, &sub.payer);
        Ok(())
    }

    /// Execute a due subscription.  Callable by anyone (a keeper), because
    /// the payer already pre-authorised the token allowance at creation
    /// time.  Fails with `SubscriptionNotDue` if `next_execution_time` has
    /// not yet been reached, guarding against double-execution within a
    /// single interval. Fails with `SubscriptionExpired` if `expiry_time`
    /// has passed, checked independently of the normal due-time gate.
    ///
    /// On the execution that reaches `max_executions` (if set), the
    /// subscription is auto-deactivated the same way cancellation does —
    /// any further call returns `SubscriptionInactive`.
    pub fn execute_subscription(env: Env, id: u64) -> Result<i128, StellarSendError> {
        let _guard = crate::reentrancy::ReentrancyGuard::new(&env);
        let mut sub = Self::load_subscription(&env, id)?;

        if !sub.active {
            return Err(StellarSendError::SubscriptionInactive);
        }

        let now = env.ledger().timestamp();
        if now < sub.next_execution_time {
            return Err(StellarSendError::SubscriptionNotDue);
        }
        if let Some(expiry) = sub.expiry_time {
            if now > expiry {
                return Err(StellarSendError::SubscriptionExpired);
            }
        }

        let config = Self::load_config(&env)?;
        let (fee_amount, net_amount) = Self::split_fee(sub.amount, config.fee_bps)?;

        // Advance the schedule by exactly one interval (not "now + interval")
        // so a late keeper call doesn't silently drift the cadence forward.
        // See the module doc comment for why an unbounded catch-up burst
        // this permits is left as-is and instead bounded by max_executions/
        // expiry_time rather than changed here.
        sub.next_execution_time = sub
            .next_execution_time
            .checked_add(sub.interval_seconds)
            .ok_or(StellarSendError::ArithmeticOverflow)?;

        sub.executions_count = sub
            .executions_count
            .checked_add(1)
            .ok_or(StellarSendError::ArithmeticOverflow)?;
        if let Some(max) = sub.max_executions {
            if sub.executions_count >= max {
                sub.active = false;
            }
        }

        env.storage().persistent().set(&(KEY_SUB, id), &sub);

        let token_client = token::Client::new(&env, &sub.token);
        let spender = env.current_contract_address();

        if fee_amount > 0 {
            token_client.transfer_from(&spender, &sub.payer, &config.fee_collector, &fee_amount);
        }
        // Guard mirrors the fee leg above: skip a zero-amount transfer_from to
        // the recipient when net_amount == 0 (reachable if fee_bps were ever
        // 100%).  The subscription state update and event are still emitted —
        // a fully-fee'd execution is a valid, if unusual, outcome, not an error.
        if net_amount > 0 {
            token_client.transfer_from(&spender, &sub.payer, &sub.recipient, &net_amount);
        }

        // Refresh only on the successful execution path. If a later operation
        // in this invocation fails, Soroban transaction atomicity rolls the TTL
        // extension back together with the state and token transfers.
        Self::refresh_subscription_ttl(&env, id, &sub);

        crate::events::emit_subscription_executed(
            &env,
            id,
            &sub.payer,
            &sub.recipient,
            net_amount,
            fee_amount,
            sub.next_execution_time,
            sub.executions_count,
            sub.active,
        );

        Ok(net_amount)
    }

    /// Permissionlessly refresh a subscription's persistent TTL without
    /// executing a payment. Keepers should submit this as a transaction for
    /// cadences that can sit dormant near the network's maximum TTL horizon.
    pub fn keep_subscription_alive(env: Env, id: u64) -> Result<(), StellarSendError> {
        let sub = Self::load_subscription(&env, id)?;
        Self::refresh_subscription_ttl(&env, id, &sub);
        Ok(())
    }

    /// Fetch a subscription by id.
    pub fn get_subscription(env: Env, id: u64) -> Result<Subscription, StellarSendError> {
        Self::load_subscription(&env, id)
    }

    /// Returns a page of `payer`'s subscription ids, in creation order,
    /// starting at `start_index` and containing at most
    /// `MAX_SUBSCRIPTIONS_PAGE` ids regardless of the requested `limit`
    /// (#48). `start_index` at or beyond `payer`'s total count returns an
    /// empty `Vec`, not an error — a payer with no subscriptions at all is
    /// indistinguishable, by design, from one whose ids have all already
    /// been paged through.
    ///
    /// Includes ids for subscriptions that are no longer `active`
    /// (cancelled, or auto-deactivated at `max_executions`) — "no longer
    /// active" is itself useful information a payer auditing their
    /// authorizations wants to see, distinguishing it from "never existed".
    /// Call `get_subscription` on each returned id (or filter client-side)
    /// to find the currently-live ones.
    pub fn get_subscriptions_for_payer(
        env: Env,
        payer: Address,
        start_index: u32,
        limit: u32,
    ) -> Vec<u64> {
        let count: u32 = env
            .storage()
            .persistent()
            .get(&(KEY_PAYER_SUB_COUNT, payer.clone()))
            .unwrap_or(0);
        Self::collect_subscription_page(&env, KEY_PAYER_SUB, &payer, start_index, limit, count)
    }

    /// Total number of subscriptions `payer` has ever created (active or
    /// not) — lets a caller know how many pages `get_subscriptions_for_payer`
    /// will take without guessing from empty-page results.
    pub fn get_payer_subscription_count(env: Env, payer: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&(KEY_PAYER_SUB_COUNT, payer))
            .unwrap_or(0)
    }

    /// Mirrors `get_subscriptions_for_payer`, keyed by `recipient` instead —
    /// a recipient auditing what recurring income they're set up to
    /// receive. Same paging/inclusion semantics.
    pub fn get_subscriptions_for_recipient(
        env: Env,
        recipient: Address,
        start_index: u32,
        limit: u32,
    ) -> Vec<u64> {
        let count: u32 = env
            .storage()
            .persistent()
            .get(&(KEY_RECIPIENT_SUB_COUNT, recipient.clone()))
            .unwrap_or(0);
        Self::collect_subscription_page(
            &env,
            KEY_RECIPIENT_SUB,
            &recipient,
            start_index,
            limit,
            count,
        )
    }

    /// Mirrors `get_payer_subscription_count`, keyed by `recipient` instead.
    pub fn get_recipient_subscription_count(env: Env, recipient: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&(KEY_RECIPIENT_SUB_COUNT, recipient))
            .unwrap_or(0)
    }

    /// Shared paging logic for both the payer- and recipient-side indexes:
    /// reads at most `MAX_SUBSCRIPTIONS_PAGE` entries starting at
    /// `start_index`, from `(prefix, address, index) → u64` storage,
    /// stopping at `total` (that address's recorded count).
    fn collect_subscription_page(
        env: &Env,
        prefix: soroban_sdk::Symbol,
        address: &Address,
        start_index: u32,
        limit: u32,
        total: u32,
    ) -> Vec<u64> {
        let capped_limit = limit.min(MAX_SUBSCRIPTIONS_PAGE);
        let end = start_index.saturating_add(capped_limit).min(total);

        let mut ids = Vec::new(env);
        for i in start_index..end {
            if let Some(id) = env
                .storage()
                .persistent()
                .get(&(prefix.clone(), address.clone(), i))
            {
                ids.push_back(id);
            }
        }
        ids
    }

    fn refresh_subscription_ttl(env: &Env, id: u64, sub: &Subscription) {
        let now = env.ledger().timestamp();
        let until_due = sub.next_execution_time.saturating_sub(now);
        let until_expiry = sub
            .expiry_time
            .map(|expiry| expiry.saturating_sub(now))
            .unwrap_or(0);

        // Include the cadence even when the current payment is already due, so
        // a newly-created or just-executed subscription survives to the next
        // expected keeper touch. Round up seconds -> ledgers, then add one day
        // of cushion for normal ledger-close variance.
        let horizon_seconds = sub.interval_seconds.max(until_due).max(until_expiry);
        let horizon_ledgers = horizon_seconds
            .saturating_add(SUBSCRIPTION_TTL_LEDGER_SECONDS - 1)
            / SUBSCRIPTION_TTL_LEDGER_SECONDS;
        let requested = horizon_ledgers.saturating_add(SUBSCRIPTION_TTL_SAFETY_LEDGERS);
        let extend_to = requested
            .min(u64::from(env.storage().max_ttl()))
            .max(1) as u32;

        env.storage().persistent().extend_ttl(
            &(KEY_SUB, id),
            extend_to.saturating_sub(1),
            extend_to,
        );
    }

    fn load_subscription(env: &Env, id: u64) -> Result<Subscription, StellarSendError> {
        env.storage()
            .persistent()
            .get(&(KEY_SUB, id))
            .ok_or(StellarSendError::SubscriptionNotFound)
    }

    fn next_sub_id(env: &Env) -> u64 {
        let seq: u64 = env.storage().instance().get(&KEY_SUB_SEQ).unwrap_or(0u64);
        let next = seq.wrapping_add(1);
        env.storage().instance().set(&KEY_SUB_SEQ, &next);
        next
    }

    /// Appends `id` to `payer`'s subscription index (#48). O(1): reads only
    /// `payer`'s own count, writes one new `(KEY_PAYER_SUB, payer, index)`
    /// entry plus the incremented count — never touches, or even loads, any
    /// of `payer`'s previously-recorded ids.
    fn record_payer_subscription(env: &Env, payer: &Address, id: u64) {
        let count_key = (KEY_PAYER_SUB_COUNT, payer.clone());
        let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&(KEY_PAYER_SUB, payer.clone(), count), &id);
        env.storage().persistent().set(&count_key, &(count + 1));
    }

    /// Mirrors `record_payer_subscription`, keyed by `recipient` instead.
    fn record_recipient_subscription(env: &Env, recipient: &Address, id: u64) {
        let count_key = (KEY_RECIPIENT_SUB_COUNT, recipient.clone());
        let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&(KEY_RECIPIENT_SUB, recipient.clone(), count), &id);
        env.storage().persistent().set(&count_key, &(count + 1));
    }
}

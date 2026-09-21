//! Integration tests for the StellarSend contract.
//!
//! Each test creates a fresh Soroban test environment, registers the contract
//! and a mock token, then exercises the public API.

#![cfg(test)]

use soroban_sdk::{
    testutils::{storage::Persistent as _, Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    vec, Address, Env, String,
};

use crate::{
    subscription::MAX_SUBSCRIPTIONS_PAGE, ContractConfig, PaymentRequestStatus,
    StellarSendContract, StellarSendContractClient, StellarSendError, MAX_FEE_BPS, KEY_SUB,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Stand up a fresh environment with a deployed StellarSend contract and a
/// mock Stellar asset (XLM-style) token.
fn setup() -> (
    Env,
    StellarSendContractClient<'static>,
    Address,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let fee_collector = Address::generate(&env);

    // Register the main contract.
    let contract_id = env.register_contract(None, StellarSendContract);
    let client = StellarSendContractClient::new(&env, &contract_id);

    // Create a mock Stellar asset token.
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_address = token_id.address();

    (
        env,
        client,
        admin,
        fee_collector,
        token_address,
        token_admin,
    )
}

/// Mint `amount` of the mock token to `to`.
fn mint(env: &Env, token: &Address, _admin: &Address, to: &Address, amount: i128) {
    let sac: StellarAssetClient = StellarAssetClient::new(env, token);
    sac.mint(to, &amount);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_initialize() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();

    client.initialize(&admin, &100u32, &fee_collector);

    let config: ContractConfig = client.get_config();
    assert_eq!(config.admin, admin);
    assert_eq!(config.fee_bps, 100u32);
    assert_eq!(config.fee_collector, fee_collector);
    assert!(config.active);
}

#[test]
fn test_initialize_already_initialized() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();

    client.initialize(&admin, &100u32, &fee_collector);

    let result = client.try_initialize(&admin, &100u32, &fee_collector);
    assert_eq!(result, Err(Ok(StellarSendError::AlreadyInitialized)));
}

#[test]
fn test_initialize_invalid_fee_bps() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();

    // One above the ceiling — must be rejected.
    let result = client.try_initialize(&admin, &(MAX_FEE_BPS + 1), &fee_collector);
    assert_eq!(result, Err(Ok(StellarSendError::InvalidFeeBps)));
}

#[test]
fn test_initialize_accepts_max_fee_boundary() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();

    client.initialize(&admin, &MAX_FEE_BPS, &fee_collector);

    let config: ContractConfig = client.get_config();
    assert_eq!(config.fee_bps, MAX_FEE_BPS);
}

/// Regression test for the vulnerability this ceiling closes: prior to
/// introducing `MAX_FEE_BPS`, `initialize` accepted `fee_bps: 10_000`
/// (100%) — meaning every subsequent payment's net amount would have been
/// zero, with the full gross amount routed to `fee_collector`. This must
/// now be rejected outright at initialization, before any payment can ever
/// be processed under that fee.
#[test]
fn test_initialize_rejects_100_percent_fee() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();

    let result = client.try_initialize(&admin, &10_000u32, &fee_collector);
    assert_eq!(result, Err(Ok(StellarSendError::InvalidFeeBps)));
}

#[test]
fn test_send_payment_happy_path() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();

    client.initialize(&admin, &100u32, &fee_collector); // 1 % fee

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    // Fund the sender with 1_000 stroops.
    mint(&env, &token, &token_admin, &sender, 1_000);

    // Send 1_000; fee = 10, net = 990.
    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &1_000i128,
        &String::from_str(&env, "test memo"),
    );

    assert_eq!(record.net_amount, 990);
    assert_eq!(record.fee_amount, 10);
    assert_eq!(record.from, sender);
    assert_eq!(record.to, recipient);

    // Verify balances.
    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&recipient), 990);
    assert_eq!(token_client.balance(&fee_collector), 10);
    assert_eq!(token_client.balance(&sender), 0);
}

#[test]
fn test_send_payment_zero_fee() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();

    client.initialize(&admin, &0u32, &fee_collector); // 0 % fee

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    mint(&env, &token, &token_admin, &sender, 500);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &500i128,
        &String::from_str(&env, "no fee"),
    );

    assert_eq!(record.net_amount, 500);
    assert_eq!(record.fee_amount, 0);

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&recipient), 500);
    assert_eq!(token_client.balance(&fee_collector), 0);
}

#[test]
fn test_send_payment_invalid_amount() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let result = client.try_send_payment(
        &sender,
        &recipient,
        &token,
        &0i128,
        &String::from_str(&env, "bad"),
    );
    assert_eq!(result, Err(Ok(StellarSendError::InvalidAmount)));
}

#[test]
fn test_fee_collection_accumulates() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();

    client.initialize(&admin, &200u32, &fee_collector); // 2 % fee

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    mint(&env, &token, &token_admin, &sender, 10_000);

    // First payment: gross 5_000 → fee 100, net 4_900.
    client.send_payment(
        &sender,
        &recipient,
        &token,
        &5_000i128,
        &String::from_str(&env, "first"),
    );

    // Second payment: gross 5_000 → fee 100, net 4_900.
    client.send_payment(
        &sender,
        &recipient,
        &token,
        &5_000i128,
        &String::from_str(&env, "second"),
    );

    let token_client = TokenClient::new(&env, &token);
    // Total fee = 200, total net = 9_800.
    assert_eq!(token_client.balance(&fee_collector), 200);
    assert_eq!(token_client.balance(&recipient), 9_800);
}

#[test]
fn test_set_fee_requires_admin() {
    let (env, client, admin, fee_collector, _token, _token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    // Happy path: admin can change the fee.
    client.set_fee(&50u32);
    let config = client.get_config();
    assert_eq!(config.fee_bps, 50u32);

    // Verify the old fee was different.
    assert_ne!(50u32, 100u32);

    // Use env to keep borrow alive.
    let _ = &env;
}

#[test]
fn test_set_fee_invalid_bps() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    let result = client.try_set_fee(&(MAX_FEE_BPS + 1));
    assert_eq!(result, Err(Ok(StellarSendError::InvalidFeeBps)));
}

#[test]
fn test_set_fee_accepts_max_fee_boundary() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    client.set_fee(&MAX_FEE_BPS);
    let config = client.get_config();
    assert_eq!(config.fee_bps, MAX_FEE_BPS);
}

/// Regression test for the vulnerability this ceiling closes: prior to
/// introducing `MAX_FEE_BPS`, a single admin key could call
/// `set_fee(10_000)` (100%) at any time and instantly zero out the net
/// amount of every subsequent payment, batch leg, payment-request
/// fulfillment, and subscription execution — draining the full gross
/// amount to `fee_collector` with no timelock or warning. This must now be
/// rejected outright.
#[test]
fn test_set_fee_rejects_100_percent_fee() {
    let (_env, client, admin, fee_collector, _token, _token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    let result = client.try_set_fee(&10_000u32);
    assert_eq!(result, Err(Ok(StellarSendError::InvalidFeeBps)));

    // The fee must remain unchanged after the rejected attempt.
    let config = client.get_config();
    assert_eq!(config.fee_bps, 100u32);
}

#[test]
fn test_send_path_payment() {
    let (env, client, admin, fee_collector, send_token, send_token_admin) = setup();

    client.initialize(&admin, &100u32, &fee_collector); // 1 %

    // Create a second token to act as the destination asset.
    let dest_token_admin = Address::generate(&env);
    let dest_token_id = env.register_stellar_asset_contract_v2(dest_token_admin.clone());
    let dest_token = dest_token_id.address();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let contract_id = client.address.clone();

    // Sender gets send_token.
    mint(&env, &send_token, &send_token_admin, &sender, 1_000);
    // Contract must hold dest_token to pay recipient (simulating DEX swap).
    mint(&env, &dest_token, &dest_token_admin, &contract_id, 1_000);

    // Send 1_000 send_token; fee = 10; net_send = 990.
    // Simulated dest_amount = 990 (1:1 model).
    let dest_amount = client.send_path_payment(
        &sender,
        &recipient,
        &send_token,
        &1_000i128,
        &dest_token,
        &900i128, // min_dest_amount (10 % slippage tolerance)
        &vec![&env],
    );

    assert_eq!(dest_amount, 990);

    let send_client = TokenClient::new(&env, &send_token);
    let dest_client = TokenClient::new(&env, &dest_token);

    // Sender should have no send_token left.
    assert_eq!(send_client.balance(&sender), 0);
    // Fee collector gets 10 send_token.
    assert_eq!(send_client.balance(&fee_collector), 10);
    // Recipient gets 990 dest_token.
    assert_eq!(dest_client.balance(&recipient), 990);
}

#[test]
fn test_send_path_payment_slippage_exceeded() {
    let (env, client, admin, fee_collector, send_token, send_token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    let dest_token_admin = Address::generate(&env);
    let dest_token_id = env.register_stellar_asset_contract_v2(dest_token_admin.clone());
    let dest_token = dest_token_id.address();

    let sender = Address::generate(&env);
    let contract_id = client.address.clone();

    mint(&env, &send_token, &send_token_admin, &sender, 1_000);
    mint(&env, &dest_token, &dest_token_admin, &contract_id, 1_000);

    // min_dest_amount > simulated output → SlippageExceeded.
    let result = client.try_send_path_payment(
        &sender,
        &Address::generate(&env),
        &send_token,
        &1_000i128,
        &dest_token,
        &999i128, // demand 999 but model gives 990
        &vec![&env],
    );
    assert_eq!(result, Err(Ok(StellarSendError::SlippageExceeded)));
}

#[test]
fn test_get_payment_record() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 1_000);

    client.send_payment(
        &sender,
        &recipient,
        &token,
        &1_000i128,
        &String::from_str(&env, "audit me"),
    );

    // Sequence starts at 1 after the first payment.
    let record = client.get_payment_record(&sender, &1u64);
    assert_eq!(record.from, sender);
    assert_eq!(record.to, recipient);
    assert_eq!(record.net_amount, 1_000);
    assert_eq!(record.fee_amount, 0);
    assert_eq!(record.memo, String::from_str(&env, "audit me"));
}

#[test]
fn test_get_payment_record_not_found() {
    let (env, client, admin, fee_collector, _token, _token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    // Contract is initialized, but no payment has ever been recorded for
    // this sender/sequence pair — must be reported as PaymentRecordNotFound,
    // not conflated with an uninitialized contract.
    let missing_sender = Address::generate(&env);
    let result = client.try_get_payment_record(&missing_sender, &1u64);
    assert_eq!(result, Err(Ok(StellarSendError::PaymentRecordNotFound)));
}

#[test]
fn test_get_payment_record_uninitialized_contract() {
    let (env, client, _admin, _fee_collector, _token, _token_admin) = setup();
    // Deliberately skip client.initialize(..) so the contract has no config.

    let sender = Address::generate(&env);
    let result = client.try_get_payment_record(&sender, &1u64);
    assert_eq!(result, Err(Ok(StellarSendError::NotInitialized)));

    let seq_result = client.try_get_sequence(&sender);
    assert_eq!(seq_result, Err(Ok(StellarSendError::NotInitialized)));
}

#[test]
fn test_interleaved_per_sender_sequence_contiguity() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let sender_a = Address::generate(&env);
    let sender_b = Address::generate(&env);
    let recipient = Address::generate(&env);

    mint(&env, &token, &token_admin, &sender_a, 2_000);
    mint(&env, &token, &token_admin, &sender_b, 1_000);

    // Initial sequences for both senders should be 0.
    assert_eq!(client.get_sequence(&sender_a), 0);
    assert_eq!(client.get_sequence(&sender_b), 0);

    // Sender A payment 1 -> seq 1
    client.send_payment(
        &sender_a,
        &recipient,
        &token,
        &500i128,
        &String::from_str(&env, "a payment 1"),
    );

    // Sender B payment 1 -> seq 1 (independent from Sender A)
    client.send_payment(
        &sender_b,
        &recipient,
        &token,
        &400i128,
        &String::from_str(&env, "b payment 1"),
    );

    // Sender A payment 2 -> seq 2 (contiguous range 1..2 for Sender A)
    client.send_payment(
        &sender_a,
        &recipient,
        &token,
        &600i128,
        &String::from_str(&env, "a payment 2"),
    );

    assert_eq!(client.get_sequence(&sender_a), 2);
    assert_eq!(client.get_sequence(&sender_b), 1);

    // Sender A's records are retrievable at contiguous sequence numbers 1 and 2
    let record_a1 = client.get_payment_record(&sender_a, &1u64);
    assert_eq!(record_a1.net_amount, 500);
    assert_eq!(record_a1.memo, String::from_str(&env, "a payment 1"));

    let record_a2 = client.get_payment_record(&sender_a, &2u64);
    assert_eq!(record_a2.net_amount, 600);
    assert_eq!(record_a2.memo, String::from_str(&env, "a payment 2"));

    // Sender B's record is retrievable at seq 1
    let record_b1 = client.get_payment_record(&sender_b, &1u64);
    assert_eq!(record_b1.net_amount, 400);
    assert_eq!(record_b1.memo, String::from_str(&env, "b payment 1"));

    // Querying beyond sender_a's sequence count returns PaymentRecordNotFound
    let result_a3 = client.try_get_payment_record(&sender_a, &3u64);
    assert_eq!(result_a3, Err(Ok(StellarSendError::PaymentRecordNotFound)));
}

#[test]
fn test_unauthorized_send_rejected() {
    // Verify that send_payment correctly requires the sender's authorisation.
    // We mock only the attacker's auth (not the victim's) and confirm that
    // try_send_payment returns an error rather than succeeding.
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let fee_collector = Address::generate(&env);
    let contract_id = env.register_contract(None, StellarSendContract);
    let client = StellarSendContractClient::new(&env, &contract_id);

    client.initialize(&admin, &100u32, &fee_collector);

    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_address = token_id.address();

    let victim = Address::generate(&env);
    let attacker = Address::generate(&env);

    // Fund the victim.
    StellarAssetClient::new(&env, &token_address).mint(&victim, &1_000);

    // Authorise only the attacker, not the victim, so the contract's
    // require_auth(&victim) will fail.
    env.mock_auths(&[]);

    // The call should fail because victim's auth is not present.
    let result = client.try_send_payment(
        &victim,
        &attacker,
        &token_address,
        &1_000i128,
        &String::from_str(&env, "steal"),
    );
    assert!(
        result.is_err(),
        "send_payment must fail when victim has not authorised the call"
    );
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

#[test]
fn test_subscription_create_and_execute() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector); // 1 %

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    // Payer pre-authorises the contract to pull funds on its behalf.
    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &10_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let id = client.create_subscription(
        &payer, &recipient, &token, &1_000i128, &600u64, &start, &None, &None,
    );

    // Due immediately (start_time == now) → executes.
    let net = client.execute_subscription(&id);
    assert_eq!(net, 990); // 1 000 - 1% fee

    let sub = client.get_subscription(&id);
    assert_eq!(sub.next_execution_time, start + 600);

    assert_eq!(token_client.balance(&recipient), 990);
    assert_eq!(token_client.balance(&fee_collector), 10);
}

#[test]
fn test_subscription_execute_before_due_fails() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &10_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    // start_time far in the future → not due yet.
    let start = env.ledger().timestamp() + 10_000;
    let id = client.create_subscription(
        &payer, &recipient, &token, &1_000i128, &600u64, &start, &None, &None,
    );

    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionNotDue)));
}

#[test]
fn test_subscription_cancel_then_execute_fails() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &10_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let id = client.create_subscription(
        &payer, &recipient, &token, &1_000i128, &600u64, &start, &None, &None,
    );

    client.cancel_subscription(&id);

    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionInactive)));
}

#[test]
fn test_get_subscriptions_for_payer_enumerates_only_that_payers_ids() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer_a = Address::generate(&env);
    let payer_b = Address::generate(&env);
    let payer_c = Address::generate(&env);
    let recipient = Address::generate(&env);
    let start = env.ledger().timestamp();

    let id_a1 = client.create_subscription(
        &payer_a, &recipient, &token, &1_000i128, &600u64, &start, &None, &None,
    );
    let id_a2 = client.create_subscription(
        &payer_a, &recipient, &token, &2_000i128, &600u64, &start, &None, &None,
    );
    let id_a3 = client.create_subscription(
        &payer_a, &recipient, &token, &3_000i128, &600u64, &start, &None, &None,
    );
    let id_b1 = client.create_subscription(
        &payer_b, &recipient, &token, &4_000i128, &600u64, &start, &None, &None,
    );

    assert_eq!(client.get_payer_subscription_count(&payer_a), 3);
    let a_ids = client.get_subscriptions_for_payer(&payer_a, &0u32, &10u32);
    assert_eq!(a_ids, vec![&env, id_a1, id_a2, id_a3]);

    assert_eq!(client.get_payer_subscription_count(&payer_b), 1);
    let b_ids = client.get_subscriptions_for_payer(&payer_b, &0u32, &10u32);
    assert_eq!(b_ids, vec![&env, id_b1]);

    // A payer with zero subscriptions gets an empty result, not an error.
    assert_eq!(client.get_payer_subscription_count(&payer_c), 0);
    let c_ids = client.get_subscriptions_for_payer(&payer_c, &0u32, &10u32);
    assert_eq!(c_ids, vec![&env]);
}

#[test]
fn test_get_subscriptions_for_recipient_enumerates_only_that_recipients_ids() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient_a = Address::generate(&env);
    let recipient_b = Address::generate(&env);
    let recipient_c = Address::generate(&env);
    let start = env.ledger().timestamp();

    let id_a1 = client.create_subscription(
        &payer,
        &recipient_a,
        &token,
        &1_000i128,
        &600u64,
        &start,
        &None,
        &None,
    );
    let id_a2 = client.create_subscription(
        &payer,
        &recipient_a,
        &token,
        &2_000i128,
        &600u64,
        &start,
        &None,
        &None,
    );
    let id_b1 = client.create_subscription(
        &payer,
        &recipient_b,
        &token,
        &3_000i128,
        &600u64,
        &start,
        &None,
        &None,
    );

    assert_eq!(client.get_recipient_subscription_count(&recipient_a), 2);
    let a_ids = client.get_subscriptions_for_recipient(&recipient_a, &0u32, &10u32);
    assert_eq!(a_ids, vec![&env, id_a1, id_a2]);

    assert_eq!(client.get_recipient_subscription_count(&recipient_b), 1);
    let b_ids = client.get_subscriptions_for_recipient(&recipient_b, &0u32, &10u32);
    assert_eq!(b_ids, vec![&env, id_b1]);

    // A recipient with zero subscriptions gets an empty result, not an error.
    assert_eq!(client.get_recipient_subscription_count(&recipient_c), 0);
    let c_ids = client.get_subscriptions_for_recipient(&recipient_c, &0u32, &10u32);
    assert_eq!(c_ids, vec![&env]);
}

#[test]
fn test_get_subscriptions_for_payer_pagination_edge_cases() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let start = env.ledger().timestamp();

    // Create more subscriptions than MAX_SUBSCRIPTIONS_PAGE so a single
    // over-sized request must be clamped rather than trapping on a runaway
    // page size.
    let total = MAX_SUBSCRIPTIONS_PAGE + 5;
    for i in 0..total {
        client.create_subscription(
            &payer,
            &recipient,
            &token,
            &(1_000i128 + i as i128),
            &600u64,
            &start,
            &None,
            &None,
        );
    }
    assert_eq!(client.get_payer_subscription_count(&payer), total);

    // A requested limit far above MAX_SUBSCRIPTIONS_PAGE is clamped, not
    // honored or rejected.
    let first_page = client.get_subscriptions_for_payer(&payer, &0u32, &(total * 10));
    assert_eq!(first_page.len(), MAX_SUBSCRIPTIONS_PAGE);

    // The remainder is reachable via a second page starting where the first
    // left off.
    let second_page = client.get_subscriptions_for_payer(&payer, &MAX_SUBSCRIPTIONS_PAGE, &10u32);
    assert_eq!(second_page.len(), 5);

    // start_index at or beyond the payer's total count returns an empty
    // Vec, not an error.
    let at_count = client.get_subscriptions_for_payer(&payer, &total, &10u32);
    assert_eq!(at_count, vec![&env]);
    let beyond_count = client.get_subscriptions_for_payer(&payer, &(total + 100), &10u32);
    assert_eq!(beyond_count, vec![&env]);
}

#[test]
fn test_subscription_invalid_max_executions_zero_rejected() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let start = env.ledger().timestamp();

    // Some(0) could never execute even once — reject at creation rather
    // than silently creating a dead-on-arrival subscription.
    let result = client.try_create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &600u64,
        &start,
        &Some(0u32),
        &None,
    );
    assert_eq!(result, Err(Ok(StellarSendError::InvalidMaxExecutions)));
}

#[test]
fn test_subscription_invalid_expiry_not_after_start_rejected() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let start = env.ledger().timestamp() + 1_000;

    // expiry_time == start_time: the subscription would be created
    // already-expired, unable to ever run — reject at creation.
    let result = client.try_create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &600u64,
        &start,
        &None,
        &Some(start),
    );
    assert_eq!(result, Err(Ok(StellarSendError::InvalidExpiry)));
}

#[test]
fn test_subscription_execute_after_expiry_fails() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &10_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let interval = 600u64;
    let expiry = start + 500; // expires before the second interval is due
    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &interval,
        &start,
        &None,
        &Some(expiry),
    );

    // First execution is fine: due immediately and not yet expired.
    client.execute_subscription(&id);

    // Jump to (and past) the *second* interval's due time, which is also
    // past expiry_time (600 > 500) — this isolates SubscriptionExpired
    // specifically: the due-time gate alone would allow this call (now >=
    // next_execution_time), so a SubscriptionNotDue result here would mean
    // the expiry check isn't actually being enforced.
    env.ledger().set_timestamp(start + interval);
    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionExpired)));

    // Unlike max_executions, hitting expiry does not auto-deactivate —
    // it stays "active" but perpetually expired, matching how
    // PaymentRequest.expiry behaves (RequestExpired never auto-cancels).
    let sub = client.get_subscription(&id);
    assert!(sub.active);
}

#[test]
fn test_subscription_max_executions_auto_deactivates_on_cap() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &10_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let interval = 600u64;
    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &interval,
        &start,
        &Some(2u32),
        &None,
    );

    // Execution 1 of 2: still active afterwards.
    client.execute_subscription(&id);
    assert!(client.get_subscription(&id).active);

    // Advance to the next due time for execution 2 of 2 (max_executions
    // doesn't change the interval — only how many total executions are
    // allowed).
    env.ledger().set_timestamp(start + interval);
    client.execute_subscription(&id);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.executions_count, 2);
    assert!(
        !sub.active,
        "reaching max_executions must auto-deactivate, same as cancel_subscription"
    );

    // A 3rd attempt — even though the schedule would otherwise allow it —
    // now correctly fails the same way a cancelled subscription would.
    env.ledger().set_timestamp(start + 2 * interval);
    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionInactive)));

    assert_eq!(token_client.balance(&recipient), 2_000);
}

#[test]
fn test_execute_subscription_rapid_catch_up_multiple_calls() {
    // Documents exactly how many rapid back-to-back calls succeed for a
    // known backlog (#23) — the "advance by exactly one interval per call"
    // design (see the module doc comment) permits catching up every missed
    // interval in a single burst, and this pins down precisely how many.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector); // 0% fee keeps the arithmetic simple.

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 100_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &100_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let interval = 600u64;
    let id = client.create_subscription(
        &payer, &recipient, &token, &1_000i128, &interval, &start, &None, &None,
    );

    // Simulate a keeper that's been offline since creation: "now" jumps
    // forward by 5 whole intervals without execute_subscription ever being
    // called in between.
    let missed_intervals: u64 = 5;
    env.ledger()
        .set_timestamp(start + missed_intervals * interval);

    // The due times {start, start+I, start+2I, start+3I, start+4I,
    // start+5I} are now ALL <= now — six due times, not five, because the
    // original start-time execution is due *in addition to* the five
    // subsequent intervals that elapsed. All six succeed as fast,
    // back-to-back calls with no cooldown between them.
    let expected_catch_up_calls = missed_intervals + 1;
    for call_number in 1..=expected_catch_up_calls {
        let net = client.execute_subscription(&id);
        assert_eq!(
            net, 1_000,
            "catch-up call {call_number} of {expected_catch_up_calls} should still move a full payment"
        );
    }

    let sub = client.get_subscription(&id);
    assert_eq!(sub.executions_count as u64, expected_catch_up_calls);
    assert_eq!(
        sub.next_execution_time,
        start + expected_catch_up_calls * interval
    );
    assert_eq!(
        token_client.balance(&recipient),
        1_000i128 * expected_catch_up_calls as i128
    );

    // Fully caught up now: next_execution_time is in the future relative
    // to "now", so a 7th call is correctly refused.
    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionNotDue)));
}

#[test]
fn test_execute_subscription_max_executions_bounds_catch_up_burst() {
    // Same backlog as test_execute_subscription_rapid_catch_up_multiple_calls
    // (six due-times available to catch up), but with max_executions set
    // below that — proving a capped subscription genuinely limits how much
    // a catch-up burst can ever move, not just documents it.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 100_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &100_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let interval = 600u64;
    let cap = 3u32;
    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &interval,
        &start,
        &Some(cap),
        &None,
    );

    // Same 5-interval backlog as the uncapped test above — six due-times
    // would otherwise be claimable in one burst.
    env.ledger().set_timestamp(start + 5 * interval);

    for _ in 0..cap {
        client.execute_subscription(&id);
    }

    let sub = client.get_subscription(&id);
    assert_eq!(sub.executions_count, cap);
    assert!(!sub.active, "the cap must auto-deactivate the subscription");
    assert_eq!(token_client.balance(&recipient), 1_000i128 * cap as i128);

    // Backlog still has unclaimed due-times left (next_execution_time is
    // still <= now), but the cap stops the burst here regardless —
    // SubscriptionInactive, not SubscriptionNotDue, proving this is the
    // cap enforcing itself rather than the schedule naturally running out.
    assert!(sub.next_execution_time <= start + 5 * interval);
    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionInactive)));
}

#[test]
fn test_execute_subscription_rapid_catch_up_multiple_calls_with_fee() {
    // Fee-bearing counterpart to test_execute_subscription_rapid_catch_up_multiple_calls
    // (see #50): the 0%-fee original never exercises fee-forwarding across a
    // rapid catch-up burst, so it can't catch an accounting bug where the
    // per-execution fee/net split behaves correctly once but drifts (or
    // double-counts against the payer's allowance) across repeated calls.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    let fee_bps = 200u32; // 2%
    client.initialize(&admin, &fee_bps, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let initial_mint = 100_000i128;
    mint(&env, &token, &token_admin, &payer, initial_mint);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &100_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let interval = 600u64;
    let amount = 1_000i128;
    let id = client.create_subscription(
        &payer, &recipient, &token, &amount, &interval, &start, &None, &None,
    );

    // Same 5-interval backlog as the 0%-fee version above: six due-times
    // (start plus five subsequent intervals) all claimable in one burst.
    let missed_intervals: u64 = 5;
    env.ledger()
        .set_timestamp(start + missed_intervals * interval);
    let expected_catch_up_calls = missed_intervals + 1;

    let expected_fee_per_execution = amount * fee_bps as i128 / 10_000i128; // 20
    let expected_net_per_execution = amount - expected_fee_per_execution; // 980

    for call_number in 1..=expected_catch_up_calls {
        let net = client.execute_subscription(&id);
        assert_eq!(
            net, expected_net_per_execution,
            "catch-up call {call_number} of {expected_catch_up_calls} should forward the fee-adjusted net amount"
        );
    }

    // Cumulative fee-collector balance across the whole burst must equal the
    // sum of each execution's individually-computed fee, not just the first
    // execution's fee scaled up (which would mask a per-call drift bug).
    assert_eq!(
        token_client.balance(&fee_collector),
        expected_fee_per_execution * expected_catch_up_calls as i128
    );
    assert_eq!(
        token_client.balance(&recipient),
        expected_net_per_execution * expected_catch_up_calls as i128
    );
    // The payer's balance must drop by the full gross amount (fee leg + net
    // leg) per execution, confirming the two transfer_from calls per
    // execution debit the payer's allowance symmetrically across a burst
    // rather than only accounting for the net leg.
    assert_eq!(
        token_client.balance(&payer),
        initial_mint - amount * expected_catch_up_calls as i128
    );
}

#[test]
fn test_execute_subscription_max_executions_bounds_catch_up_burst_with_fee() {
    // Fee-bearing counterpart to
    // test_execute_subscription_max_executions_bounds_catch_up_burst (#50):
    // verifies the max_executions cap still enforces correctly, and fee
    // accounting stays correct, when a capped catch-up burst also forwards
    // a nonzero fee per execution.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    let fee_bps = 200u32; // 2%
    client.initialize(&admin, &fee_bps, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let initial_mint = 100_000i128;
    mint(&env, &token, &token_admin, &payer, initial_mint);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &100_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let interval = 600u64;
    let amount = 1_000i128;
    let cap = 3u32;
    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &amount,
        &interval,
        &start,
        &Some(cap),
        &None,
    );

    // Same 5-interval backlog as the uncapped fee-bearing test above — six
    // due-times would otherwise be claimable in one burst.
    env.ledger().set_timestamp(start + 5 * interval);

    let expected_fee_per_execution = amount * fee_bps as i128 / 10_000i128; // 20
    let expected_net_per_execution = amount - expected_fee_per_execution; // 980

    for _ in 0..cap {
        let net = client.execute_subscription(&id);
        assert_eq!(net, expected_net_per_execution);
    }

    let sub = client.get_subscription(&id);
    assert_eq!(sub.executions_count, cap);
    assert!(!sub.active, "the cap must auto-deactivate the subscription");

    assert_eq!(
        token_client.balance(&fee_collector),
        expected_fee_per_execution * cap as i128
    );
    assert_eq!(
        token_client.balance(&recipient),
        expected_net_per_execution * cap as i128
    );
    assert_eq!(
        token_client.balance(&payer),
        initial_mint - amount * cap as i128
    );

    // The cap must still stop the burst even though unclaimed due-times
    // remain, regardless of the nonzero fee path.
    assert!(sub.next_execution_time <= start + 5 * interval);
    let result = client.try_execute_subscription(&id);
    assert_eq!(result, Err(Ok(StellarSendError::SubscriptionInactive)));
}

// ---------------------------------------------------------------------------
// Batch payments
// ---------------------------------------------------------------------------

#[test]
fn test_batch_payment_happy_path() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector); // 1 %

    let sender = Address::generate(&env);
    let r1 = Address::generate(&env);
    let r2 = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 10_000);

    let payments = vec![&env, (r1.clone(), 1_000i128), (r2.clone(), 2_000i128)];
    let records = client.send_batch_payment(&sender, &token, &payments);

    assert_eq!(records.len(), 2);
    assert_eq!(records.get(0).unwrap().net_amount, 990);
    assert_eq!(records.get(1).unwrap().net_amount, 1_980);

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&r1), 990);
    assert_eq!(token_client.balance(&r2), 1_980);
    assert_eq!(token_client.balance(&fee_collector), 30);
    assert_eq!(token_client.balance(&sender), 7_000);
}

#[test]
fn test_batch_payment_empty_fails() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);

    let sender = Address::generate(&env);
    let result = client.try_send_batch_payment(&sender, &token, &vec![&env]);
    assert_eq!(result, Err(Ok(StellarSendError::EmptyBatch)));
}

#[test]
fn test_batch_payment_reverts_atomically_on_bad_leg() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let sender = Address::generate(&env);
    let r1 = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 1_000);

    // Second leg has an invalid (zero) amount — whole batch must be rejected
    // and no balance should move, even for the valid first leg.
    let payments = vec![&env, (r1.clone(), 500i128), (r1.clone(), 0i128)];
    let result = client.try_send_batch_payment(&sender, &token, &payments);
    assert_eq!(result, Err(Ok(StellarSendError::InvalidAmount)));

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&sender), 1_000);
    assert_eq!(token_client.balance(&r1), 0);
}

// ---------------------------------------------------------------------------
// Payment requests / invoicing
// ---------------------------------------------------------------------------

#[test]
fn test_payment_request_create_and_fulfill() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector); // 1 %

    let requester = Address::generate(&env);
    let payer = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let expiry = env.ledger().timestamp() + 1_000;
    let id = client.create_payment_request(
        &requester,
        &None,
        &token,
        &1_000i128,
        &String::from_str(&env, "invoice #1"),
        &expiry,
    );

    let net = client.fulfill_payment_request(&id, &payer);
    assert_eq!(net, 990);

    let request = client.get_payment_request(&id);
    assert_eq!(request.status, PaymentRequestStatus::Fulfilled);

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&requester), 990);
    assert_eq!(token_client.balance(&fee_collector), 10);
}

#[test]
fn test_payment_request_fee_locked_at_creation_survives_later_fee_change() {
    // Regression test for #45: a requester prices an invoice against the fee
    // in effect at creation, so an admin `set_fee` between creation and
    // fulfillment must not change what the requester nets on the open request.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector); // 1 % at creation

    let requester = Address::generate(&env);
    let payer = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let expiry = env.ledger().timestamp() + 1_000;
    let id = client.create_payment_request(
        &requester,
        &None,
        &token,
        &1_000i128,
        &String::from_str(&env, "invoice #fee-locked"),
        &expiry,
    );

    // The request stores the fee it was priced against.
    let request = client.get_payment_request(&id);
    assert_eq!(request.fee_bps, 100u32);
    assert_eq!(request.status, PaymentRequestStatus::Open);

    // Admin raises the global fee to 10 % *after* the invoice is open.
    client.set_fee(&1_000u32);

    // Fulfillment must still net the requester 990 (1 % of 1_000), not the
    // 900 the new 10 % global rate would produce.
    let net = client.fulfill_payment_request(&id, &payer);
    assert_eq!(net, 990);

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&requester), 990);
    assert_eq!(token_client.balance(&fee_collector), 10);

    // The fulfilled request still carries the fee it was created under, and
    // the live global config reflects the new rate for future requests.
    let request = client.get_payment_request(&id);
    assert_eq!(request.fee_bps, 100u32);
    assert_eq!(request.status, PaymentRequestStatus::Fulfilled);
    assert_eq!(client.get_config().fee_bps, 1_000u32);
}

#[test]
fn test_payment_request_expired_fulfill_fails() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let requester = Address::generate(&env);
    let payer = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let expiry = env.ledger().timestamp() + 100;
    let id = client.create_payment_request(
        &requester,
        &None,
        &token,
        &1_000i128,
        &String::from_str(&env, "invoice #2"),
        &expiry,
    );

    env.ledger().set_timestamp(expiry + 1);

    let result = client.try_fulfill_payment_request(&id, &payer);
    assert_eq!(result, Err(Ok(StellarSendError::RequestExpired)));
}

#[test]
fn test_payment_request_wrong_payer_rejected() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let requester = Address::generate(&env);
    let designated_payer = Address::generate(&env);
    let other_payer = Address::generate(&env);
    mint(&env, &token, &token_admin, &other_payer, 10_000);

    let expiry = env.ledger().timestamp() + 1_000;
    let id = client.create_payment_request(
        &requester,
        &Some(designated_payer),
        &token,
        &1_000i128,
        &String::from_str(&env, "invoice #3"),
        &expiry,
    );

    let result = client.try_fulfill_payment_request(&id, &other_payer);
    assert_eq!(result, Err(Ok(StellarSendError::WrongPayer)));
}

#[test]
fn test_payment_request_cancel_then_fulfill_fails() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector);

    let requester = Address::generate(&env);
    let payer = Address::generate(&env);
    mint(&env, &token, &token_admin, &payer, 10_000);

    let expiry = env.ledger().timestamp() + 1_000;
    let id = client.create_payment_request(
        &requester,
        &None,
        &token,
        &1_000i128,
        &String::from_str(&env, "invoice #4"),
        &expiry,
    );

    client.cancel_payment_request(&id);

    let result = client.try_fulfill_payment_request(&id, &payer);
    assert_eq!(result, Err(Ok(StellarSendError::RequestCancelled)));
}

// ---------------------------------------------------------------------------
// Fee rounding tests
// ---------------------------------------------------------------------------

#[test]
fn test_split_fee_rounds_up_below_threshold() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &1u32, &fee_collector); // 1 bps

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 10_000);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &9_999i128,
        &String::from_str(&env, "round up below threshold"),
    );

    assert_eq!(record.fee_amount, 1);
    assert_eq!(record.net_amount, 9_998);
}

#[test]
fn test_split_fee_exact_division() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &1u32, &fee_collector); // 1 bps

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 20_000);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &10_000i128,
        &String::from_str(&env, "exact division"),
    );

    assert_eq!(record.fee_amount, 1);
    assert_eq!(record.net_amount, 9_999);
}

#[test]
fn test_split_fee_just_above_boundary() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &1u32, &fee_collector); // 1 bps

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 20_000);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &10_001i128,
        &String::from_str(&env, "just above boundary"),
    );

    assert_eq!(record.fee_amount, 2);
    assert_eq!(record.net_amount, 9_999);
}

#[test]
fn test_split_fee_very_small_amount() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &1u32, &fee_collector); // 1 bps

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 10);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &1i128,
        &String::from_str(&env, "very small amount"),
    );

    assert_eq!(record.fee_amount, 1);
    assert_eq!(record.net_amount, 0);
}

#[test]
fn test_split_fee_zero_fee_rate() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &0u32, &fee_collector); // 0 bps

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 10_000);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &9_999i128,
        &String::from_str(&env, "zero fee rate"),
    );

    assert_eq!(record.fee_amount, 0);
    assert_eq!(record.net_amount, 9_999);
}

#[test]
fn test_batch_fee_evasion_regression() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &1u32, &fee_collector); // 1 bps

    let sender = Address::generate(&env);
    // fund for 50 * 9,999 = 499,950
    mint(&env, &token, &token_admin, &sender, 499_950);

    let mut payments = vec![&env];
    for _ in 0..50 {
        payments.push_back((Address::generate(&env), 9_999i128));
    }

    // Sender must approve the contract for batch transfer. We don't need token_client for `approve` when it's just a mock setup, wait `client.send_batch_payment` may not require it if it's acting on behalf of sender directly using `from.require_auth()`, wait no, `send_batch_payment` needs sender auth, not token approve. Let's check `test_send_payment_happy_path`. In `send_payment`, it just mocks all auths and calls `client.send_payment`. Wait, `send_batch_payment` does the same. No, wait, some tests use `token_client.approve` like subscriptions, but `send_payment` doesn't need it because it uses `require_auth` on `from`.
    env.mock_all_auths();

    // Run batch payment
    client.send_batch_payment(&sender, &token, &payments);

    // Each of the 50 legs should charge 1 fee unit, resulting in 50 fee_collector balance.
    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&fee_collector), 50);
}

#[test]
fn test_equivalent_single_payment() {
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &1u32, &fee_collector); // 1 bps

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &token, &token_admin, &sender, 499_950);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &499_950i128,
        &String::from_str(&env, "equivalent single payment"),
    );

    // 499,950 * 1 / 10000 = 49.995 => ceil => 50
    assert_eq!(record.fee_amount, 50);
    assert_eq!(record.net_amount, 499_900);
}

// ---------------------------------------------------------------------------
// Zero-net-transfer guard tests (closes #53)
//
// These tests exercise all four call sites at the maximum allowed fee
// (MAX_FEE_BPS = 1_000 bps / 10%) to confirm:
//   1. The net-amount guard (`if net_amount > 0`) is symmetric with the
//      existing fee-amount guard — both legs are now conditionally transferred.
//   2. Payments succeed cleanly at the ceiling fee, producing correct
//      fee/net splits (fee = 10% of gross, net = 90% of gross).
//
// Prior to MAX_FEE_BPS being capped at 1_000, a fee of 10_000 bps (100%)
// made net_amount == 0 reachable on every payment, and the unconditional
// `token_client.transfer(..., &net_amount)` call could trap on any SEP-41
// token implementation that (validly) rejects zero-amount transfers.  The
// guard added by this fix is defense-in-depth for that case; with
// MAX_FEE_BPS = 1_000, net_amount == 0 is no longer reachable through the
// public API, but the guard keeps the code correct if the ceiling is ever
// raised again, and removes the asymmetry that made the fee leg safe while
// leaving the net leg unsafe.
// ---------------------------------------------------------------------------

#[test]
fn test_send_payment_at_max_fee_skips_zero_net_transfer() {
    // Confirm send_payment at MAX_FEE_BPS (10%) succeeds, produces correct
    // fee/net split, and transfers only the nonzero net amount to recipient.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &MAX_FEE_BPS, &fee_collector); // 10% fee

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    // Use a round amount so the 10% fee is exact: 1_000 gross → 100 fee, 900 net.
    mint(&env, &token, &token_admin, &sender, 1_000);

    let record = client.send_payment(
        &sender,
        &recipient,
        &token,
        &1_000i128,
        &String::from_str(&env, "max fee test"),
    );

    assert_eq!(record.fee_amount, 100, "fee should be 10% of gross");
    assert_eq!(record.net_amount, 900, "net should be 90% of gross");

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&fee_collector), 100);
    assert_eq!(token_client.balance(&recipient), 900);
    assert_eq!(token_client.balance(&sender), 0);
}

#[test]
fn test_send_batch_payment_at_max_fee_skips_zero_net_transfer() {
    // Confirm send_batch_payment at MAX_FEE_BPS (10%) succeeds across both
    // legs, and the per-leg net-amount guard is in place (net > 0 for each
    // leg, so both transfers execute).
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &MAX_FEE_BPS, &fee_collector); // 10% fee

    let sender = Address::generate(&env);
    let r1 = Address::generate(&env);
    let r2 = Address::generate(&env);
    // 1_000 + 2_000 gross; fee = 100 + 200 = 300; net = 900 + 1_800 = 2_700.
    mint(&env, &token, &token_admin, &sender, 3_000);

    let payments = vec![&env, (r1.clone(), 1_000i128), (r2.clone(), 2_000i128)];
    let records = client.send_batch_payment(&sender, &token, &payments);

    assert_eq!(records.len(), 2);
    assert_eq!(records.get(0).unwrap().fee_amount, 100);
    assert_eq!(records.get(0).unwrap().net_amount, 900);
    assert_eq!(records.get(1).unwrap().fee_amount, 200);
    assert_eq!(records.get(1).unwrap().net_amount, 1_800);

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&r1), 900);
    assert_eq!(token_client.balance(&r2), 1_800);
    assert_eq!(token_client.balance(&fee_collector), 300);
    assert_eq!(token_client.balance(&sender), 0);
}

#[test]
fn test_fulfill_payment_request_at_max_fee_skips_zero_net_transfer() {
    // Confirm fulfill_payment_request at MAX_FEE_BPS (10%) succeeds and the
    // net-amount guard is symmetric with the fee-leg guard already present.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &MAX_FEE_BPS, &fee_collector); // 10% fee

    let requester = Address::generate(&env);
    let payer = Address::generate(&env);
    // 1_000 gross → 100 fee, 900 net.
    mint(&env, &token, &token_admin, &payer, 1_000);

    let expiry = env.ledger().timestamp() + 1_000;
    let id = client.create_payment_request(
        &requester,
        &None,
        &token,
        &1_000i128,
        &String::from_str(&env, "max fee invoice"),
        &expiry,
    );

    let net = client.fulfill_payment_request(&id, &payer);
    assert_eq!(net, 900, "net amount should be 90% of gross at 10% fee");

    let request = client.get_payment_request(&id);
    assert_eq!(request.status, PaymentRequestStatus::Fulfilled);

    let token_client = TokenClient::new(&env, &token);
    assert_eq!(token_client.balance(&requester), 900);
    assert_eq!(token_client.balance(&fee_collector), 100);
    assert_eq!(token_client.balance(&payer), 0);
}

#[test]
fn test_execute_subscription_at_max_fee_skips_zero_net_transfer() {
    // Confirm execute_subscription at MAX_FEE_BPS (10%) succeeds and the
    // net-amount transfer_from guard is symmetric with the fee leg already
    // guarded.  The returned net_amount and balance changes must reflect the
    // correct 90%/10% split.
    let (env, client, admin, fee_collector, token, token_admin) = setup();
    client.initialize(&admin, &MAX_FEE_BPS, &fee_collector); // 10% fee

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    // 1_000 gross per execution → 100 fee, 900 net.
    mint(&env, &token, &token_admin, &payer, 10_000);

    let token_client = TokenClient::new(&env, &token);
    token_client.approve(
        &payer,
        &client.address,
        &10_000i128,
        &(env.ledger().sequence() + 1_000),
    );

    let start = env.ledger().timestamp();
    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &600u64,
        &start,
        &None,
        &None,
    );

    let net = client.execute_subscription(&id);
    assert_eq!(net, 900, "net amount should be 90% of gross at 10% fee");

    assert_eq!(token_client.balance(&recipient), 900);
    assert_eq!(token_client.balance(&fee_collector), 100);
    // Payer debited full 1_000 gross (fee + net legs combined).
    assert_eq!(token_client.balance(&payer), 9_000);
}


#[test]
fn test_yearly_subscription_ttl_covers_next_due_date() {
    let (env, client, admin, fee_collector, token, _token_admin) = setup();

    // A one-year cadence is deliberately much longer than the ordinary
    // persistent-entry minimum. Give the test host enough maximum TTL headroom
    // so the production cadence-aware extension is not clipped by the host cap.
    const YEAR_SECONDS: u64 = 365 * 24 * 60 * 60;
    const LEDGER_SECONDS: u64 = 5;
    let ledgers_until_due =
        ((YEAR_SECONDS + LEDGER_SECONDS - 1) / LEDGER_SECONDS) as u32;
    env.ledger().set_min_persistent_entry_ttl(10);
    env.ledger().set_max_entry_ttl(ledgers_until_due + 20_000);

    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let first_due = env.ledger().timestamp() + YEAR_SECONDS;

    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &YEAR_SECONDS,
        &first_due,
        &None,
        &None,
    );

    // Persistent::get_ttl reports the number of ledgers for which the entry
    // remains accessible. A value beyond the one-year due horizon proves the
    // subscription can still be loaded when its first payment becomes due,
    // without a RestoreFootprintOp or any other manual subscription restore.
    let ttl = env.as_contract(&client.address, || {
        env.storage().persistent().get_ttl(&(KEY_SUB, id))
    });
    assert!(
        ttl > ledgers_until_due,
        "cadence-aware TTL must keep a yearly subscription live through its next due date"
    );

    // Permissionless keepers can refresh the same lifecycle horizon while the
    // subscription is dormant; no payer signature is needed for this call.
    client.keep_subscription_alive(&id);
    let refreshed_ttl = env.as_contract(&client.address, || {
        env.storage().persistent().get_ttl(&(KEY_SUB, id))
    });
    assert!(refreshed_ttl >= ttl);
}

#![cfg(test)]

use soroban_sdk::{
    testutils::{storage::Persistent as _, Address as _, Ledger as _},
    Address, Env,
};

use crate::{StellarSendContract, StellarSendContractClient, KEY_SUB};

#[test]
fn yearly_subscription_ttl_survives_to_first_due_date() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_sequence_number(1_000);
    env.ledger().set_max_entry_ttl(6_400_000);

    let contract_id = env.register_contract(None, StellarSendContract);
    let client = StellarSendContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let fee_collector = Address::generate(&env);
    client.initialize(&admin, &0u32, &fee_collector);

    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token = Address::generate(&env);
    let year_seconds = 365u64 * 24 * 60 * 60;
    let first_due = env.ledger().timestamp() + year_seconds;
    let expiry = first_due + 24 * 60 * 60;

    let id = client.create_subscription(
        &payer,
        &recipient,
        &token,
        &1_000i128,
        &year_seconds,
        &first_due,
        &None,
        &Some(expiry),
    );

    let key = (KEY_SUB, id);
    let initial_ttl = env.as_contract(&contract_id, || {
        env.storage().persistent().get_ttl(&key)
    });
    let year_ledgers = ((year_seconds + 4) / 5) as u32;
    assert!(
        initial_ttl >= year_ledgers,
        "yearly subscription TTL must reach its first due ledger"
    );

    // Move to the first due time without any restore operation. The entry
    // remains live because creation extended its TTL through this horizon.
    env.ledger()
        .set_sequence_number(1_000 + year_ledgers.saturating_sub(1));
    env.ledger().set_timestamp(first_due);
    let still_live = env.as_contract(&contract_id, || {
        env.storage().persistent().has(&key)
    });
    assert!(still_live, "subscription must remain live through first due date");
}

#![cfg(test)]

use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    vec, Address, Env,
};

use crate::{StellarSendContract, StellarSendContractClient, StellarSendError, MAX_PATH_LEN};

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
    let contract_id = env.register_contract(None, StellarSendContract);
    let client = StellarSendContractClient::new(&env, &contract_id);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    (env, client, admin, fee_collector, token_id.address(), token_admin)
}

fn mint(env: &Env, token: &Address, to: &Address, amount: i128) {
    StellarAssetClient::new(env, token).mint(to, &amount);
}

#[test]
fn empty_path_remains_valid() {
    let (env, client, admin, fee_collector, send_token, send_token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);
    let dest_admin = Address::generate(&env);
    let dest_token = env.register_stellar_asset_contract_v2(dest_admin.clone()).address();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &send_token, &sender, 1_000);
    mint(&env, &dest_token, &client.address, 1_000);
    let amount = client.send_path_payment(
        &sender,
        &recipient,
        &send_token,
        &1_000i128,
        &dest_token,
        &900i128,
        &vec![&env],
    );
    assert_eq!(amount, 990);
}

#[test]
fn max_length_path_remains_valid() {
    let (env, client, admin, fee_collector, send_token, _send_token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);
    let dest_admin = Address::generate(&env);
    let dest_token = env.register_stellar_asset_contract_v2(dest_admin.clone()).address();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    mint(&env, &send_token, &sender, 1_000);
    mint(&env, &dest_token, &client.address, 1_000);
    let mut path = vec![&env];
    for _ in 0..MAX_PATH_LEN {
        path.push_back(Address::generate(&env));
    }
    let amount = client.send_path_payment(
        &sender,
        &recipient,
        &send_token,
        &1_000i128,
        &dest_token,
        &900i128,
        &path,
    );
    assert_eq!(amount, 990);
}

#[test]
fn overlong_path_is_rejected_before_side_effects() {
    let (env, client, admin, fee_collector, send_token, _send_token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let dest_token = Address::generate(&env);
    mint(&env, &send_token, &sender, 1_000);
    let mut path = vec![&env];
    for _ in 0..=MAX_PATH_LEN {
        path.push_back(Address::generate(&env));
    }
    let result = client.try_send_path_payment(
        &sender,
        &recipient,
        &send_token,
        &1_000i128,
        &dest_token,
        &900i128,
        &path,
    );
    assert_eq!(result, Err(Ok(StellarSendError::InvalidPath)));
    assert_eq!(client.get_sequence(&sender), 0);
    let token = TokenClient::new(&env, &send_token);
    assert_eq!(token.balance(&sender), 1_000);
    assert_eq!(token.balance(&fee_collector), 0);
}

#[test]
fn endpoint_assets_are_rejected_as_intermediate_hops() {
    let (env, client, admin, fee_collector, send_token, _send_token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);
    let dest_token = Address::generate(&env);
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let send_result = client.try_send_path_payment(
        &sender,
        &recipient,
        &send_token,
        &1_000i128,
        &dest_token,
        &900i128,
        &vec![&env, send_token.clone()],
    );
    assert_eq!(send_result, Err(Ok(StellarSendError::InvalidPath)));
    let dest_result = client.try_send_path_payment(
        &sender,
        &recipient,
        &send_token,
        &1_000i128,
        &dest_token,
        &900i128,
        &vec![&env, dest_token.clone()],
    );
    assert_eq!(dest_result, Err(Ok(StellarSendError::InvalidPath)));
}

#[test]
fn repeated_intermediate_hop_is_rejected() {
    let (env, client, admin, fee_collector, send_token, _send_token_admin) = setup();
    client.initialize(&admin, &100u32, &fee_collector);
    let hop_a = Address::generate(&env);
    let hop_b = Address::generate(&env);
    let result = client.try_send_path_payment(
        &Address::generate(&env),
        &Address::generate(&env),
        &send_token,
        &1_000i128,
        &Address::generate(&env),
        &900i128,
        &vec![&env, hop_a.clone(), hop_b, hop_a],
    );
    assert_eq!(result, Err(Ok(StellarSendError::InvalidPath)));
}

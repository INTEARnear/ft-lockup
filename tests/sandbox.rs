use std::sync::{Arc, OnceLock};

use ft_lockup::Account;
use near_api::types::transaction::result::TransactionResult;
use near_api::{AccountId, Contract, NearToken, NetworkConfig, Signer};
use near_sandbox::Sandbox;
use near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY;
use near_sdk::base64::Engine;
use near_sdk::base64::prelude::BASE64_STANDARD;
use near_sdk::json_types::{U64, U128};
use near_sdk::serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

const TOTAL_SUPPLY: u128 = 1_000;
const DEPOSIT_AMOUNT: u128 = 100;
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
const STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10);

fn compile_contract(manifest_dir: &str) -> Vec<u8> {
    let build_opts = cargo_near_build::BuildOpts::builder()
        .manifest_path(format!("{manifest_dir}/Cargo.toml"))
        .build();
    let wasm_path = cargo_near_build::build_with_cli(build_opts).expect("contract build failed");
    std::fs::read(wasm_path).expect("failed to read built wasm")
}

fn lockup_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| compile_contract(env!("CARGO_MANIFEST_DIR")))
}

async fn token_wasm() -> &'static [u8] {
    static WASM: OnceCell<Vec<u8>> = OnceCell::const_new();
    WASM.get_or_init(async || {
        BASE64_STANDARD
            .decode(
                &Contract::global_wasm()
                    .by_hash(
                        "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF"
                            .parse()
                            .unwrap(),
                    )
                    .fetch_from_mainnet()
                    .await
                    .unwrap()
                    .data
                    .code_base64,
            )
            .unwrap()
    })
    .await
}

/// Sandbox with a test token owned by alice (holding the whole supply) and a lockup contract for it.
/// All accounts share the sandbox default key.
struct TestEnv {
    sandbox: Sandbox,
    network: NetworkConfig,
    signer: Arc<Signer>,
    token: AccountId,
    lockup: AccountId,
    alice: AccountId,
}

impl TestEnv {
    async fn new() -> Self {
        let sandbox = Sandbox::start_sandbox().await.unwrap();
        let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse().unwrap());
        let signer =
            Signer::from_secret_key(DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse().unwrap()).unwrap();
        let env = Self {
            sandbox,
            network,
            signer,
            token: "token.sandbox".parse().unwrap(),
            lockup: "lockup.sandbox".parse().unwrap(),
            alice: "alice.sandbox".parse().unwrap(),
        };

        env.create_account(&env.alice).await;
        env.deploy_token(&env.token).await;
        env.create_account(&env.lockup).await;
        Contract::deploy(env.lockup.clone())
            .use_code(lockup_wasm().to_vec())
            .with_init_call(
                "new",
                json!({
                    "token_id": env.token,
                    "unlock_period_ns": (7u64 * 24 * 60 * 60 * 1_000_000_000).to_string()
                }),
            )
            .unwrap()
            .with_signer(env.signer.clone())
            .send_to(&env.network)
            .await
            .unwrap()
            .assert_success();
        env.call(
            &env.lockup,
            &env.token,
            "storage_deposit",
            json!({}),
            STORAGE_DEPOSIT,
        )
        .await
        .assert_success();

        env
    }

    async fn create_account(&self, account_id: &AccountId) {
        self.sandbox
            .create_account(account_id.clone())
            .send()
            .await
            .unwrap();
    }

    async fn deploy_token(&self, token_id: &AccountId) {
        self.create_account(token_id).await;
        Contract::deploy(token_id.clone())
            .use_code(token_wasm().await.to_vec())
            .with_init_call(
                "new_default_meta",
                json!({ "owner_id": self.alice, "total_supply": U128(TOTAL_SUPPLY) }),
            )
            .unwrap()
            .with_signer(self.signer.clone())
            .send_to(&self.network)
            .await
            .unwrap()
            .assert_success();
    }

    async fn call(
        &self,
        caller: &AccountId,
        contract: &AccountId,
        method: &str,
        args: Value,
        deposit: NearToken,
    ) -> TransactionResult {
        Contract(contract.clone())
            .call_function(method, args)
            .transaction()
            .deposit(deposit)
            .max_gas()
            .with_signer(caller.clone(), self.signer.clone())
            .send_to(&self.network)
            .await
            .unwrap()
    }

    async fn view<T: DeserializeOwned + Send + Sync>(
        &self,
        contract: &AccountId,
        method: &str,
        args: Value,
    ) -> T {
        Contract(contract.clone())
            .call_function(method, args)
            .read_only()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }

    async fn alice_call_lockup(&self, method: &str, deposit: NearToken) -> TransactionResult {
        self.call(&self.alice, &self.lockup, method, json!({}), deposit)
            .await
    }

    async fn alice_deposits_to_lockup(&self) {
        self.call(
            &self.alice,
            &self.token,
            "ft_transfer_call",
            json!({ "receiver_id": self.lockup, "amount": U128(DEPOSIT_AMOUNT), "msg": "" }),
            ONE_YOCTO,
        )
        .await
        .assert_success();
    }

    async fn alice_token_balance(&self) -> u128 {
        let balance: U128 = self
            .view(
                &self.token,
                "ft_balance_of",
                json!({ "account_id": self.alice }),
            )
            .await;
        balance.0
    }

    async fn alice_lockup_account(&self) -> Option<Account> {
        self.view(
            &self.lockup,
            "get_account",
            json!({ "account_id": self.alice }),
        )
        .await
    }

    async fn alice_is_frozen(&self) -> bool {
        self.view(
            &self.lockup,
            "is_frozen",
            json!({ "account_id": self.alice }),
        )
        .await
    }

    async fn set_unlock_period(
        &self,
        caller: &AccountId,
        unlock_period_ns: u64,
    ) -> TransactionResult {
        self.call(
            caller,
            &self.lockup,
            "set_unlock_period",
            json!({ "unlock_period_ns": U64(unlock_period_ns) }),
            NearToken::from_yoctonear(0),
        )
        .await
    }
}

#[tokio::test]
async fn full_lockup_cycle_returns_tokens() {
    let env = TestEnv::new().await;
    env.set_unlock_period(&env.lockup, 0).await.assert_success();
    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();

    env.alice_deposits_to_lockup().await;
    assert_eq!(
        env.alice_token_balance().await,
        TOTAL_SUPPLY - DEPOSIT_AMOUNT
    );
    assert_eq!(
        env.alice_lockup_account().await.unwrap().deposited,
        U128(DEPOSIT_AMOUNT)
    );

    env.alice_call_lockup("lock", NearToken::from_yoctonear(1))
        .await
        .assert_success();
    let account = env.alice_lockup_account().await.unwrap();
    assert_eq!(
        (account.deposited, account.locked),
        (U128(0), U128(DEPOSIT_AMOUNT))
    );

    env.alice_call_lockup("unlock", NearToken::from_yoctonear(1))
        .await
        .assert_success();
    let account = env.alice_lockup_account().await.unwrap();
    assert_eq!(
        (account.locked, account.unlocking),
        (U128(0), U128(DEPOSIT_AMOUNT))
    );

    env.alice_call_lockup("withdraw", ONE_YOCTO)
        .await
        .assert_success();
    assert_eq!(env.alice_token_balance().await, TOTAL_SUPPLY);
    assert_eq!(
        env.alice_lockup_account().await.unwrap(),
        Account::default()
    );
}

#[tokio::test]
async fn withdraw_waits_for_unlock_period_set_at_unlock_time() {
    let env = TestEnv::new().await;
    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();
    env.alice_deposits_to_lockup().await;
    env.alice_call_lockup("lock", NearToken::from_yoctonear(1))
        .await
        .assert_success();
    env.alice_call_lockup("unlock", NearToken::from_yoctonear(1))
        .await
        .assert_success();

    let failure = env
        .alice_call_lockup("withdraw", ONE_YOCTO)
        .await
        .assert_failure();
    assert!(
        failure.to_string().contains("Nothing to withdraw yet"),
        "{failure}"
    );

    env.set_unlock_period(&env.lockup, 0).await.assert_success();
    let failure = env
        .alice_call_lockup("withdraw", ONE_YOCTO)
        .await
        .assert_failure();
    assert!(
        failure.to_string().contains("Nothing to withdraw yet"),
        "{failure}"
    );
    assert_eq!(
        env.alice_lockup_account().await.unwrap().unlocking,
        U128(DEPOSIT_AMOUNT)
    );
    assert_eq!(
        env.alice_token_balance().await,
        TOTAL_SUPPLY - DEPOSIT_AMOUNT
    );
}

#[tokio::test]
async fn only_contract_account_can_set_unlock_period() {
    let env = TestEnv::new().await;

    env.set_unlock_period(&env.alice, 0).await.assert_failure();
    let unlock_period: U64 = env.view(&env.lockup, "get_unlock_period", json!({})).await;
    assert_eq!(unlock_period, U64(7 * 24 * 60 * 60 * 1_000_000_000));

    env.set_unlock_period(&env.lockup, 60)
        .await
        .assert_success();
    let unlock_period: U64 = env.view(&env.lockup, "get_unlock_period", json!({})).await;
    assert_eq!(unlock_period, U64(60));
}

#[tokio::test]
async fn deposits_from_unregistered_accounts_and_other_tokens_are_refunded() {
    let env = TestEnv::new().await;

    env.alice_deposits_to_lockup().await;
    assert_eq!(env.alice_token_balance().await, TOTAL_SUPPLY);

    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();
    let other_token: AccountId = "other-token.sandbox".parse().unwrap();
    env.deploy_token(&other_token).await;
    env.call(
        &env.lockup,
        &other_token,
        "storage_deposit",
        json!({}),
        STORAGE_DEPOSIT,
    )
    .await
    .assert_success();
    env.call(
        &env.alice,
        &other_token,
        "ft_transfer_call",
        json!({ "receiver_id": env.lockup, "amount": U128(DEPOSIT_AMOUNT), "msg": "" }),
        ONE_YOCTO,
    )
    .await
    .assert_success();
    let other_token_balance: U128 = env
        .view(
            &other_token,
            "ft_balance_of",
            json!({ "account_id": env.alice }),
        )
        .await;
    assert_eq!(other_token_balance, U128(TOTAL_SUPPLY));
    assert_eq!(
        env.alice_lockup_account().await.unwrap(),
        Account::default()
    );
}

#[tokio::test]
async fn failed_withdraw_transfer_restores_balance() {
    let env = TestEnv::new().await;
    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();
    env.alice_deposits_to_lockup().await;
    env.call(
        &env.alice,
        &env.token,
        "ft_transfer",
        json!({ "receiver_id": env.lockup, "amount": U128(TOTAL_SUPPLY - DEPOSIT_AMOUNT) }),
        ONE_YOCTO,
    )
    .await
    .assert_success();
    env.call(
        &env.alice,
        &env.token,
        "storage_unregister",
        json!({}),
        ONE_YOCTO,
    )
    .await
    .assert_success();

    env.alice_call_lockup("withdraw", ONE_YOCTO)
        .await
        .assert_success();

    assert_eq!(
        env.alice_lockup_account().await.unwrap().deposited,
        U128(DEPOSIT_AMOUNT)
    );
}

#[tokio::test]
async fn transfer_with_msg_locks_amounts_for_each_account() {
    let env = TestEnv::new().await;
    let bob: AccountId = "bob.sandbox".parse().unwrap();
    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();
    env.call(
        &env.alice,
        &env.lockup,
        "register",
        json!({ "account_id": bob }),
        STORAGE_DEPOSIT,
    )
    .await
    .assert_success();

    env.call(
        &env.alice,
        &env.token,
        "ft_transfer_call",
        json!({
            "receiver_id": env.lockup,
            "amount": U128(DEPOSIT_AMOUNT),
            "msg": json!([[env.alice,"30"],[bob,"70"]]).to_string(),
        }),
        ONE_YOCTO,
    )
    .await
    .assert_success();

    assert_eq!(
        env.alice_token_balance().await,
        TOTAL_SUPPLY - DEPOSIT_AMOUNT
    );
    assert_eq!(env.alice_lockup_account().await.unwrap().locked, U128(30));
    let bob_account: Option<Account> = env
        .view(&env.lockup, "get_account", json!({ "account_id": bob }))
        .await;
    assert_eq!(bob_account.unwrap().locked, U128(70));
    let total_locked: U128 = env.view(&env.lockup, "get_total_locked", json!({})).await;
    assert_eq!(total_locked, U128(DEPOSIT_AMOUNT));
}

#[tokio::test]
async fn transfer_with_mismatched_sum_or_unregistered_account_is_refunded() {
    let env = TestEnv::new().await;
    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();

    for msg in [
        json!([[env.alice, "99"]]).to_string(),
        json!([[env.alice, "30"], ["bob.sandbox", "70"]]).to_string(),
    ] {
        env.call(
            &env.alice,
            &env.token,
            "ft_transfer_call",
            json!({ "receiver_id": env.lockup, "amount": U128(DEPOSIT_AMOUNT), "msg": msg }),
            ONE_YOCTO,
        )
        .await
        .assert_success();
    }

    assert_eq!(env.alice_token_balance().await, TOTAL_SUPPLY);
    assert_eq!(
        env.alice_lockup_account().await.unwrap(),
        Account::default()
    );
}

#[tokio::test]
async fn frozen_account_cannot_withdraw_until_unfrozen() {
    let env = TestEnv::new().await;
    env.alice_call_lockup("register", STORAGE_DEPOSIT)
        .await
        .assert_success();
    env.alice_deposits_to_lockup().await;

    let set_alice_frozen = async |caller: &AccountId, frozen: bool| {
        env.call(
            caller,
            &env.lockup,
            "set_frozen",
            json!({ "account_id": env.alice, "frozen": frozen }),
            NearToken::from_yoctonear(0),
        )
        .await
    };
    set_alice_frozen(&env.alice, false).await.assert_failure();
    set_alice_frozen(&env.lockup, true).await.assert_success();
    assert!(env.alice_is_frozen().await);

    let failure = env
        .alice_call_lockup("withdraw", ONE_YOCTO)
        .await
        .assert_failure();
    assert!(
        failure.to_string().contains("Account is frozen"),
        "{failure}"
    );

    set_alice_frozen(&env.lockup, false).await.assert_success();
    env.alice_call_lockup("withdraw", ONE_YOCTO)
        .await
        .assert_success();
    assert_eq!(env.alice_token_balance().await, TOTAL_SUPPLY);
}

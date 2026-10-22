use std::time::Duration;

use near_contract_standards::fungible_token::core::ext_ft_core;
use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
use near_sdk::json_types::{U64, U128};
use near_sdk::store::LookupMap;
use near_sdk::{
    AccountId, BorshStorageKey, Gas, IntoStorageKey, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue, assert_one_yocto, env, near, require,
};

const FT_TRANSFER_GAS: Gas = Gas::from_tgas(10);
const ON_WITHDRAW_GAS: Gas = Gas::from_tgas(10);

#[near(serializers = [borsh, json])]
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Account {
    /// Received via ft_transfer_call, not locked yet. Can be withdrawn any time.
    pub deposited: U128,
    pub locked: U128,
    /// Unlocked, becomes withdrawable at `unlocks_at_ns`.
    pub unlocking: U128,
    pub unlocks_at_ns: U64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    token_id: AccountId,
    accounts: LookupMap<AccountId, Account>,
    unlock_period_ns: u64,
    total_locked: u128,
}

#[near(serializers=[borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Accounts,
}

const MAX_UNLOCK_PERIOD: Duration = Duration::from_hours(24 * 90);

#[near]
impl Contract {
    #[init]
    #[private]
    pub fn new(token_id: AccountId, unlock_period_ns: U64) -> Self {
        require!(
            unlock_period_ns.0 < MAX_UNLOCK_PERIOD.as_nanos() as u64,
            "Invalid unlock period"
        );
        Self {
            token_id,
            accounts: LookupMap::new(StorageKey::Accounts),
            unlock_period_ns: unlock_period_ns.0,
            total_locked: 0,
        }
    }

    /// Applies only to future `unlock()` calls, already unlocking tokens keep their `unlocks_at_ns`.
    #[private]
    pub fn set_unlock_period(&mut self, unlock_period_ns: U64) {
        require!(
            unlock_period_ns.0 < MAX_UNLOCK_PERIOD.as_nanos() as u64,
            "Invalid unlock period"
        );
        self.unlock_period_ns = unlock_period_ns.0;
    }

    #[payable]
    pub fn lock(&mut self) {
        assert_one_yocto();
        let account = self.registered_account_mut(&env::predecessor_account_id());
        require!(account.deposited.0 > 0, "Nothing to lock");
        let amount_to_lock = account.deposited.0;
        account.locked.0 += amount_to_lock;
        account.deposited.0 = 0;
        self.total_locked += amount_to_lock;
    }

    #[payable]
    pub fn unlock(&mut self) {
        assert_one_yocto();
        let unlocks_at_ns = env::block_timestamp() + self.unlock_period_ns;
        let account = self.registered_account_mut(&env::predecessor_account_id());
        require!(account.locked.0 > 0, "Nothing to unlock");
        require!(
            account.unlocking.0 == 0,
            "Previous unlock must be withdrawn first"
        );
        account.unlocking = account.locked;
        account.locked.0 = 0;
        account.unlocks_at_ns.0 = unlocks_at_ns;
        self.total_locked -= account.unlocking.0;
    }

    /// Sends back the never-locked deposit plus the unlocking amount once its unlock period has passed.
    #[payable]
    pub fn withdraw(&mut self) -> Promise {
        assert_one_yocto();
        let account_id = env::predecessor_account_id();
        let account = self.registered_account_mut(&account_id);

        let mut amount_to_withdraw = account.deposited.0;
        account.deposited.0 = 0;
        if env::block_timestamp() >= account.unlocks_at_ns.0 {
            amount_to_withdraw += account.unlocking.0;
            account.unlocking.0 = 0;
            account.unlocks_at_ns.0 = 0;
        }
        require!(amount_to_withdraw > 0, "Nothing to withdraw yet");

        ext_ft_core::ext(self.token_id.clone())
            .with_attached_deposit(NearToken::from_yoctonear(1))
            .with_static_gas(FT_TRANSFER_GAS)
            .ft_transfer(account_id.clone(), U128(amount_to_withdraw), None)
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(ON_WITHDRAW_GAS)
                    .on_withdraw(account_id, U128(amount_to_withdraw)),
            )
    }

    #[private]
    pub fn on_withdraw(
        &mut self,
        account_id: AccountId,
        amount: U128,
        #[callback_result] transfer_result: Result<(), PromiseError>,
    ) {
        if transfer_result.is_err() {
            // Accounts are never unregistered
            self.accounts.get_mut(&account_id).unwrap().deposited.0 += amount.0;
        }
    }

    #[payable]
    pub fn register(&mut self, account_id: Option<AccountId>) {
        let account_id = account_id.unwrap_or_else(env::predecessor_account_id);
        let attached = env::attached_deposit();
        let account = Account::default();
        let storage_usage = 40
            + StorageKey::Accounts.into_storage_key().len() as u64
            + 4
            + account_id.len() as u64
            + near_sdk::borsh::to_vec(&account).unwrap().len() as u64;
        let storage_cost = env::storage_byte_cost().saturating_mul(storage_usage as u128);

        let refund = if self.accounts.contains_key(&account_id) {
            attached
        } else {
            require!(
                attached >= storage_cost,
                format!("Attach at least {storage_cost}")
            );
            self.accounts.insert(account_id, account);
            attached.saturating_sub(storage_cost)
        };
        if !refund.is_zero() {
            Promise::new(env::predecessor_account_id())
                .transfer(refund)
                .detach();
        }
    }

    pub fn is_registered(&self, account_id: AccountId) -> bool {
        self.accounts.contains_key(&account_id)
    }

    pub fn get_account(&self, account_id: AccountId) -> Option<Account> {
        self.accounts.get(&account_id).cloned()
    }

    pub fn get_token_id(&self) -> AccountId {
        self.token_id.clone()
    }

    pub fn get_unlock_period(&self) -> U64 {
        U64(self.unlock_period_ns)
    }

    pub fn get_total_locked(&self) -> U128 {
        U128(self.total_locked)
    }
}

impl Contract {
    fn registered_account_mut(&mut self, account_id: &AccountId) -> &mut Account {
        self.accounts
            .get_mut(account_id)
            .unwrap_or_else(|| env::panic_str("Account is not registered, call register first"))
    }
}

#[near]
impl FungibleTokenReceiver for Contract {
    fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        require!(msg.is_empty(), "msg must be empty");
        require!(
            env::predecessor_account_id() == self.token_id,
            "Only the configured token is accepted"
        );
        self.registered_account_mut(&sender_id).deposited.0 += amount.0;
        PromiseOrValue::Value(U128(0))
    }
}

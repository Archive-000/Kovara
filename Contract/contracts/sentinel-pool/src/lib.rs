#![no_std]
//! `SentinelPool` — manages verifier staking, governance thresholds, withdrawals,
//! and fraud slashing for the sentinel set.
//!
//! # Initialization is a precondition, not a convention
//!
//! Every state-changing entry point calls `require_initialized` before it
//! touches storage or asks for authorization. An uninitialized pool therefore
//! rejects `stake`, `withdraw`, `set_threshold`, `set_min_stake` and `slash`
//! with [`Error::NotInitialized`] instead of silently behaving like a pool whose
//! `min_stake` is `0` and whose `lock_period` is `0` — a state in which anybody
//! could stake nothing and anybody could be slashed against a stake that does
//! not exist.
//!
//! `initialize` is itself all-or-nothing: every parameter is validated before
//! the first write, so a rejected call leaves `DataKey::Admin` absent and the
//! contract still initializable. See [`SentinelPool::initialize`].

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Map,
    Symbol,
};

/// Upper bound on the withdrawal lock period, in ledger sequence numbers.
///
/// The lock is expressed as `ledger.sequence()`, which is a `u32`. A lock that
/// cannot elapse is worse than a short one: it would permanently freeze every
/// sentinel's stake with no recovery path. Bounding the parameter at
/// initialization keeps `staked_at + lock_period` inside the `u32` ledger
/// sequence space for every reachable `staked_at`.
pub const MAX_LOCK_PERIOD: u64 = 1_000_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    InvalidStake = 1,
    Unauthorized = 2,
    StakeBelowMinimum = 3,
    LockPeriodActive = 4,
    InvalidThreshold = 5,
    InvalidAmount = 6,
    FraudProofAlreadyUsed = 7,
    InsufficientBalance = 8,
    /// The pool has not been configured yet. Append-only discriminant; see the
    /// ABI note in `README.md`.
    NotInitialized = 9,
    /// `lock_period` is zero or above [`MAX_LOCK_PERIOD`].
    InvalidLockPeriod = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    MinStake,
    LockPeriod,
    Threshold,
    Stakes,
    StakedAt,
    SlashProofs,
}

#[contractevent]
#[derive(Clone)]
pub struct StakedEvent {
    #[topic]
    pub sentinel: Address,
    pub amount: i128,
    pub total: i128,
}

#[contractevent]
#[derive(Clone)]
pub struct WithdrawnEvent {
    #[topic]
    pub sentinel: Address,
    pub amount: i128,
    pub remaining: i128,
}

#[contractevent]
#[derive(Clone)]
pub struct ThresholdUpdatedEvent {
    #[topic]
    pub actor: Address,
    pub threshold: u32,
}

#[contractevent]
#[derive(Clone)]
pub struct SlashedEvent {
    #[topic]
    pub sentinel: Address,
    #[topic]
    pub reason: Symbol,
    pub amount: i128,
}

#[contract]
pub struct SentinelPool;

#[contractimpl]
impl SentinelPool {
    /// # Configure the pool
    ///
    /// All-or-nothing. Every parameter is validated **before** the first write,
    /// so a rejected call leaves storage untouched and the contract still
    /// initializable — [`Self::is_initialized`] keeps reporting `false` and a
    /// later, corrected call still succeeds.
    ///
    /// Validation order is fixed and each step is a pure predicate:
    ///
    /// 1. `min_stake > 0` — a non-positive minimum makes every stake
    ///    trivially valid and every sentinel worthless.
    /// 2. `1 <= lock_period <= MAX_LOCK_PERIOD` — a zero lock removes the
    ///    withdrawal delay entirely, and an unbounded lock could exceed the
    ///    `u32` ledger sequence space and freeze stake permanently.
    /// 3. `threshold > 0` — a zero quorum means any single signature governs.
    ///
    /// Only then is `admin.require_auth()` collected, the re-initialization
    /// guard consulted, and storage written.
    ///
    /// # Errors
    /// * [`Error::InvalidStake`] — `min_stake <= 0`.
    /// * [`Error::InvalidLockPeriod`] — `lock_period` is zero or too large.
    /// * [`Error::InvalidThreshold`] — `threshold == 0`.
    /// * [`Error::Unauthorized`] — the pool is already initialized.
    pub fn initialize(
        env: Env,
        admin: Address,
        min_stake: i128,
        lock_period: u64,
        threshold: u32,
    ) -> Result<(), Error> {
        // 1. Parameters — pure, before auth and before storage.
        if min_stake <= 0 {
            return Err(Error::InvalidStake);
        }
        if lock_period == 0 || lock_period > MAX_LOCK_PERIOD {
            return Err(Error::InvalidLockPeriod);
        }
        if threshold == 0 {
            return Err(Error::InvalidThreshold);
        }

        // 2. Auth comes after every pure check.
        admin.require_auth();

        // 3. Re-initialization guard, still before the first write.
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::Unauthorized);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::MinStake, &min_stake);
        env.storage().instance().set(&DataKey::LockPeriod, &lock_period);
        env.storage().instance().set(&DataKey::Threshold, &threshold);
        env.storage().instance().set(&DataKey::Stakes, &Map::<Address, i128>::new(&env));
        env.storage().instance().set(&DataKey::StakedAt, &Map::<Address, u64>::new(&env));
        env.storage()
            .instance()
            .set(&DataKey::SlashProofs, &Map::<(Address, Symbol), bool>::new(&env));

        Ok(())
    }

    /// Whether [`Self::initialize`] has completed.
    ///
    /// A view, not a guard: it reports state and writes nothing. The enforcing
    /// form is `require_initialized`.
    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Admin)
    }

    /// Fail unless the pool has been initialized.
    ///
    /// Every state-changing entry point opens with this. Without it the
    /// fallbacks in the read paths (`min_stake -> 0`, `lock_period -> 0`) turn
    /// an uninitialized pool into one with a zero minimum stake and no lock,
    /// which is a strictly weaker contract than the one the admin intended.
    ///
    /// Checked before `require_auth()` so a call against an uninitialized pool
    /// fails with a specific, stable error instead of asking the caller for an
    /// authorization that can never succeed.
    fn require_initialized(env: &Env) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            Ok(())
        } else {
            Err(Error::NotInitialized)
        }
    }

    /// The configured admin, or [`Error::Unauthorized`] when there is none.
    ///
    /// A stored admin is mandatory. It used to be optional — the lookup fell
    /// back to `unwrap_or_else(|| caller.clone())`, which meant that on an
    /// uninitialized pool *any* address passed the equality check and could
    /// write the threshold it had just authenticated as. Requiring the stored
    /// value removes that path entirely.
    fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
        let stored: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        match stored {
            Some(stored) if stored == *caller => Ok(()),
            _ => Err(Error::Unauthorized),
        }
    }

    /// # Add to a sentinel's stake
    ///
    /// The new balance is computed with `checked_add`, so an overflowing
    /// deposit fails with [`Error::InvalidStake`] **before** any write. No
    /// partial state is produced: `Stakes`, `StakedAt` and the emitted event
    /// are all downstream of that check.
    ///
    /// # Errors
    /// * [`Error::NotInitialized`] — the pool is not configured.
    /// * [`Error::InvalidStake`] — `amount <= 0`, or the resulting balance
    ///   would overflow `i128`.
    /// * [`Error::StakeBelowMinimum`] — the resulting balance is under
    ///   `min_stake`.
    pub fn stake(env: Env, sentinel: Address, amount: i128) -> Result<(), Error> {
        Self::require_initialized(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidStake);
        }

        sentinel.require_auth();

        let mut stakes: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&DataKey::Stakes)
            .unwrap_or_else(|| Map::new(&env));

        let current = stakes.get(sentinel.clone()).unwrap_or(0);

        // Checked arithmetic. `checked_add` returns `None` only on overflow, so
        // the `InvalidStake` branch below is reached without `stakes` having
        // been touched — `current` is a copy, not a reference.
        let new_total = current
            .checked_add(amount)
            .ok_or(Error::InvalidStake)?;

        let min_stake = Self::min_stake(env.clone());
        if new_total < min_stake {
            return Err(Error::StakeBelowMinimum);
        }

        stakes.set(sentinel.clone(), new_total);
        env.storage().instance().set(&DataKey::Stakes, &stakes);

        let mut staked_at: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&DataKey::StakedAt)
            .unwrap_or_else(|| Map::new(&env));
        staked_at.set(sentinel.clone(), u64::from(env.ledger().sequence()));
        env.storage().instance().set(&DataKey::StakedAt, &staked_at);

        StakedEvent {
            sentinel: sentinel.clone(),
            amount,
            total: new_total,
        }
        .publish(&env);

        Ok(())
    }

    pub fn balance(env: Env, sentinel: Address) -> i128 {
        let stakes: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&DataKey::Stakes)
            .unwrap_or_else(|| Map::new(&env));
        stakes.get(sentinel).unwrap_or(0)
    }

    pub fn threshold(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(0)
    }

    pub fn min_stake(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MinStake)
            .unwrap_or(0)
    }

    pub fn quorum_threshold(env: Env, required: u32) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(required)
    }

    /// # Update the governance quorum threshold
    ///
    /// Two independent gates, both before the write:
    ///
    /// * the pool is initialized (so there is an admin to compare against), and
    /// * `admin` equals the stored admin — checked against storage, never
    ///   against the caller's own argument.
    ///
    /// `threshold == 0` is rejected with [`Error::InvalidThreshold`] before
    /// `require_auth`, so an invalid value costs the caller nothing and writes
    /// nothing.
    ///
    /// # Errors
    /// * [`Error::InvalidThreshold`] — `threshold == 0`.
    /// * [`Error::NotInitialized`] — the pool is not configured.
    /// * [`Error::Unauthorized`] — `admin` is not the configured admin.
    pub fn set_threshold(env: Env, admin: Address, threshold: u32) -> Result<(), Error> {
        if threshold == 0 {
            return Err(Error::InvalidThreshold);
        }

        Self::require_initialized(&env)?;

        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        env.storage().instance().set(&DataKey::Threshold, &threshold);

        ThresholdUpdatedEvent {
            actor: admin.clone(),
            threshold,
        }
        .publish(&env);

        Ok(())
    }

    /// # Update the minimum stake
    ///
    /// Same authorization shape as [`Self::set_threshold`]: initialized pool,
    /// configured admin, positive value — all checked before the write.
    ///
    /// # Errors
    /// * [`Error::InvalidStake`] — `min_stake <= 0`.
    /// * [`Error::NotInitialized`] — the pool is not configured.
    /// * [`Error::Unauthorized`] — `admin` is not the configured admin.
    pub fn set_min_stake(env: Env, admin: Address, min_stake: i128) -> Result<(), Error> {
        if min_stake <= 0 {
            return Err(Error::InvalidStake);
        }

        Self::require_initialized(&env)?;

        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        env.storage().instance().set(&DataKey::MinStake, &min_stake);
        Ok(())
    }

    /// # Withdraw part of a sentinel's stake
    ///
    /// Accounting is checked end to end, in this order, with no write until
    /// every check has passed:
    ///
    /// 1. the pool is initialized;
    /// 2. `amount > 0`;
    /// 3. `amount <= current` — otherwise [`Error::InsufficientBalance`];
    /// 4. the lock period has elapsed;
    /// 5. `current - amount` does not underflow — `checked_sub`, reported as
    ///    [`Error::InsufficientBalance`] rather than a panic;
    /// 6. the remainder stays at or above `min_stake`.
    ///
    /// Because the balance map is only written after step 6, every rejection
    /// leaves the sentinel's balance exactly as it was.
    ///
    /// # Errors
    /// * [`Error::NotInitialized`] — the pool is not configured.
    /// * [`Error::InvalidStake`] — `amount <= 0`.
    /// * [`Error::InsufficientBalance`] — `amount` exceeds the balance, or the
    ///   subtraction would underflow.
    /// * [`Error::LockPeriodActive`] — the lock has not elapsed.
    /// * [`Error::StakeBelowMinimum`] — the remainder would fall under
    ///   `min_stake`.
    pub fn withdraw(env: Env, sentinel: Address, amount: i128) -> Result<(), Error> {
        Self::require_initialized(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidStake);
        }

        sentinel.require_auth();

        let mut stakes: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&DataKey::Stakes)
            .unwrap_or_else(|| Map::new(&env));

        let current = stakes.get(sentinel.clone()).unwrap_or(0);
        if amount > current {
            return Err(Error::InsufficientBalance);
        }

        let lock_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriod)
            .unwrap_or(0);

        if lock_period > 0 {
            let staked_at: Map<Address, u64> = env
                .storage()
                .instance()
                .get(&DataKey::StakedAt)
                .unwrap_or_else(|| Map::new(&env));
            let last_staked = staked_at.get(sentinel.clone()).unwrap_or(0);
            let elapsed = u64::from(env.ledger().sequence()).saturating_sub(last_staked);
            if elapsed < lock_period {
                return Err(Error::LockPeriodActive);
            }
        }

        // Checked subtraction. `amount <= current` above already rules out an
        // underflow, so this `ok_or` arm is a belt-and-braces guard that keeps
        // the invariant local to this line: `remaining` is derived by checked
        // arithmetic, never by subtraction that can wrap.
        let remaining = current
            .checked_sub(amount)
            .ok_or(Error::InsufficientBalance)?;

        let min_stake = Self::min_stake(env.clone());
        if remaining < min_stake {
            return Err(Error::StakeBelowMinimum);
        }

        stakes.set(sentinel.clone(), remaining);
        env.storage().instance().set(&DataKey::Stakes, &stakes);

        WithdrawnEvent {
            sentinel: sentinel.clone(),
            amount,
            remaining,
        }
        .publish(&env);

        Ok(())
    }

    /// # Slash a sentinel's stake
    ///
    /// Requires a configured admin — `require_initialized` first, then
    /// `require_admin` against the stored value — so an unconfigured
    /// deployment can no longer be slashed against by whichever address
    /// happened to authorize the call.
    ///
    /// # Errors
    /// * [`Error::NotInitialized`] — the pool is not configured.
    /// * [`Error::Unauthorized`] — `admin` is not the configured admin.
    /// * [`Error::InvalidStake`] — `amount <= 0`.
    /// * [`Error::FraudProofAlreadyUsed`] — the `(sentinel, reason)` proof was
    ///   already applied.
    /// * [`Error::InsufficientBalance`] — `amount` exceeds the stake.
    /// * [`Error::StakeBelowMinimum`] — the remainder would fall under
    ///   `min_stake`.
    pub fn slash(
        env: Env,
        admin: Address,
        sentinel: Address,
        amount: i128,
        reason: Symbol,
    ) -> Result<(), Error> {
        Self::require_initialized(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidStake);
        }

        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        let mut proofs: Map<(Address, Symbol), bool> = env
            .storage()
            .instance()
            .get(&DataKey::SlashProofs)
            .unwrap_or_else(|| Map::new(&env));

        let proof_key = (sentinel.clone(), reason.clone());
        if proofs.get(proof_key.clone()).unwrap_or(false) {
            return Err(Error::FraudProofAlreadyUsed);
        }

        let mut stakes: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&DataKey::Stakes)
            .unwrap_or_else(|| Map::new(&env));

        let current = stakes.get(sentinel.clone()).unwrap_or(0);
        if amount > current {
            return Err(Error::InsufficientBalance);
        }

        let remaining = current
            .checked_sub(amount)
            .ok_or(Error::InsufficientBalance)?;

        let min_stake = Self::min_stake(env.clone());
        if remaining < min_stake {
            return Err(Error::StakeBelowMinimum);
        }

        stakes.set(sentinel.clone(), remaining);
        env.storage().instance().set(&DataKey::Stakes, &stakes);

        proofs.set(proof_key, true);
        env.storage().instance().set(&DataKey::SlashProofs, &proofs);

        SlashedEvent {
            sentinel: sentinel.clone(),
            reason,
            amount,
        }
        .publish(&env);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    #[test]
    fn stake_and_balance_persist_across_queries() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &150);

        assert_eq!(client.balance(&sentinel), 150);
        assert_eq!(client.min_stake(), 100);
        assert_eq!(client.threshold(), 2);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn stake_below_required_minimum_is_rejected() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &99);
    }

    #[test]
    fn governance_threshold_updates_are_authorized_and_persisted() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_threshold(&admin, &4);

        assert_eq!(client.threshold(), 4);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn unauthorized_threshold_update_fails_without_state_change() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_threshold(&attacker, &5);
    }

    #[test]
    fn withdrawals_respect_lock_period_and_minimums() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);

        assert!(client.try_withdraw(&sentinel, &50).is_err());

        env.ledger().with_mut(|l| l.sequence_number = 20);
        client.withdraw(&sentinel, &100);
        assert_eq!(client.balance(&sentinel), 150);

        client.withdraw(&sentinel, &50);
        assert_eq!(client.balance(&sentinel), 100);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #4)")]
    fn withdrawal_during_lock_period_raises_explicit_error() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        client.withdraw(&sentinel, &50);
    }

    #[test]
    fn slashing_records_the_penalty_and_rejects_replay() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        client.slash(&admin, &sentinel, &50, &reason);

        assert_eq!(client.balance(&sentinel), 200);
        assert!(client.try_slash(&admin, &sentinel, &10, &reason).is_err());
    }

    // ── CT-31: initialization safeguards ────────────────────────────────────

    #[test]
    fn a_rejected_initialization_leaves_the_pool_uninitialized() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        env.mock_all_auths();

        assert!(!client.is_initialized());

        // Non-positive minimum stake.
        assert!(client.try_initialize(&admin, &0, &10, &2).is_err());
        assert!(!client.is_initialized());

        // Zero lock period.
        assert!(client.try_initialize(&admin, &100, &0, &2).is_err());
        assert!(!client.is_initialized());

        // Lock period beyond MAX_LOCK_PERIOD.
        assert!(client
            .try_initialize(&admin, &100, &(MAX_LOCK_PERIOD + 1), &2)
            .is_err());
        assert!(!client.is_initialized());

        // Zero quorum threshold.
        assert!(client.try_initialize(&admin, &100, &10, &0).is_err());
        assert!(!client.is_initialized());

        // Nothing partial leaked into any configuration slot.
        assert_eq!(client.min_stake(), 0);
        assert_eq!(client.threshold(), 0);

        // The largest supported lock period is accepted, so the bound above is
        // a real limit and not an off-by-one.
        client.initialize(&admin, &100, &MAX_LOCK_PERIOD, &2);
        assert!(client.is_initialized());
        assert_eq!(client.threshold(), 2);
    }

    #[test]
    fn re_initialization_is_refused() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let other = Address::generate(&env);

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);
        assert!(client.try_initialize(&other, &999, &999, &9).is_err());

        // The original configuration survives the refused call.
        assert_eq!(client.min_stake(), 100);
        assert_eq!(client.threshold(), 2);
    }

    // ── CT-32: authorized threshold updates ─────────────────────────────────

    #[test]
    fn zero_threshold_is_refused_without_touching_storage() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);
        assert!(client.try_set_threshold(&admin, &0).is_err());

        assert_eq!(client.threshold(), 2);
    }

    #[test]
    fn threshold_update_on_an_uninitialized_pool_is_refused() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);

        env.mock_all_auths();

        // The caller is the *only* address that could ever be stored, and it is
        // still refused: there is no configured admin to match against.
        assert!(!client.is_initialized());
        assert!(client.try_set_threshold(&admin, &7).is_err());
        assert!(client.try_set_threshold(&attacker, &7).is_err());

        assert_eq!(client.threshold(), 0);
    }

    #[test]
    fn min_stake_updates_share_the_same_authorization_rules() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);

        assert!(client.try_set_min_stake(&admin, &0).is_err());
        assert!(client.try_set_min_stake(&admin, &-5).is_err());
        assert!(client.try_set_min_stake(&attacker, &5).is_err());
        assert_eq!(client.min_stake(), 100);

        client.set_min_stake(&admin, &250);
        assert_eq!(client.min_stake(), 250);
    }

    // ── CT-33: checked stake arithmetic ─────────────────────────────────────

    #[test]
    fn stake_overflow_is_refused_and_leaves_the_balance_untouched() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &1_000);

        // `1_000 + i128::MAX` overflows.
        assert!(client.try_stake(&sentinel, &i128::MAX).is_err());
        assert_eq!(client.balance(&sentinel), 1_000);

        // The balance is unchanged, so the same overflow is still the answer.
        assert!(client.try_stake(&sentinel, &i128::MAX).is_err());
        assert_eq!(client.balance(&sentinel), 1_000);
    }

    #[test]
    fn the_largest_representable_stake_is_accepted_exactly_once() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &i128::MAX);

        assert_eq!(client.balance(&sentinel), i128::MAX);
        // Replaying the same maximal deposit is the overflow case.
        assert!(client.try_stake(&sentinel, &i128::MAX).is_err());
        assert_eq!(client.balance(&sentinel), i128::MAX);
    }

    #[test]
    fn a_zero_or_negative_deposit_never_creates_a_stake() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);

        assert!(client.try_stake(&sentinel, &0).is_err());
        assert!(client.try_stake(&sentinel, &-1).is_err());
        assert_eq!(client.balance(&sentinel), 0);
    }

    // ── CT-34: an initialized pool is a precondition ────────────────────────

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn staking_before_initialization_is_refused() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        assert!(!client.is_initialized());
        client.stake(&sentinel, &500);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn withdrawing_before_initialization_is_refused() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.withdraw(&sentinel, &100);
    }

    #[test]
    fn slash_before_initialization_claims_no_fraud_proof() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        assert!(client.try_slash(&admin, &sentinel, &10, &reason).is_err());

        // The refused slash did not burn the proof: after a correct
        // initialization the same proof is still usable exactly once.
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        client.slash(&admin, &sentinel, &50, &reason);
        assert_eq!(client.balance(&sentinel), 200);
        assert!(client.try_slash(&admin, &sentinel, &10, &reason).is_err());
    }

    #[test]
    fn initialization_unblocks_every_entry_point() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();

        assert!(client.try_stake(&sentinel, &250).is_err());

        client.initialize(&admin, &100, &10, &2);

        client.stake(&sentinel, &250);
        assert_eq!(client.balance(&sentinel), 250);

        env.ledger().with_mut(|l| l.sequence_number = 20);
        client.withdraw(&sentinel, &100);
        assert_eq!(client.balance(&sentinel), 150);
    }
}

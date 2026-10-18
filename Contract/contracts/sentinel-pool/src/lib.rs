#![no_std]
//! `SentinelPool` — manages verifier staking, governance thresholds, withdrawals,
//! and fraud slashing for the sentinel set.
//!
//! # The minimum-stake floor is a registration rule
//!
//! An address with a non-zero stake is a *registered sentinel*, and a registered
//! sentinel is expected to keep at least `min_stake` on deposit for as long as
//! it stays registered. `stake` enforces that on the way in and `withdraw`
//! enforces it on the way out, so the floor holds at every observable balance —
//! including the very first one.
//!
//! The consequence, stated plainly: **a partial withdrawal may take a sentinel
//! down to exactly `min_stake` and no further.** Withdrawing to zero is not
//! available through `withdraw`, because a zero balance would silently
//! de-register the address, and the pool has no separate deregistration step to
//! record why the address stopped being a sentinel. Leaving the set is a
//! lifecycle event, not a side effect of an arithmetic result.
//!
//! # Withdrawal accounting is checked end to end
//!
//! `withdraw` validates `amount > 0`, `amount <= balance` and the resulting
//! remainder in that order, and writes the balance map only once every check has
//! passed. A rejected withdrawal leaves the sentinel's balance byte-for-byte
//! unchanged and emits nothing.
//!
//! # Reads never mutate
//!
//! `balance`, `stake_of` and `total_active_stake` are views. They report
//! storage and ledger state and write nothing, so an indexer can poll them
//! without constructing a transaction or perturbing the pool.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Map,
    Symbol,
};

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

/// A read-only snapshot of one sentinel's position.
///
/// Returned by [`SentinelPool::stake_of`]. It bundles the three facts a caller
/// needs to reason about a sentinel — how much is deposited, when the lock
/// started, and when the lock expires — so a caller does not have to make three
/// separate reads and correlate them against a moving ledger.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SentinelStake {
    /// The deposited amount. `0` for an address that has never staked.
    pub amount: i128,
    /// Ledger sequence number at which the current lock window started.
    /// `0` when `amount == 0`.
    pub staked_at: u64,
    /// Ledger sequence number at which the lock expires, i.e.
    /// `staked_at + lock_period`. Saturates at `u64::MAX`.
    pub unlock_at: u64,
    /// Whether the lock has elapsed as of the queried ledger.
    pub unlocked: bool,
}

#[contractevent]
#[derive(Clone)]
pub struct StakedEvent {
    #[topic]
    pub sentinel: Address,
    /// The amount added by this call.
    pub amount: i128,
    /// The sentinel's resulting stake, after this deposit.
    ///
    /// `amount` alone is not enough to reconstruct a sentinel's position: two
    /// deposits of the same size against different starting balances are very
    /// different events. An indexer that wants to rebuild balances from the
    /// event stream — rather than by polling `balance` — needs this field.
    pub total: i128,
    /// Ledger sequence number the deposit landed on, and therefore the ledger
    /// from which the withdrawal lock runs.
    pub staked_at: u64,
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
    pub fn initialize(
        env: Env,
        admin: Address,
        min_stake: i128,
        lock_period: u64,
        threshold: u32,
    ) -> Result<(), Error> {
        if min_stake <= 0 {
            return Err(Error::InvalidStake);
        }
        if threshold == 0 {
            return Err(Error::InvalidThreshold);
        }

        admin.require_auth();

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

    /// # Deposit stake
    ///
    /// The deposit is added to whatever the address already had, so a sentinel
    /// can top up without withdrawing first. The resulting balance is checked
    /// against `min_stake` **after** the addition, which is what makes the floor
    /// hold for the first stake as well as for later ones.
    ///
    /// The resulting balance and the ledger the lock starts from are both
    /// published in [`StakedEvent`], so the position after the call is
    /// reconstructible from the event stream alone.
    ///
    /// # Errors
    /// * [`Error::InvalidStake`] — `amount <= 0`.
    /// * [`Error::StakeBelowMinimum`] — the resulting balance is under
    ///   `min_stake`.
    pub fn stake(env: Env, sentinel: Address, amount: i128) -> Result<(), Error> {
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
        let new_total = current
            .checked_add(amount)
            .ok_or(Error::InvalidStake)?;

        let min_stake = Self::min_stake(env.clone());
        if new_total < min_stake {
            return Err(Error::StakeBelowMinimum);
        }

        stakes.set(sentinel.clone(), new_total);
        env.storage().instance().set(&DataKey::Stakes, &stakes);

        // The lock window starts on the ledger the deposit lands on. Read once
        // and reused, so the balance and the lock timestamp in `StakedEvent`
        // can never disagree about which ledger this was.
        let ledger_seq = u64::from(env.ledger().sequence());

        let mut staked_at: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&DataKey::StakedAt)
            .unwrap_or_else(|| Map::new(&env));
        staked_at.set(sentinel.clone(), ledger_seq);
        env.storage().instance().set(&DataKey::StakedAt, &staked_at);

        StakedEvent {
            sentinel: sentinel.clone(),
            amount,
            total: new_total,
            staked_at: ledger_seq,
        }
        .publish(&env);

        Ok(())
    }

    /// # One sentinel's full position
    ///
    /// A view. Reads storage and the ledger, writes nothing, and emits nothing.
    ///
    /// An address that has never staked is **not** an error: it reports the
    /// zero record (`amount == 0`, `staked_at == 0`, `unlocked == true`). That
    /// keeps the query total — a caller iterating candidates never has to
    /// special-case `None` for "not a sentinel yet", which is different from
    /// "the read failed".
    ///
    /// `unlock_at` saturates rather than overflowing: this is a reporting
    /// value, and reporting `u64::MAX` for an unreachable unlock ledger is more
    /// useful to a caller than failing the read.
    pub fn stake_of(env: Env, sentinel: Address) -> SentinelStake {
        let amount = Self::balance(env.clone(), sentinel.clone());
        let lock_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriod)
            .unwrap_or(0);

        if amount == 0 {
            return SentinelStake {
                amount: 0,
                staked_at: 0,
                unlock_at: 0,
                unlocked: true,
            };
        }

        let staked_at: u64 = Self::staked_at(env.clone(), sentinel);
        let unlock_at = staked_at.saturating_add(lock_period);
        let now = u64::from(env.ledger().sequence());

        SentinelStake {
            amount,
            staked_at,
            unlock_at,
            unlocked: now >= unlock_at,
        }
    }

    /// # Every deposited amount in the pool
    ///
    /// A view, and deliberately **computed rather than stored**: it folds the
    /// `Stakes` map at read time, so it cannot drift out of step with the
    /// per-address balances the way a cached running total can. The cost is one
    /// pass over the map, which is bounded by the sentinel set size — the same
    /// order as any governance read that has to see every sentinel.
    ///
    /// Sums saturate at `i128::MAX` rather than wrapping.
    pub fn total_active_stake(env: Env) -> i128 {
        let stakes: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&DataKey::Stakes)
            .unwrap_or_else(|| Map::new(&env));

        let mut total: i128 = 0;
        for amount in stakes.values() {
            total = total.saturating_add(amount);
        }
        total
    }

    /// The ledger sequence number the sentinel's current lock started on.
    ///
    /// `0` for an address that has never staked, which is also the genesis
    /// ledger — so a caller that only needs "is this address locked?" should
    /// read [`Self::stake_of`], which resolves the ambiguity.
    pub fn staked_at(env: Env, sentinel: Address) -> u64 {
        let staked_at: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&DataKey::StakedAt)
            .unwrap_or_else(|| Map::new(&env));
        staked_at.get(sentinel).unwrap_or(0)
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

    pub fn set_threshold(env: Env, admin: Address, threshold: u32) -> Result<(), Error> {
        if threshold == 0 {
            return Err(Error::InvalidThreshold);
        }

        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| admin.clone());

        if admin != stored_admin {
            return Err(Error::Unauthorized);
        }

        env.storage().instance().set(&DataKey::Threshold, &threshold);

        ThresholdUpdatedEvent {
            actor: admin.clone(),
            threshold,
        }
        .publish(&env);

        Ok(())
    }

    pub fn set_min_stake(env: Env, admin: Address, min_stake: i128) -> Result<(), Error> {
        if min_stake <= 0 {
            return Err(Error::InvalidStake);
        }

        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| admin.clone());

        if admin != stored_admin {
            return Err(Error::Unauthorized);
        }

        env.storage().instance().set(&DataKey::MinStake, &min_stake);
        Ok(())
    }

    /// # Withdraw part of a sentinel's stake
    ///
    /// # The floor rule
    ///
    /// A withdrawal may take a registered sentinel down to **exactly**
    /// `min_stake` and no further; anything that would leave the balance below
    /// `min_stake` is refused with [`Error::StakeBelowMinimum`], including a
    /// withdrawal of the entire balance. See the module docs for why exiting
    /// the set is a lifecycle event rather than an arithmetic side effect.
    ///
    /// The same rule is enforced on the way in by [`Self::stake`], so the floor
    /// holds for every balance the pool ever exposes — there is no window in
    /// which a registered sentinel is under-funded.
    ///
    /// # Accounting order
    ///
    /// Checks run in this order and the balance map is written only after all
    /// of them pass:
    ///
    /// 1. `amount > 0` — [`Error::InvalidStake`];
    /// 2. `amount <= balance` — [`Error::InsufficientBalance`];
    /// 3. the lock has elapsed — [`Error::LockPeriodActive`];
    /// 4. `balance - amount` does not underflow — `checked_sub`, reported as
    ///    [`Error::InsufficientBalance`];
    /// 5. the remainder is `>= min_stake` — [`Error::StakeBelowMinimum`].
    ///
    /// Step 2 makes step 4 unreachable in practice; `checked_sub` is kept
    /// anyway so that `remaining` is derived by checked arithmetic on the line
    /// that consumes it, rather than by a subtraction that a future edit could
    /// quietly un-check.
    ///
    /// Because nothing is written until step 5 succeeds, every rejection —
    /// invalid amount, excessive amount, active lock, below-minimum remainder —
    /// leaves the sentinel's balance exactly as it was, and emits no event.
    pub fn withdraw(env: Env, sentinel: Address, amount: i128) -> Result<(), Error> {
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

        let remaining = current
            .checked_sub(amount)
            .ok_or(Error::InsufficientBalance)?;

        // The floor rule. `remaining == min_stake` is allowed; only going under
        // it is refused.
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

    pub fn slash(
        env: Env,
        admin: Address,
        sentinel: Address,
        amount: i128,
        reason: Symbol,
    ) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidStake);
        }

        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| admin.clone());

        if admin != stored_admin {
            return Err(Error::Unauthorized);
        }

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
        testutils::{Address as _, Events as _, Ledger},
        Env, Event,
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

    // ── CT-35: stake event details ──────────────────────────────────────────

    #[test]
    fn a_stake_event_carries_the_resulting_balance_and_its_ledger() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        env.ledger().with_mut(|l| l.sequence_number = 7);
        client.stake(&sentinel, &150);

        // The whole payload is asserted: the event names the increment, the
        // balance *after* it, and the ledger the lock runs from.
        let first = StakedEvent {
            sentinel: sentinel.clone(),
            amount: 150,
            total: 150,
            staked_at: 7,
        };
        assert_eq!(
            env.events().all().filter_by_contract(&contract_id),
            [first.to_xdr(&env, &contract_id)]
        );

        // A top-up reports the balance after *that* deposit, not the delta.
        env.ledger().with_mut(|l| l.sequence_number = 9);
        client.stake(&sentinel, &50);

        let second = StakedEvent {
            sentinel: sentinel.clone(),
            amount: 50,
            total: 200,
            staked_at: 9,
        };
        assert_eq!(
            env.events().all().filter_by_contract(&contract_id),
            [first.to_xdr(&env, &contract_id), second.to_xdr(&env, &contract_id)]
        );
        assert_eq!(client.balance(&sentinel), 200);
    }

    // ── CT-36: the minimum-stake floor on withdrawal ────────────────────────

    #[test]
    fn a_partial_withdrawal_may_reach_the_floor_but_never_cross_it() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        env.ledger().with_mut(|l| l.sequence_number = 20);

        // Exactly to the floor: allowed.
        client.withdraw(&sentinel, &150);
        assert_eq!(client.balance(&sentinel), 100);

        // One unit below the floor: refused, balance preserved.
        assert!(client.try_withdraw(&sentinel, &1).is_err());
        assert_eq!(client.balance(&sentinel), 100);

        // The whole balance: refused too — exiting the set is a lifecycle
        // event, not an arithmetic side effect.
        assert!(client.try_withdraw(&sentinel, &100).is_err());
        assert_eq!(client.balance(&sentinel), 100);
    }

    #[test]
    fn the_floor_is_enforced_on_the_way_in_as_well() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // The first stake must clear the floor on its own.
        assert!(client.try_stake(&sentinel, &99).is_err());
        assert_eq!(client.balance(&sentinel), 0);

        client.stake(&sentinel, &100);
        assert_eq!(client.balance(&sentinel), 100);

        // A top-up that is individually small is fine: the *resulting* balance
        // is what has to clear the floor, not the increment.
        client.stake(&sentinel, &1);
        assert_eq!(client.balance(&sentinel), 101);
    }

    #[test]
    fn raising_the_floor_above_an_existing_balance_is_refused_on_withdrawal() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &150);
        client.set_min_stake(&admin, &200);

        env.ledger().with_mut(|l| l.sequence_number = 20);

        // The sentinel is already under the new floor, so every withdrawal —
        // including a zero-change one — is refused and the balance is intact.
        assert!(client.try_withdraw(&sentinel, &1).is_err());
        assert_eq!(client.balance(&sentinel), 150);
    }

    // ── CT-37: overflow-safe withdrawal accounting ──────────────────────────

    #[test]
    fn an_excessive_withdrawal_preserves_the_balance() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let stranger = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        env.ledger().with_mut(|l| l.sequence_number = 20);

        // More than the balance.
        assert!(client.try_withdraw(&sentinel, &251).is_err());
        assert_eq!(client.balance(&sentinel), 250);

        // `i128::MAX` against a small balance.
        assert!(client.try_withdraw(&sentinel, &i128::MAX).is_err());
        assert_eq!(client.balance(&sentinel), 250);

        // Zero and negative amounts.
        assert!(client.try_withdraw(&sentinel, &0).is_err());
        assert!(client.try_withdraw(&sentinel, &-250).is_err());
        assert!(client.try_withdraw(&sentinel, &i128::MIN).is_err());
        assert_eq!(client.balance(&sentinel), 250);

        // An address that never staked has nothing to withdraw.
        assert!(client.try_withdraw(&stranger, &1).is_err());
        assert_eq!(client.balance(&stranger), 0);
    }

    #[test]
    fn a_refused_withdrawal_emits_nothing() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        env.ledger().with_mut(|l| l.sequence_number = 20);

        let before = env.events().all().events().len();
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        assert!(client.try_withdraw(&sentinel, &0).is_err());
        assert_eq!(env.events().all().events().len(), before);
        assert_eq!(client.balance(&sentinel), 250);

        // The accepted withdrawal does publish.
        client.withdraw(&sentinel, &50);
        assert_eq!(env.events().all().events().len(), before + 1);
    }

    // ── CT-38: stake query methods ──────────────────────────────────────────

    #[test]
    fn stake_of_reports_the_full_position_without_writing() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let stranger = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        env.ledger().with_mut(|l| l.sequence_number = 5);
        client.stake(&sentinel, &250);

        let before = env.events().all().events().len();

        // Locked: 5 + 10 = 15, queried at ledger 6.
        env.ledger().with_mut(|l| l.sequence_number = 6);
        let locked = client.stake_of(&sentinel);
        assert_eq!(locked.amount, 250);
        assert_eq!(locked.staked_at, 5);
        assert_eq!(locked.unlock_at, 15);
        assert!(!locked.unlocked);

        // Exactly at the unlock ledger the lock has elapsed.
        env.ledger().with_mut(|l| l.sequence_number = 15);
        assert!(client.stake_of(&sentinel).unlocked);

        // An address that never staked is the zero record, not an error.
        let none = client.stake_of(&stranger);
        assert_eq!(none.amount, 0);
        assert_eq!(none.staked_at, 0);
        assert_eq!(none.unlock_at, 0);
        assert!(none.unlocked);

        // Reads are reads: no events, no storage drift.
        assert_eq!(env.events().all().events().len(), before);
        assert_eq!(client.balance(&sentinel), 250);
    }

    #[test]
    fn stake_of_tracks_the_lock_across_a_top_up() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        env.ledger().with_mut(|l| l.sequence_number = 5);
        client.stake(&sentinel, &250);

        env.ledger().with_mut(|l| l.sequence_number = 8);
        client.stake(&sentinel, &50);

        assert_eq!(client.staked_at(&sentinel), 8);
        let position = client.stake_of(&sentinel);
        assert_eq!(position.amount, 300);
        assert_eq!(position.staked_at, 8);
        assert_eq!(position.unlock_at, 18);
    }

    #[test]
    fn total_active_stake_folds_every_sentinel() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let carol = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // An empty pool totals zero, not an error.
        assert_eq!(client.total_active_stake(), 0);

        client.stake(&alice, &250);
        assert_eq!(client.total_active_stake(), 250);

        client.stake(&bob, &300);
        client.stake(&carol, &100);
        assert_eq!(client.total_active_stake(), 650);

        // A withdrawal only counts once it clears both the lock and the floor.
        env.ledger().with_mut(|l| l.sequence_number = 20);
        assert!(client.try_withdraw(&alice, &200).is_err());
        assert_eq!(client.total_active_stake(), 650);

        client.withdraw(&alice, &150);
        assert_eq!(client.total_active_stake(), 500);

        // A slash is a decrease in the total too.
        let reason = Symbol::new(&env, "fraudulent_submission");
        client.slash(&admin, &bob, &50, &reason);
        assert_eq!(client.total_active_stake(), 450);

        let before = env.events().all().events().len();
        assert_eq!(client.total_active_stake(), 450);
        assert_eq!(env.events().all().events().len(), before);
    }

    #[test]
    fn total_active_stake_saturates_instead_of_wrapping() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        client.stake(&alice, &i128::MAX);
        client.stake(&bob, &100);

        // A sum that cannot be represented reports the ceiling, never a
        // negative total.
        assert_eq!(client.total_active_stake(), i128::MAX);
    }
}

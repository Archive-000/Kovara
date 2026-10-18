#![no_std]
//! `SentinelPool` — manages verifier staking, governance thresholds, withdrawals,
//! and fraud slashing for the sentinel set.
//!
//! # The withdrawal lock is a window, not a counter
//!
//! A sentinel's lock is stored as a single ledger — `staked_at` — and the lock
//! runs until `staked_at + lock_period`. Everything about the lock follows from
//! that one value being an *absolute ledger* rather than a countdown:
//!
//! * **Arithmetic is checked, not saturating.** `unlock_at` is derived with
//!   `checked_add`; an unrepresentable window returns
//!   [`Error::InvalidLockPeriod`] instead of silently pinning itself to
//!   `u64::MAX`, which would be a lock that never expires.
//! * **A top-up does not move the window.** While the lock is still running,
//!   `stake` leaves `staked_at` untouched. A sentinel that keeps topping up
//!   cannot push its own unlock date further out, and — equally important — a
//!   top-up cannot be used to escape a lock that is about to expire by resetting
//!   it. Once the window has elapsed, the next `stake` starts a fresh one.
//!
//! # A slash is identified by a proof, not by a (sentinel, reason) pair
//!
//! Replay protection used to key on `(sentinel, reason)` — a pair of a contract
//! address and a `Symbol`. That key is not unique: two genuinely distinct
//! incidents against the same sentinel can share a reason code, and the second
//! one is then refused as a replay of the first. Conversely a single incident
//! relabelled with a different reason slips through twice.
//!
//! `slash` now takes an explicit `proof_id`, chosen by the caller and required
//! to be globally unique within the pool. A [`SlashProof`] record is stored
//! under it, so "this identifier has been used" is a fact the contract can
//! answer, and an auditor can join a slash back to the evidence that justified
//! it.
//!
//! # Slash amounts are bounded before anything is written
//!
//! `amount` is checked against three bounds in a fixed order — positive, at most
//! the current stake, leaving at least `min_stake` behind — and the balance map
//! is written only after all three pass. Each failure has its own error, so a
//! caller can tell a malformed amount from an unaffordable one.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Map,
    String, Symbol,
};

/// Upper bound on the withdrawal lock period, in ledger sequence numbers.
///
/// The lock is `unlock_at = staked_at + lock_period`, and `staked_at` is a
/// ledger sequence number. A lock period large enough to overflow that addition
/// would never elapse, which is a permanent freeze rather than a slow
/// withdrawal. Bounding the parameter keeps every reachable unlock ledger
/// representable, and the arithmetic is still checked at runtime.
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
    /// `staked_at + lock_period` is not representable as a `u64`. Append-only
    /// discriminant; see the ABI note in `README.md`.
    InvalidLockPeriod = 9,
    /// A slash amount was zero or negative.
    InvalidSlashAmount = 10,
    /// A slash amount exceeded the sentinel's current stake.
    SlashExceedsStake = 11,
    /// A slash was submitted without a proof identifier.
    InvalidProofId = 12,
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
    /// `proof_id` -> [`SlashProof`].
    ///
    /// The key is the caller's unique identifier, not a `(sentinel, reason)`
    /// pair; see the module docs for why the pair was not unique enough.
    SlashProofs,
}

/// The record stored under a slash proof identifier.
///
/// Keeping the whole record — not a bare `true` — is what makes the identifier
/// useful: an auditor can ask the contract "which proof is this?" and get the
/// sentinel, the amount, the reason and the ledger back, instead of having to
/// reconstruct that from the event stream.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlashProof {
    /// The sentinel the slash was applied to.
    pub sentinel: Address,
    /// The reason code, retained as an event topic for filtering. It is
    /// descriptive only; uniqueness comes from the proof identifier.
    pub reason: Symbol,
    /// The amount removed from the stake.
    pub amount: i128,
    /// Ledger sequence number the slash was applied on.
    pub slashed_at: u64,
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
    /// The caller's unique proof identifier. **Not** a topic: it is high-cardinality
    /// and would blow up the topic list of an audit consumer that subscribes to
    /// "every slash for this reason".
    pub proof_id: String,
    pub amount: i128,
    /// The sentinel's stake after the slash.
    pub remaining: i128,
    /// Ledger sequence number the slash was applied on.
    pub slashed_at: u64,
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
        if lock_period == 0 || lock_period > MAX_LOCK_PERIOD {
            return Err(Error::InvalidLockPeriod);
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
            .set(&DataKey::SlashProofs, &Map::<String, SlashProof>::new(&env));

        Ok(())
    }

    /// # Deposit stake
    ///
    /// # Lock extension policy
    ///
    /// **A top-up never extends a lock that is still running.** `staked_at` is
    /// left untouched while `now < staked_at + lock_period`, and is set to the
    /// current ledger once that window has elapsed.
    ///
    /// Both halves of that matter. Allowing the top-up to move the window would
    /// let a sentinel push their own unlock date out indefinitely by depositing
    /// one unit repeatedly — the lock would be a fee, not a constraint.
    /// Resetting it unconditionally on every deposit would be worse: a top-up
    /// landing one ledger before expiry would silently extend the lock by the
    /// full `lock_period`, which is a trap rather than a policy.
    ///
    /// # Errors
    /// * [`Error::InvalidStake`] — `amount <= 0`.
    /// * [`Error::StakeBelowMinimum`] — the resulting balance is under
    ///   `min_stake`.
    /// * [`Error::InvalidLockPeriod`] — the stored lock window is not
    ///   representable.
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

        let mut staked_at: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&DataKey::StakedAt)
            .unwrap_or_else(|| Map::new(&env));

        let now = u64::from(env.ledger().sequence());

        // `Option`, not `unwrap_or(0)`: genesis ledger 0 is a real `staked_at`
        // and must not be confused with "never staked".
        let existing: Option<u64> = staked_at.get(sentinel.clone());
        let effective_staked_at = match existing {
            // The window is still running — leave it alone.
            Some(last) if now < Self::lock_expiry_at(&env, last)? => last,
            // First stake, or the previous window has elapsed: start a new one.
            _ => now,
        };

        staked_at.set(sentinel.clone(), effective_staked_at);
        env.storage().instance().set(&DataKey::StakedAt, &staked_at);

        StakedEvent {
            sentinel: sentinel.clone(),
            amount,
            total: new_total,
        }
        .publish(&env);

        Ok(())
    }

/// The ledger at which a sentinel's current lock expires.
///
/// `staked_at + lock_period`, computed with `checked_add` — an unrepresentable
/// window is [`Error::InvalidLockPeriod`], not a saturated `u64::MAX` that
/// would read as "locked forever" without saying so.
///
/// `None` only for an address that has never staked. An address whose first
/// deposit landed on genesis reports `Some(lock_period)`, because its recorded
/// window start is a real `0`.
pub fn unlock_at(env: Env, sentinel: Address) -> Result<Option<u64>, Error> {
        let staked_at: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&DataKey::StakedAt)
            .unwrap_or_else(|| Map::new(&env));

        match staked_at.get(sentinel) {
            Some(last) => Self::lock_expiry_at(&env, last).map(Some),
            None => Ok(None),
        }
    }

    /// `staked_at + lock_period`, or [`Error::InvalidLockPeriod`] on overflow.
    fn lock_expiry_at(env: &Env, staked_at: u64) -> Result<u64, Error> {
        let lock_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriod)
            .unwrap_or(0);

        staked_at
            .checked_add(lock_period)
            .ok_or(Error::InvalidLockPeriod)
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

            // Compare ledgers directly rather than subtracting: `now - last`
            // saturates, and a saturated subtraction would silently report "the
            // lock has elapsed" for a window that has not been reached. Deriving
            // the unlock ledger with `checked_add` keeps the comparison exact at
            // both boundaries.
            let unlock = Self::lock_expiry_at(&env, last_staked)?;
            if u64::from(env.ledger().sequence()) < unlock {
                return Err(Error::LockPeriodActive);
            }
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

        WithdrawnEvent {
            sentinel: sentinel.clone(),
            amount,
            remaining,
        }
        .publish(&env);

        Ok(())
    }

    /// # Apply a fraud slash
    ///
    /// # Proof identity
    ///
    /// `proof_id` is the caller's unique identifier for the underlying fraud
    /// evidence. It must be globally unique within the pool and non-empty; the
    /// stored [`SlashProof`] is what makes the identifier auditable. Reusing one
    /// returns [`Error::FraudProofAlreadyUsed`].
    ///
    /// `reason` remains a `Symbol` and an event topic so consumers can filter by
    /// cause, but it carries **no** replay semantics: two real incidents against
    /// one sentinel can share a reason code and must still be slashable
    /// separately.
    ///
    /// # Amount bounds
    ///
    /// Checked in this order, all before the first write:
    ///
    /// | Bound | Error |
    /// | --- | --- |
    /// | `amount > 0` | [`Error::InvalidSlashAmount`] |
    /// | `amount <= current stake` | [`Error::SlashExceedsStake`] |
    /// | remainder `>= min_stake` | [`Error::StakeBelowMinimum`] |
    ///
    /// The three bounds are distinct conditions with distinct remedies, so each
    /// gets its own error rather than collapsing into one. Because the balance
    /// and the proof map are written only after all three pass, a refused slash
    /// leaves the stake unchanged **and** leaves the identifier unused.
    ///
    /// # Errors
    /// * [`Error::InvalidProofId`] — `proof_id` is empty.
    /// * [`Error::FraudProofAlreadyUsed`] — the identifier was already applied.
    /// * [`Error::InvalidSlashAmount`] — `amount <= 0`.
    /// * [`Error::SlashExceedsStake`] — `amount` exceeds the current stake.
    /// * [`Error::StakeBelowMinimum`] — the remainder would fall under
    ///   `min_stake`.
    /// * [`Error::Unauthorized`] — `admin` is not the configured admin.
    pub fn slash(
        env: Env,
        admin: Address,
        sentinel: Address,
        amount: i128,
        proof_id: String,
        reason: Symbol,
    ) -> Result<(), Error> {
        if proof_id.is_empty() {
            return Err(Error::InvalidProofId);
        }
        if amount <= 0 {
            return Err(Error::InvalidSlashAmount);
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

        let mut proofs: Map<String, SlashProof> = env
            .storage()
            .instance()
            .get(&DataKey::SlashProofs)
            .unwrap_or_else(|| Map::new(&env));

        if proofs.contains_key(proof_id.clone()) {
            return Err(Error::FraudProofAlreadyUsed);
        }

        let mut stakes: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&DataKey::Stakes)
            .unwrap_or_else(|| Map::new(&env));

        let current = stakes.get(sentinel.clone()).unwrap_or(0);
        if amount > current {
            return Err(Error::SlashExceedsStake);
        }

        let remaining = current
            .checked_sub(amount)
            .ok_or(Error::SlashExceedsStake)?;

        let min_stake = Self::min_stake(env.clone());
        if remaining < min_stake {
            return Err(Error::StakeBelowMinimum);
        }

        stakes.set(sentinel.clone(), remaining);
        env.storage().instance().set(&DataKey::Stakes, &stakes);

        proofs.set(
            proof_id.clone(),
            SlashProof {
                sentinel: sentinel.clone(),
                reason: reason.clone(),
                amount,
                slashed_at: u64::from(env.ledger().sequence()),
            },
        );
        env.storage().instance().set(&DataKey::SlashProofs, &proofs);

        SlashedEvent {
            sentinel: sentinel.clone(),
            reason,
            proof_id,
            amount,
            remaining,
            slashed_at: u64::from(env.ledger().sequence()),
        }
        .publish(&env);

        Ok(())
    }

    /// The proof record stored under `proof_id`, if any.
    ///
    /// The counterpart to `slash`'s uniqueness check: an auditor holding an
    /// identifier from an event or an off-chain case file can ask the contract
    /// what it was applied to, for how much, and on which ledger — without
    /// replaying the event stream.
    pub fn slash_proof(env: Env, proof_id: String) -> Option<SlashProof> {
        let proofs: Map<String, SlashProof> = env
            .storage()
            .instance()
            .get(&DataKey::SlashProofs)
            .unwrap_or_else(|| Map::new(&env));

        proofs.get(proof_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    fn proof(env: &Env, text: &str) -> String {
        String::from_str(env, text)
    }

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
        let case_42 = proof(&env, "case-42");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        client.slash(&admin, &sentinel, &50, &case_42, &reason);

        assert_eq!(client.balance(&sentinel), 200);
        assert!(client
            .try_slash(&admin, &sentinel, &10, &case_42, &reason)
            .is_err());
    }

    // ── CT-39: lock extension policy ────────────────────────────────────────

    #[test]
    fn a_top_up_does_not_extend_a_lock_that_is_still_running() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        env.ledger().with_mut(|l| l.sequence_number = 100);
        client.stake(&sentinel, &250);
        assert_eq!(client.unlock_at(&sentinel), Some(110));

        // Top-ups at 105 and 109 — both inside the window.
        env.ledger().with_mut(|l| l.sequence_number = 105);
        client.stake(&sentinel, &10);
        env.ledger().with_mut(|l| l.sequence_number = 109);
        client.stake(&sentinel, &10);

        // The window still ends at 110, not at 119.
        assert_eq!(client.unlock_at(&sentinel), Some(110));
        assert_eq!(client.balance(&sentinel), 270);

        // And the withdrawal boundary is unchanged by the top-ups.
        env.ledger().with_mut(|l| l.sequence_number = 109);
        assert!(client.try_withdraw(&sentinel, &50).is_err());

        env.ledger().with_mut(|l| l.sequence_number = 110);
        client.withdraw(&sentinel, &50);
        assert_eq!(client.balance(&sentinel), 220);
    }

    #[test]
    fn a_top_up_after_expiry_starts_a_fresh_window() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        env.ledger().with_mut(|l| l.sequence_number = 100);
        client.stake(&sentinel, &250);
        assert_eq!(client.unlock_at(&sentinel), Some(110));

        // Exactly at expiry: the window is over, so this top-up opens a new one.
        env.ledger().with_mut(|l| l.sequence_number = 110);
        client.stake(&sentinel, &10);
        assert_eq!(client.unlock_at(&sentinel), Some(120));

        // The new window is enforced from its own start, not the old one.
        env.ledger().with_mut(|l| l.sequence_number = 115);
        assert!(client.try_withdraw(&sentinel, &50).is_err());

        env.ledger().with_mut(|l| l.sequence_number = 120);
        client.withdraw(&sentinel, &50);
        assert_eq!(client.balance(&sentinel), 220);
    }

    #[test]
    fn a_genesis_stake_is_not_mistaken_for_never_staked() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // Ledger 0: `staked_at == 0` is a real value here, not "absent".
        assert_eq!(env.ledger().sequence(), 0);
        client.stake(&sentinel, &250);
        assert_eq!(client.unlock_at(&sentinel), Some(10));

        // A top-up at ledger 3 must keep the genesis window rather than treat
        // it as a first-ever stake and reset the clock to 3.
        env.ledger().with_mut(|l| l.sequence_number = 3);
        client.stake(&sentinel, &10);
        assert_eq!(client.unlock_at(&sentinel), Some(10));

        env.ledger().with_mut(|l| l.sequence_number = 9);
        assert!(client.try_withdraw(&sentinel, &50).is_err());
        env.ledger().with_mut(|l| l.sequence_number = 10);
        client.withdraw(&sentinel, &50);
    }

    #[test]
    fn an_address_that_never_staked_has_no_unlock_ledger() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let stranger = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        assert_eq!(client.unlock_at(&stranger), None);
    }

    // ── CT-40: checked lock-period arithmetic ────────────────────────────────

    #[test]
    fn the_lock_period_is_bounded_at_initialization() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        env.mock_all_auths();

        assert!(client.try_initialize(&admin, &100, &0, &2).is_err());
        assert!(client
            .try_initialize(&admin, &100, &(MAX_LOCK_PERIOD + 1), &2)
            .is_err());

        // The largest supported period is accepted and the largest reachable
        // ledger still produces a representable unlock ledger.
        client.initialize(&admin, &100, &MAX_LOCK_PERIOD, &2);

        let sentinel = Address::generate(&env);
        env.ledger().with_mut(|l| l.sequence_number = u32::MAX);
        client.stake(&sentinel, &250);

        // u32::MAX + MAX_LOCK_PERIOD is representable in u64, so the check
        // passes and the window is real rather than saturated.
        let expected = u64::from(u32::MAX) + MAX_LOCK_PERIOD;
        assert_eq!(client.unlock_at(&sentinel), Some(expected));

        env.ledger().with_mut(|l| l.sequence_number = u32::MAX);
        assert!(client.try_withdraw(&sentinel, &50).is_err());
    }

    #[test]
    fn a_lock_window_that_would_overflow_is_reported_not_saturated() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &MAX_LOCK_PERIOD, &2);

        let sentinel = Address::generate(&env);
        let overflowed = u64::MAX - (MAX_LOCK_PERIOD - 1);

        // Write the window start directly so the test can reach a ledger the
        // host could never set through `l.sequence_number`.
        env.as_contract(&contract_id, || {
            let mut staked_at: Map<Address, u64> = env
                .storage()
                .instance()
                .get(&DataKey::StakedAt)
                .unwrap_or_else(|| Map::new(&env));
            staked_at.set(sentinel.clone(), overflowed);
            env.storage().instance().set(&DataKey::StakedAt, &staked_at);

            let mut stakes: Map<Address, i128> = env
                .storage()
                .instance()
                .get(&DataKey::Stakes)
                .unwrap_or_else(|| Map::new(&env));
            stakes.set(sentinel.clone(), 250);
            env.storage().instance().set(&DataKey::Stakes, &stakes);
        });

        // `overflowed + MAX_LOCK_PERIOD` exceeds `u64::MAX`.
        assert!(client.try_unlock_at(&sentinel).is_err());
        assert!(client.try_withdraw(&sentinel, &50).is_err());
        // Balance preserved by both refusals.
        assert_eq!(client.balance(&sentinel), 250);
    }

    // ── CT-41: explicit slash amount bounds ──────────────────────────────────

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn a_zero_slash_amount_is_rejected_with_its_own_error() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        client.slash(&admin, &sentinel, &0, &proof(&env, "case-1"), &reason);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn a_slash_larger_than_the_stake_is_rejected_with_its_own_error() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);
        client.slash(
            &admin,
            &sentinel,
            &251,
            &proof(&env, "case-1"),
            &reason,
        );
    }

    #[test]
    fn a_refused_slash_changes_neither_stake_nor_proof_state() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");
        let case_1 = proof(&env, "case-1");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);

        // Every distinct refusal, against the same identifier.
        assert!(client
            .try_slash(&admin, &sentinel, &0, &case_1, &reason)
            .is_err());
        assert!(client
            .try_slash(&admin, &sentinel, &-50, &case_1, &reason)
            .is_err());
        assert!(client
            .try_slash(&admin, &sentinel, &i128::MIN, &case_1, &reason)
            .is_err());
        assert!(client
            .try_slash(&admin, &sentinel, &1_000, &case_1, &reason)
            .is_err());
        assert!(client
            .try_slash(&admin, &sentinel, &i128::MAX, &case_1, &reason)
            .is_err());
        // Would leave the sentinel under the minimum.
        assert!(client
            .try_slash(&admin, &sentinel, &200, &case_1, &reason)
            .is_err());
        // Empty identifier.
        assert!(client
            .try_slash(&admin, &sentinel, &50, &proof(&env, ""), &reason)
            .is_err());

        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.slash_proof(&case_1), None);

        // Because none of those refusals consumed the identifier, it is still
        // usable exactly once.
        client.slash(&admin, &sentinel, &50, &case_1, &reason);
        assert_eq!(client.balance(&sentinel), 200);
        assert!(client.slash_proof(&case_1).is_some());
    }

    #[test]
    fn a_slash_of_exactly_the_available_stake_is_refused_by_the_floor() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);

        // Exactly the balance is within the "exceeds stake" bound but breaks the
        // minimum-stake floor, which is the error that must surface.
        assert!(client
            .try_slash(&admin, &sentinel, &250, &proof(&env, "case-1"), &reason)
            .is_err());
        assert_eq!(client.balance(&sentinel), 250);

        // The largest slash the floor allows.
        client.slash(
            &admin,
            &sentinel,
            &150,
            &proof(&env, "case-2"),
            &reason,
        );
        assert_eq!(client.balance(&sentinel), 100);
    }

    // ── CT-42: collision-resistant proof identity ───────────────────────────

    #[test]
    fn two_incidents_sharing_a_reason_code_are_both_slashable() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &1_000);

        // Same sentinel, same reason, different evidence: the old
        // `(sentinel, reason)` key refused the second one.
        client.slash(&admin, &sentinel, &100, &proof(&env, "case-2024-11"), &reason);
        client.slash(&admin, &sentinel, &200, &proof(&env, "case-2025-03"), &reason);

        assert_eq!(client.balance(&sentinel), 700);
        assert!(client.slash_proof(&proof(&env, "case-2024-11")).is_some());
        assert!(client.slash_proof(&proof(&env, "case-2025-03")).is_some());
    }

    #[test]
    fn the_proof_identifier_alone_bounds_replay() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");
        let other_reason = Symbol::new(&env, "late_report");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&alice, &400);
        client.stake(&bob, &400);

        let case_1 = proof(&env, "case-1");

        client.slash(&admin, &alice, &100, &case_1.clone(), &reason);

        // A different sentinel: still refused. Replay protection is on the
        // identifier, not on the (sentinel, reason) pair — otherwise the same
        // evidence could be used against a second address.
        assert!(client
            .try_slash(&admin, &bob, &100, &case_1, &reason)
            .is_err());
        // A different reason: still refused, for the same reason.
        assert!(client
            .try_slash(&admin, &alice, &100, &case_1, &other_reason)
            .is_err());
        // A different sentinel *and* a different reason: still refused.
        assert!(client
            .try_slash(&admin, &bob, &100, &case_1, &other_reason)
            .is_err());

        assert_eq!(client.balance(&alice), 300);
        assert_eq!(client.balance(&bob), 400);
    }

    #[test]
    fn a_proof_record_describes_what_it_was_applied_to() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");
        let case_1 = proof(&env, "case-1");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);

        assert_eq!(client.slash_proof(&case_1), None);

        env.ledger().with_mut(|l| l.sequence_number = 77);
        client.slash(&admin, &sentinel, &50, &case_1.clone(), &reason);

        assert_eq!(
            client.slash_proof(&case_1),
            Some(SlashProof {
                sentinel: sentinel.clone(),
                reason,
                amount: 50,
                slashed_at: 77,
            })
        );

        // Unknown identifiers report `None` rather than failing.
        assert_eq!(client.slash_proof(&proof(&env, "case-404")), None);
    }

    #[test]
    fn an_unauthorized_slash_neither_moves_stake_nor_burns_the_proof() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");
        let case_1 = proof(&env, "case-1");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);

        assert!(client
            .try_slash(&attacker, &sentinel, &50, &case_1.clone(), &reason)
            .is_err());
        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.slash_proof(&case_1), None);

        // The failed attempt did not consume the identifier.
        client.slash(&admin, &sentinel, &50, &case_1.clone(), &reason);
        assert_eq!(client.balance(&sentinel), 200);
    }
}

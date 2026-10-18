#![no_std]
//! `SentinelPool` — manages verifier staking, governance thresholds, withdrawals,
//! and fraud slashing for the sentinel set.
//!
//! # One aggregate, maintained not recomputed
//!
//! `total_stake` is a stored running total, updated by `stake`, `withdraw` and
//! `slash` with `checked_add` / `checked_sub`. Every mutation writes the
//! per-address balance and the aggregate in the same call, and checks the
//! arithmetic **before** either write, so the aggregate can never disagree with
//! the balances it summarizes and an arithmetic failure leaves both untouched.
//!
//! # Emergency pause
//!
//! `pause` stops value movement without freezing the pool's configuration.
//! [`SentinelPool::pause`] carries the full matrix; the short version is that
//! `stake`, `withdraw` and `slash` refuse with [`Error::PoolPaused`], while the
//! pause control itself, the configuration setters and every read keep working —
//! because the controls you need during an incident are exactly the ones that
//! must not be frozen.
//!
//! # Leaving the set is a lifecycle event
//!
//! A sentinel with a non-zero balance cannot withdraw down to zero: it would
//! silently de-register the address with nothing recording why. `deregister` is
//! that record. After deregistration the withdrawal lock still applies, and a
//! further challenge period must elapse before the balance can leave the pool —
//! so a sentinel cannot deregister and immediately withdraw to escape a slash
//! review that is still running.
//!
//! `is_eligible` is the query that ties these together: it answers "would this
//! address count as an active sentinel at ledger N?" in one read, without the
//! caller re-deriving the floor, the lock and the deregistration state by hand.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Map,
    Symbol,
};

/// Default challenge period, in ledger sequence numbers: thirty days.
///
/// The gap between deregistering and being able to withdraw the balance. It
/// exists so that a slash under review when a sentinel leaves still has a live
/// stake to act on.
pub const DEFAULT_CHALLENGE_PERIOD_LEDGERS: u64 = 2_592_000;

/// Upper bound on the challenge period, in ledger sequence numbers.
///
/// The deadline is `deregistered_at + challenge_period` and is stored in a
/// `u64`. Bounding the period keeps the deadline representable for every
/// reachable `deregistered_at`; the arithmetic is still checked at use.
pub const MAX_CHALLENGE_PERIOD_LEDGERS: u64 = 20_000_000;

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
    /// The pool is paused. Append-only discriminant; see the ABI note in
    /// `README.md`.
    PoolPaused = 9,
    /// The challenge period is zero or above [`MAX_CHALLENGE_PERIOD_LEDGERS`].
    InvalidChallengePeriod = 10,
    /// A full exit was attempted by an address that never deregistered.
    NotDeregistered = 11,
    /// The challenge period after deregistration has not elapsed.
    ChallengePeriodActive = 12,
    /// The address is already deregistered.
    AlreadyDeregistered = 13,
    /// No governance address is configured to operate the pause control.
    PauseNotAuthorized = 14,
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
    /// Stored sum of every entry in [`DataKey::Stakes`].
    TotalStake,
    /// Whether the pool is paused.
    Paused,
    /// The address authorized to pause and unpause.
    Governance,
    /// Challenge period in ledger sequence numbers.
    ChallengePeriod,
    /// `sentinel` -> [`Deregistration`].
    Deregistrations,
}

/// A recorded deregistration.
///
/// The challenge period is copied onto the record rather than read from the
/// current configuration when the exit is attempted. Without that copy,
/// shortening the period would retroactively cut short a challenge period that
/// is already running — the same hole an appeal window has.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Deregistration {
    /// Ledger the address deregistered on.
    pub at: u64,
    /// The challenge period in force at that moment.
    pub challenge_period: u64,
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

/// Emitted when the pause control changes state.
#[contractevent]
#[derive(Clone)]
pub struct PauseChangedEvent {
    #[topic]
    pub actor: Address,
    /// Whether the pool is paused after this call.
    pub paused: bool,
    /// The pool-wide aggregate after this call.
    pub total_stake: i128,
}

/// Emitted when a sentinel leaves the set.
#[contractevent]
#[derive(Clone)]
pub struct DeregisteredEvent {
    #[topic]
    pub sentinel: Address,
    /// Ledger the deregistration was recorded on.
    pub deregistered_at: u64,
    /// The ledger from which a full exit becomes available.
    pub withdrawable_at: u64,
    /// The balance the sentinel still has at stake during the challenge period.
    pub stake_at_deregistration: i128,
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

        // The aggregate starts at zero and is written by every mutation that
        // moves a balance. Seeding it here means it exists from the first
        // ledger, so a reader never has to distinguish "no stake" from "never
        // initialized".
        env.storage().instance().set(&DataKey::TotalStake, &0i128);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage()
            .instance()
            .set(&DataKey::ChallengePeriod, &DEFAULT_CHALLENGE_PERIOD_LEDGERS);
        env.storage()
            .instance()
            .set(&DataKey::Deregistrations, &Map::<Address, u64>::new(&env));

        Ok(())
    }

    /// # The pool-wide aggregate
    ///
    /// A stored running total, not a fold over [`DataKey::Stakes`] at read
    /// time. `stake`, `withdraw` and `slash` each update it with checked
    /// arithmetic in the same call that updates the balance, so it cannot drift
    /// away from the balances it summarizes.
    pub fn total_stake(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalStake)
            .unwrap_or(0)
    }

    /// Whether the pool is paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// The challenge period in force, in ledger sequence numbers.
    pub fn challenge_period(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::ChallengePeriod)
            .unwrap_or(DEFAULT_CHALLENGE_PERIOD_LEDGERS)
    }

    /// Fail unless the pool is not paused.
    fn require_not_paused(env: &Env) -> Result<(), Error> {
        let paused: bool = env.storage().instance().get(&DataKey::Paused).unwrap_or(false);
        if paused {
            Err(Error::PoolPaused)
        } else {
            Ok(())
        }
    }

    /// Fail unless `caller` is the configured governance address.
    ///
    /// There is no fallback to the caller: a pool with no governance address
    /// cannot be paused by anyone, rather than being pausable by whichever
    /// address happened to sign the transaction.
    fn require_governance(env: &Env, caller: &Address) -> Result<(), Error> {
        let stored: Option<Address> = env.storage().instance().get(&DataKey::Governance);
        match stored {
            Some(governance) if governance == *caller => Ok(()),
            Some(_) => Err(Error::Unauthorized),
            None => Err(Error::PauseNotAuthorized),
        }
    }

    /// Fail unless `caller` is the configured admin.
    fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
        let stored: Option<Address> = env.storage().instance().get(&DataKey::Admin);
        match stored {
            Some(admin) if admin == *caller => Ok(()),
            _ => Err(Error::Unauthorized),
        }
    }

    /// # Rotate the pause authority
    ///
    /// Admin-only. Pausing is an operational power — it stops the pool moving
    /// value — so it is held by governance rather than by whoever can change
    /// the configuration. Until this is called, `pause` and `unpause` are
    /// refused with [`Error::PauseNotAuthorized`].
    ///
    /// # Errors
    /// * [`Error::Unauthorized`] — `admin` is not the configured admin.
    pub fn set_governance(env: Env, admin: Address, governance: Address) -> Result<(), Error> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);

        Ok(())
    }

    /// # Set the deregistration challenge period
    ///
    /// Admin-only. Not retroactive: a deregistration records the ledger it
    /// happened on, so changing the period afterwards cannot shorten a challenge
    /// period that is already running.
    ///
    /// # Errors
    /// * [`Error::InvalidChallengePeriod`] — zero, or above
    ///   [`MAX_CHALLENGE_PERIOD_LEDGERS`].
    /// * [`Error::Unauthorized`] — `admin` is not the configured admin.
    pub fn set_challenge_period(env: Env, admin: Address, period: u64) -> Result<(), Error> {
        if period == 0 || period > MAX_CHALLENGE_PERIOD_LEDGERS {
            return Err(Error::InvalidChallengePeriod);
        }

        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        env.storage()
            .instance()
            .set(&DataKey::ChallengePeriod, &period);

        Ok(())
    }

    /// # Pause the pool
    ///
    /// # The pause matrix
    ///
    /// | Call | Paused | Not paused |
    /// | --- | --- | --- |
    /// | `stake` | `PoolPaused` | succeeds |
    /// | `withdraw` | `PoolPaused` | succeeds |
    /// | `slash` | `PoolPaused` | succeeds |
    /// | `deregister` | `PoolPaused` | succeeds |
    /// | `pause` | succeeds (idempotent) | succeeds |
    /// | `unpause` | succeeds | `PoolPaused` refused — nothing to undo |
    /// | `set_threshold`, `set_min_stake` | succeeds | succeeds |
    /// | `set_governance`, `set_challenge_period` | succeeds | succeeds |
    /// | every read | succeeds | succeeds |
    ///
    /// Value movement stops; configuration and the emergency controls do not.
    /// A pause that also froze `unpause` or the configuration setters would be a
    /// one-way trip for the operator dealing with the incident.
    ///
    /// # Errors
    /// * [`Error::PauseNotAuthorized`] — no governance address is configured.
    /// * [`Error::Unauthorized`] — `caller` is not the governance address.
    pub fn pause(env: Env, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        env.storage().instance().set(&DataKey::Paused, &true);

        PauseChangedEvent {
            actor: caller,
            paused: true,
            total_stake: Self::total_stake(env.clone()),
        }
        .publish(&env);

        Ok(())
    }

    /// Lift the pause. Only meaningful while paused — unpausing a running pool
    /// is refused rather than silently accepted, so a caller that believes it
    /// lifted an incident cannot be wrong about it.
    pub fn unpause(env: Env, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        if !Self::is_paused(env.clone()) {
            return Err(Error::PoolPaused);
        }

        env.storage().instance().set(&DataKey::Paused, &false);

        PauseChangedEvent {
            actor: caller,
            paused: false,
            total_stake: Self::total_stake(env.clone()),
        }
        .publish(&env);

        Ok(())
    }

    /// # Add to a sentinel's stake
    ///
    /// Refused while the pool is paused, and refused for a deregistered address
    /// with [`Error::AlreadyDeregistered`]. Allowing a top-up during the
    /// challenge period would let a sentinel reset it and escape the delay by
    /// depositing a single unit; a deregistered address must either finish its
    /// exit or wait it out.
    ///
    /// The new balance and the aggregate are both derived with checked
    /// arithmetic **before** either is written, so an overflow leaves the
    /// balance and the total exactly as they were.
    ///
    /// # Errors
    /// * [`Error::PoolPaused`] — the pool is paused.
    /// * [`Error::AlreadyDeregistered`] — the address is on its way out.
    /// * [`Error::InvalidStake`] — `amount <= 0`, or the resulting balance
    ///   would overflow `i128`.
    /// * [`Error::StakeBelowMinimum`] — the resulting balance is under
    ///   `min_stake`.
    pub fn stake(env: Env, sentinel: Address, amount: i128) -> Result<(), Error> {
        Self::require_not_paused(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidStake);
        }

        sentinel.require_auth();

        if Self::deregistration(env.clone(), sentinel.clone()).is_some() {
            return Err(Error::AlreadyDeregistered);
        }

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

        // The aggregate moves by the same `amount` as the balance, checked
        // before either write. Reading it once and reusing the value keeps the
        // balance and the total derived from one arithmetic step rather than two
        // that could drift.
        let aggregate = Self::total_stake(env.clone());
        let new_aggregate = aggregate
            .checked_add(amount)
            .ok_or(Error::InvalidStake)?;

        stakes.set(sentinel.clone(), new_total);
        env.storage().instance().set(&DataKey::Stakes, &stakes);
        env.storage()
            .instance()
            .set(&DataKey::TotalStake, &new_aggregate);

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
    /// Refused while the pool is paused.
    ///
    /// # Two withdrawal modes
    ///
    /// * **Registered sentinel** (the default). The remainder must stay at or
    ///   above `min_stake`; a withdrawal that would cross the floor is
    ///   [`Error::StakeBelowMinimum`]. Leaving the set has its own step —
    ///   [`Self::deregister`].
    /// * **Deregistered sentinel.** After [`Self::deregister`] and once the
    ///   challenge period has elapsed, the whole balance may be withdrawn and
    ///   the `min_stake` floor does not apply. The withdrawal lock still does.
    ///
    /// # Accounting order
    ///
    /// Checks run in this order and nothing is written until all of them pass:
    ///
    /// 1. the pool is not paused;
    /// 2. `amount > 0`;
    /// 3. `amount <= balance` — [`Error::InsufficientBalance`];
    /// 4. the withdrawal lock has elapsed — [`Error::LockPeriodActive`];
    /// 5. a full exit is claimed only by a deregistered sentinel past its
    ///    challenge period — [`Error::NotDeregistered`] or
    ///    [`Error::ChallengePeriodActive`];
    /// 6. `balance - amount` does not underflow — `checked_sub`;
    /// 7. the remainder clears the floor, unless this is an exit.
    ///
    /// # Errors
    /// * [`Error::PoolPaused`] — the pool is paused.
    /// * [`Error::InvalidStake`] — `amount <= 0`.
    /// * [`Error::InsufficientBalance`] — `amount` exceeds the balance.
    /// * [`Error::LockPeriodActive`] — the withdrawal lock has not elapsed.
    /// * [`Error::NotDeregistered`] — a full exit without deregistering first.
    /// * [`Error::ChallengePeriodActive`] — the challenge period has not
    ///   elapsed.
    /// * [`Error::StakeBelowMinimum`] — the remainder would fall under
    ///   `min_stake`.
    pub fn withdraw(env: Env, sentinel: Address, amount: i128) -> Result<(), Error> {
        Self::require_not_paused(&env)?;

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

        // A full exit is only available to a sentinel that deregistered and
        // waited out the challenge period. The deregistration is resolved
        // *before* the floor is consulted, so an ordinary partial withdrawal
        // that would dip under `min_stake` reports `StakeBelowMinimum` while a
        // deliberate full exit reports why *it* is not allowed yet.
        let is_exit = remaining == 0;

        if is_exit {
            let deregistration = Self::deregistration(env.clone(), sentinel.clone())
                .ok_or(Error::NotDeregistered)?;
            let withdrawable_at = deregistration
                .at
                .saturating_add(deregistration.challenge_period);
            if u64::from(env.ledger().sequence()) < withdrawable_at {
                return Err(Error::ChallengePeriodActive);
            }
        }

        if !is_exit {
            let min_stake = Self::min_stake(env.clone());
            if remaining < min_stake {
                return Err(Error::StakeBelowMinimum);
            }
        }

        let aggregate = Self::total_stake(env.clone());
        let new_aggregate = aggregate
            .checked_sub(amount)
            .ok_or(Error::InsufficientBalance)?;

        stakes.set(sentinel.clone(), remaining);
        env.storage().instance().set(&DataKey::Stakes, &stakes);
        env.storage()
            .instance()
            .set(&DataKey::TotalStake, &new_aggregate);

        // Completing the exit closes out the deregistration, so a returning
        // sentinel starts clean: no stale record, and a fresh challenge period
        // if it deregisters again.
        if is_exit {
            let mut deregistrations: Map<Address, Deregistration> = env
                .storage()
                .instance()
                .get(&DataKey::Deregistrations)
                .unwrap_or_else(|| Map::new(&env));
            deregistrations.remove(sentinel.clone());
            env.storage()
                .instance()
                .set(&DataKey::Deregistrations, &deregistrations);
        }

        WithdrawnEvent {
            sentinel: sentinel.clone(),
            amount,
            remaining,
        }
        .publish(&env);

        Ok(())
    }

    /// # Leave the sentinel set
    ///
    /// Records that `sentinel` is on its way out, starting the challenge period.
    /// The stake stays in the pool throughout — deregistering is not a
    /// withdrawal, and the balance is still there for a slash under review.
    ///
    /// Refused while the pool is paused, and refused twice: an address with no
    /// stake has nothing to deregister from, and an address that already has is
    /// not given a fresh challenge period.
    ///
    /// # Errors
    /// * [`Error::PoolPaused`] — the pool is paused.
    /// * [`Error::InvalidStake`] — the address holds no stake.
    /// * [`Error::AlreadyDeregistered`] — already deregistered.
    pub fn deregister(env: Env, sentinel: Address) -> Result<(), Error> {
        Self::require_not_paused(&env)?;

        sentinel.require_auth();

        if Self::deregistration(env.clone(), sentinel.clone()).is_some() {
            return Err(Error::AlreadyDeregistered);
        }

        let stake = Self::balance(env.clone(), sentinel.clone());
        if stake <= 0 {
            return Err(Error::InvalidStake);
        }

        let at = u64::from(env.ledger().sequence());
        let challenge_period = Self::challenge_period(env.clone());

        let mut deregistrations: Map<Address, Deregistration> = env
            .storage()
            .instance()
            .get(&DataKey::Deregistrations)
            .unwrap_or_else(|| Map::new(&env));
        deregistrations.set(
            sentinel.clone(),
            Deregistration {
                at,
                challenge_period,
            },
        );
        env.storage()
            .instance()
            .set(&DataKey::Deregistrations, &deregistrations);

        DeregisteredEvent {
            sentinel,
            deregistered_at: at,
            withdrawable_at: at.saturating_add(challenge_period),
            stake_at_deregistration: stake,
        }
        .publish(&env);

        Ok(())
    }

    /// The ledger a sentinel deregistered on, or `None` while registered.
    pub fn deregistered_at(env: Env, sentinel: Address) -> Option<u64> {
        Self::deregistration(env, sentinel).map(|d| d.at)
    }

    /// The ledger from which a deregistered sentinel may withdraw its balance.
    ///
    /// `None` while the address is still registered. The deadline is computed
    /// from the period that was in force at deregistration, so it is stable even
    /// if the admin changes the period afterwards.
    pub fn withdrawable_at(env: Env, sentinel: Address) -> Option<u64> {
        Self::deregistration(env, sentinel)
            .map(|d| d.at.saturating_add(d.challenge_period))
    }

    /// The recorded deregistration for `sentinel`, if any.
    fn deregistration(env: Env, sentinel: Address) -> Option<Deregistration> {
        let deregistrations: Map<Address, Deregistration> = env
            .storage()
            .instance()
            .get(&DataKey::Deregistrations)
            .unwrap_or_else(|| Map::new(&env));
        deregistrations.get(sentinel)
    }

    /// # Would this address count as an active sentinel at ledger `at_ledger`?
    ///
    /// One read, three conditions, all of which the contract itself enforces:
    ///
    /// 1. the balance is at or above `min_stake`;
    /// 2. the address is not deregistered;
    /// 3. the withdrawal lock has elapsed at `at_ledger`.
    ///
    /// The ledger is a parameter so a caller can ask the hypothetical question —
    /// "will this sentinel be eligible at the next halving?" — without waiting
    /// for it. It defaults to the current ledger for callers that just want the
    /// present-tense answer.
    ///
    /// Returns `false` rather than an error for an unknown address: "not
    /// eligible" is the answer, not a failure.
    pub fn is_eligible(env: Env, sentinel: Address, at_ledger: u32) -> bool {
        if Self::deregistered_at(env.clone(), sentinel.clone()).is_some() {
            return false;
        }

        let balance = Self::balance(env.clone(), sentinel.clone());
        if balance < Self::min_stake(env.clone()) {
            return false;
        }

        let lock_period: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LockPeriod)
            .unwrap_or(0);

        let staked_at: Map<Address, u64> = env
            .storage()
            .instance()
            .get(&DataKey::StakedAt)
            .unwrap_or_else(|| Map::new(&env));
        let last_staked = staked_at.get(sentinel).unwrap_or(0);
        let elapsed = u64::from(at_ledger).saturating_sub(last_staked);

        elapsed >= lock_period
    }

    /// Refused while the pool is paused, and the pool-wide aggregate moves with
    /// the slashed balance in the same checked step.
    ///
    /// # Errors
    /// * [`Error::PoolPaused`] — the pool is paused.
    pub fn slash(
        env: Env,
        admin: Address,
        sentinel: Address,
        amount: i128,
        reason: Symbol,
    ) -> Result<(), Error> {
        Self::require_not_paused(&env)?;

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

        let aggregate = Self::total_stake(env.clone());
        let new_aggregate = aggregate
            .checked_sub(amount)
            .ok_or(Error::InsufficientBalance)?;

        stakes.set(sentinel.clone(), remaining);
        env.storage().instance().set(&DataKey::Stakes, &stakes);
        env.storage()
            .instance()
            .set(&DataKey::TotalStake, &new_aggregate);

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

    // ── CT-47: the pool-wide aggregate ──────────────────────────────────────

    #[test]
    fn the_aggregate_tracks_stake_withdrawals_and_slashes() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // An empty pool totals zero, not an error.
        assert_eq!(client.total_stake(), 0);

        client.stake(&alice, &250);
        assert_eq!(client.total_stake(), 250);

        client.stake(&bob, &300);
        client.stake(&alice, &50);
        assert_eq!(client.total_stake(), 600);

        env.ledger().with_mut(|l| l.sequence_number = 20);
        client.withdraw(&alice, &150);
        assert_eq!(client.total_stake(), 450);

        let reason = Symbol::new(&env, "fraudulent_submission");
        client.slash(&admin, &bob, &50, &reason);
        assert_eq!(client.total_stake(), 400);
    }

    #[test]
    fn the_aggregate_never_disagrees_with_the_balances() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let carol = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        client.stake(&alice, &250);
        client.stake(&bob, &400);

        env.ledger().with_mut(|l| l.sequence_number = 20);
        client.withdraw(&alice, &50);

        // The invariant, spelled out: the total is the sum of the balances.
        assert_eq!(
            client.total_stake(),
            client.balance(&alice) + client.balance(&bob) + client.balance(&carol)
        );

        // A refused withdrawal moves neither side of the equality.
        assert!(client.try_withdraw(&alice, &i128::MAX).is_err());
        assert_eq!(
            client.total_stake(),
            client.balance(&alice) + client.balance(&bob) + client.balance(&carol)
        );
        assert_eq!(client.total_stake(), 600);
    }

    #[test]
    fn a_refused_stake_leaves_the_aggregate_untouched() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &1_000);

        // Overflow of the balance.
        assert!(client.try_stake(&sentinel, &i128::MAX).is_err());
        assert_eq!(client.total_stake(), 1_000);

        // Below the floor.
        assert!(client.try_stake(&sentinel, &1).is_err());
        assert_eq!(client.total_stake(), 1_000);

        assert_eq!(client.balance(&sentinel), 1_000);
    }

    #[test]
    fn the_aggregate_saturates_the_balance_it_summarizes() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        client.stake(&alice, &i128::MAX);
        assert_eq!(client.total_stake(), i128::MAX);

        // A second deposit cannot be added to the aggregate: refused, and the
        // balance is untouched rather than wrapped negative.
        assert!(client.try_stake(&bob, &100).is_err());
        assert_eq!(client.total_stake(), i128::MAX);
        assert_eq!(client.balance(&bob), 0);
    }

    // ── CT-48: eligibility query ────────────────────────────────────────────

    #[test]
    fn eligibility_is_the_floor_the_lock_and_the_deregistration() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // Never staked: not eligible.
        assert!(!client.is_eligible(&sentinel, &0));

        env.ledger().with_mut(|l| l.sequence_number = 5);
        client.stake(&sentinel, &250);

        // Funded but still locked.
        assert!(!client.is_eligible(&sentinel, &5));
        assert!(!client.is_eligible(&sentinel, &14));

        // Lock elapsed.
        assert!(client.is_eligible(&sentinel, &15));

        // The query is about the supplied ledger, not the current one: asking
        // about the past gives the answer for the past.
        assert!(!client.is_eligible(&sentinel, &5));
    }

    #[test]
    fn an_under_funded_or_deregistered_sentinel_is_never_eligible() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        env.ledger().with_mut(|l| l.sequence_number = 5);
        client.stake(&sentinel, &250);
        assert!(client.is_eligible(&sentinel, &20));

        // Raising the floor above the balance makes the sentinel ineligible
        // without touching its stake.
        client.set_min_stake(&admin, &500);
        assert!(!client.is_eligible(&sentinel, &20));
        assert_eq!(client.balance(&sentinel), 250);

        client.set_min_stake(&admin, &100);
        assert!(client.is_eligible(&sentinel, &20));

        // Deregistering ends eligibility immediately, even though the balance is
        // still there during the challenge period.
        client.deregister(&sentinel);
        assert!(!client.is_eligible(&sentinel, &20));
        assert_eq!(client.balance(&sentinel), 250);
    }

    #[test]
    fn eligibility_does_not_mutate_anything() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.stake(&sentinel, &250);

        let before = env.events().all().events().len();
        for ledger in [0_u32, 5, 15, 1_000] {
            let _ = client.is_eligible(&sentinel, &ledger);
        }
        assert_eq!(env.events().all().events().len(), before);
        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.total_stake(), 250);
    }

    // ── CT-49: emergency pause ──────────────────────────────────────────────

    #[test]
    fn a_paused_pool_stops_value_movement_but_not_its_controls() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let governance = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let reason = Symbol::new(&env, "fraudulent_submission");

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_governance(&admin, &governance);
        client.stake(&sentinel, &250);

        client.pause(&governance);
        assert!(client.is_paused());

        env.ledger().with_mut(|l| l.sequence_number = 20);

        // Frozen.
        assert!(client.try_stake(&sentinel, &50).is_err());
        assert!(client.try_withdraw(&sentinel, &50).is_err());
        assert!(client.try_slash(&governance, &sentinel, &50, &reason).is_err());
        assert!(client.try_deregister(&sentinel).is_err());

        // Still working: the controls and the configuration, because those are
        // exactly what you need during an incident.
        assert_eq!(client.total_stake(), 250);
        assert_eq!(client.balance(&sentinel), 250);
        client.set_threshold(&admin, &5);
        client.set_min_stake(&admin, &150);
        client.set_challenge_period(&admin, &100);
        // Pausing twice is idempotent, not an error.
        client.pause(&governance);

        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.total_stake(), 250);
        assert_eq!(client.threshold(), 5);
    }

    #[test]
    fn unpausing_restores_value_movement() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let governance = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_governance(&admin, &governance);
        client.stake(&sentinel, &250);

        client.pause(&governance);
        client.unpause(&governance);
        assert!(!client.is_paused());

        env.ledger().with_mut(|l| l.sequence_number = 20);
        client.withdraw(&sentinel, &50);
        assert_eq!(client.balance(&sentinel), 200);
        assert_eq!(client.total_stake(), 200);
    }

    #[test]
    fn the_pause_control_is_governance_only_with_no_fallback() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // Unconfigured: nobody, including the admin, can pause.
        assert!(client.try_pause(&admin).is_err());
        assert!(!client.is_paused());

        client.set_governance(&admin, &admin);

        // Configured: only the configured address.
        assert!(client.try_pause(&attacker).is_err());
        assert!(!client.is_paused());
        client.pause(&admin);
        assert!(client.is_paused());
    }

    #[test]
    fn a_pause_cannot_be_rotated_away_by_an_unauthorized_address() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let governance = Address::generate(&env);
        let attacker = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_governance(&admin, &governance);
        client.pause(&governance);

        // Only the admin can rotate the pause authority, not governance and
        // certainly not a stranger.
        assert!(client.try_set_governance(&governance, &attacker).is_err());
        assert!(client.try_set_governance(&attacker, &attacker).is_err());
        assert!(client.try_pause(&attacker).is_err());
        assert!(client.is_paused());
    }

    #[test]
    fn unpausing_a_running_pool_is_refused() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let governance = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_governance(&admin, &governance);

        assert!(client.try_unpause(&governance).is_err());

        client.pause(&governance);
        client.unpause(&governance);
        // Already lifted: refused rather than silently accepted.
        assert!(client.try_unpause(&governance).is_err());
    }

    // ── CT-50: exit after deregistration ────────────────────────────────────

    #[test]
    fn an_exit_needs_deregistration_and_then_the_challenge_period() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_challenge_period(&admin, &100);
        client.stake(&sentinel, &250);

        // Registered: a full exit is refused, and it is refused *because* the
        // sentinel never deregistered.
        env.ledger().with_mut(|l| l.sequence_number = 20);
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.deregistered_at(&sentinel), None);
        assert_eq!(client.withdrawable_at(&sentinel), None);

        client.deregister(&sentinel);
        assert_eq!(client.deregistered_at(&sentinel), Some(20));
        assert_eq!(client.withdrawable_at(&sentinel), Some(120));

        // Lock elapsed but the challenge period has not.
        env.ledger().with_mut(|l| l.sequence_number = 50);
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        assert_eq!(client.balance(&sentinel), 250);

        // One ledger before the challenge deadline.
        env.ledger().with_mut(|l| l.sequence_number = 119);
        assert!(client.try_withdraw(&sentinel, &250).is_err());

        // The deadline itself.
        env.ledger().with_mut(|l| l.sequence_number = 120);
        client.withdraw(&sentinel, &250);
        assert_eq!(client.balance(&sentinel), 0);
        assert_eq!(client.total_stake(), 0);
    }

    #[test]
    fn a_deregistered_sentinel_can_still_take_a_partial_withdrawal() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_challenge_period(&admin, &100);
        client.stake(&sentinel, &250);
        client.deregister(&sentinel);

        env.ledger().with_mut(|l| l.sequence_number = 20);

        // Before the challenge deadline, the floor still applies to a partial
        // withdrawal: the challenge period gates the *exit*, not ordinary
        // withdrawals, which were always allowed down to the floor.
        assert!(client.try_withdraw(&sentinel, &200).is_err());
        client.withdraw(&sentinel, &150);
        assert_eq!(client.balance(&sentinel), 100);

        env.ledger().with_mut(|l| l.sequence_number = 120);
        client.withdraw(&sentinel, &100);
        assert_eq!(client.balance(&sentinel), 0);
    }

    #[test]
    fn deregistration_is_refused_twice_and_without_a_stake() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);
        let stranger = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        // Nothing to deregister from.
        assert!(client.try_deregister(&stranger).is_err());

        client.stake(&sentinel, &250);
        client.deregister(&sentinel);

        // A second call would hand out a fresh challenge period.
        assert!(client.try_deregister(&sentinel).is_err());
        assert_eq!(client.deregistered_at(&sentinel), Some(0));
    }

    #[test]
    fn the_challenge_period_is_bounded_and_admin_only() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);

        assert_eq!(client.challenge_period(), DEFAULT_CHALLENGE_PERIOD_LEDGERS);

        assert!(client.try_set_challenge_period(&attacker, &10).is_err());
        assert!(client.try_set_challenge_period(&admin, &0).is_err());
        assert!(client
            .try_set_challenge_period(&admin, &(MAX_CHALLENGE_PERIOD_LEDGERS + 1))
            .is_err());
        assert_eq!(client.challenge_period(), DEFAULT_CHALLENGE_PERIOD_LEDGERS);

        client.set_challenge_period(&admin, &MAX_CHALLENGE_PERIOD_LEDGERS);
        assert_eq!(client.challenge_period(), MAX_CHALLENGE_PERIOD_LEDGERS);
    }

    #[test]
    fn shortening_the_challenge_period_does_not_shorten_a_running_one() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_challenge_period(&admin, &1_000);
        client.stake(&sentinel, &250);

        env.ledger().with_mut(|l| l.sequence_number = 1_000);
        client.deregister(&sentinel);
        assert_eq!(client.withdrawable_at(&sentinel), Some(2_000));

        client.set_challenge_period(&admin, &1);

        // The deadline recorded at deregistration stands.
        assert_eq!(client.withdrawable_at(&sentinel), Some(2_000));
        env.ledger().with_mut(|l| l.sequence_number = 1_500);
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        env.ledger().with_mut(|l| l.sequence_number = 2_000);
        client.withdraw(&sentinel, &250);
    }

    #[test]
    fn an_exited_sentinel_can_return_to_the_set() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_challenge_period(&admin, &100);
        client.stake(&sentinel, &250);

        client.deregister(&sentinel);
        env.ledger().with_mut(|l| l.sequence_number = 120);
        client.withdraw(&sentinel, &250);
        assert_eq!(client.balance(&sentinel), 0);
        assert_eq!(client.total_stake(), 0);

        // Completing the exit closed out the deregistration, so nothing is left
        // to block a fresh registration.
        assert_eq!(client.deregistered_at(&sentinel), None);
        assert_eq!(client.withdrawable_at(&sentinel), None);

        // Re-staking starts a new lock; eligibility follows it.
        env.ledger().with_mut(|l| l.sequence_number = 200);
        client.stake(&sentinel, &250);
        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.total_stake(), 250);
        assert!(!client.is_eligible(&sentinel, &200));
        assert!(client.is_eligible(&sentinel, &210));

        // A fresh full exit still needs its own deregistration and its own
        // challenge period.
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        client.deregister(&sentinel);
        assert_eq!(client.withdrawable_at(&sentinel), Some(300));
        env.ledger().with_mut(|l| l.sequence_number = 299);
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        env.ledger().with_mut(|l| l.sequence_number = 300);
        client.withdraw(&sentinel, &250);
        assert_eq!(client.balance(&sentinel), 0);
    }

    #[test]
    fn a_deregistered_sentinel_cannot_top_up_to_reset_the_challenge_period() {
        let env = Env::default();
        let contract_id = env.register_contract(None, SentinelPool);
        let client = SentinelPoolClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sentinel = Address::generate(&env);

        env.mock_all_auths();
        client.initialize(&admin, &100, &10, &2);
        client.set_challenge_period(&admin, &1_000);
        client.stake(&sentinel, &250);
        client.deregister(&sentinel);

        // A one-unit top-up must not be able to reset the recorded window.
        assert!(client.try_stake(&sentinel, &1).is_err());
        assert_eq!(client.balance(&sentinel), 250);
        assert_eq!(client.withdrawable_at(&sentinel), Some(1_000));

        env.ledger().with_mut(|l| l.sequence_number = 999);
        assert!(client.try_withdraw(&sentinel, &250).is_err());
        env.ledger().with_mut(|l| l.sequence_number = 1_000);
        client.withdraw(&sentinel, &250);
    }
}

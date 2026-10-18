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
    String, Symbol,
};

/// Upper bound on the withdrawal lock period, in ledger sequence numbers.
///
/// The lock is `unlock_at = staked_at + lock_period`, and `staked_at` is a
/// ledger sequence number. A lock period large enough to overflow that addition
/// would never elapse, which is a permanent freeze rather than a slow
/// withdrawal. Bounding the parameter keeps every reachable unlock ledger
/// representable, and the arithmetic is still checked at runtime.
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
    /// `staked_at + lock_period` is not representable as a `u64`. Append-only
    /// discriminant; see the ABI note in `README.md`.
    InvalidLockPeriod = 9,
    /// A slash amount was zero or negative.
    InvalidSlashAmount = 10,
    /// A slash amount exceeded the sentinel's current stake.
    SlashExceedsStake = 11,
    /// A slash was submitted without a proof identifier.
    InvalidProofId = 12,
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
    /// * [`Error::InvalidLockPeriod`] — the stored lock window is not
    ///   representable.
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

        // The lock window starts on the ledger the deposit lands on. Read once
        // and reused, so the balance and the lock timestamp in `StakedEvent`
        // can never disagree about which ledger this was.
        let ledger_seq = u64::from(env.ledger().sequence());

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

        // Checked subtraction. `amount <= current` above already rules out an
        // underflow, so this `ok_or` arm is a belt-and-braces guard that keeps
        // the invariant local to this line: `remaining` is derived by checked
        // arithmetic, never by subtraction that can wrap.
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
        proof_id: String,
        reason: Symbol,
    ) -> Result<(), Error> {
        if proof_id.is_empty() {
            return Err(Error::InvalidProofId);
        }
        Self::require_initialized(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidSlashAmount);
        }

        admin.require_auth();
        Self::require_admin(&env, &admin)?;

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
        testutils::{Address as _, Events as _, Ledger},
        Env, Event,
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

    // ── CT-31: initialization safeguards ────────────────────────────────────

    #[test]
    fn a_rejected_initialization_leaves_the_pool_uninitialized() {
    // ── CT-35: stake event details ──────────────────────────────────────────

    #[test]
    fn a_stake_event_carries_the_resulting_balance_and_its_ledger() {
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

        env.mock_all_auths();

        client.initialize(&admin, &100, &10, &2);
        assert!(client.try_set_threshold(&admin, &0).is_err());

        assert_eq!(client.threshold(), 2);
    }

    #[test]
    fn threshold_update_on_an_uninitialized_pool_is_refused() {
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
        client.stake(&sentinel, &i128::MAX);

        assert_eq!(client.balance(&sentinel), i128::MAX);
        // Replaying the same maximal deposit is the overflow case.
        assert!(client.try_stake(&sentinel, &i128::MAX).is_err());
        assert_eq!(client.balance(&sentinel), i128::MAX);
    }

    #[test]
    fn a_zero_or_negative_deposit_never_creates_a_stake() {
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
        let sentinel = Address::generate(&env);

        env.mock_all_auths();

        assert!(client.try_stake(&sentinel, &250).is_err());

        client.initialize(&admin, &100, &10, &2);

        client.stake(&sentinel, &250);
        assert_eq!(client.balance(&sentinel), 250);

        env.ledger().with_mut(|l| l.sequence_number = 20);
        client.withdraw(&sentinel, &100);
        assert_eq!(client.balance(&sentinel), 150);
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

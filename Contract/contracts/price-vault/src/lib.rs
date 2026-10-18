#![no_std]

//! `PriceVault` — stores raw price submissions keyed by `(country_iso,
//! category, valid_from)`.
//!
//! A submission is a single cost-of-living observation (bread, rent, transport,
//! …) made in a given country at a given moment. Each observation carries an
//! explicit **validity window** and moves through a strict **verification
//! state machine** before it can be aggregated into the Kovara index.
//!
//! ```text
//!                         ┌──────────────► Verified   (terminal)
//!  submit() ──► Pending ──┤
//!                         └──────────────► Rejected   (terminal)
//! ```
//!
//! # Design invariants
//!
//! ## Validation happens before auth, and before storage
//!
//! Every rejection path in [`PriceVault::submit`] is a **pure** predicate over
//! the caller's arguments. The order is fixed:
//!
//! 1. [`registry`] — the country and category must be known identifiers.
//! 2. [`payload`] — the arguments must satisfy [`SUBMIT_SCHEMA`].
//! 3. [`guards`] — arithmetic is checked, never assumed.
//!
//! Only then does the contract call `require_auth` and touch storage. A
//! malformed call therefore costs the caller nothing beyond the validation it
//! already had to pay for, and can never leave a partial write behind.
//!
//! ## Validity windows
//!
//! ```text
//! valid_from ──────────── valid_until
//!     │                      │
//!  earliest ledger ts     expiry ts (exclusive)
//! ```
//!
//! The lower bound is inclusive, the upper bound exclusive, so consecutive
//! non-overlapping windows `[t, t+w)` and `[t+w, t+2w)` never both claim a
//! query timestamp. [`PriceVault::is_valid_at`] is the canonical predicate;
//! [`PriceVault::get_valid`] combines it with the read.
//!
//! ## Terminal states are terminal
//!
//! `Verified` and `Rejected` are absorbing. There is no path back to `Pending`
//! and no path between them — see [`SubmissionStatus::can_transition_to`]. An
//! attempt to leave a terminal state fails with [`Error::InvalidTransition`]
//! and leaves the record byte-for-byte unchanged.
//!
//! ## Error codes are ABI
//!
//! [`Error`] is `#[repr(u32)]` and its discriminants are part of the contract
//! ABI: deployed clients map integer codes back to meanings. They are therefore
//! allocated **contiguously from 1** and are **append-only**. A new condition
//! gets the next free number; an existing number is never reused or renumbered,
//! even if the condition it described is later removed.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Symbol, Vec,
};

// ── Constants ────────────────────────────────────────────────────────────────

/// Default validity window: 24 hours expressed in seconds.
///
/// A price observation submitted without an explicit window should use this.
/// 24 h suits daily cost-of-living basket items (bread, rent, transport) where
/// intra-day volatility is minimal.
pub const DEFAULT_VALIDITY_WINDOW_SECS: u64 = 86_400;

/// The schema version this build of `PriceVault` understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Largest price value accepted by [`SUBMIT_SCHEMA`] (fixed-point units).
///
/// The ceiling bounds how far one rogue submission can skew an index median.
pub const MAX_PRICE: i128 = 1_000_000_000_000;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Errors returned by `PriceVault`.
///
/// Discriminants are contiguous from 1 and append-only — see the ABI note in
/// the module docs.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// `valid_from` is zero.
    InvalidTimestamp = 1,
    /// `validity_window_secs` is zero, or `valid_from + window` overflows.
    InvalidWindow = 2,
    /// `from_ts > to_ts` in a range query.
    InvalidRange = 3,
    /// The country is not a supported ISO 3166-1 alpha-2 identifier.
    InvalidCountry = 4,
    /// The category is not a supported basket category identifier.
    InvalidCategory = 5,
    /// The requested status transition is not permitted.
    InvalidTransition = 6,
    /// No submission exists for the given key.
    NotFound = 7,

    // ── payload schema ───────────────────────────────────────────────────
    /// A required field is missing.
    MissingRequiredField = 8,
    /// A field value is below its declared minimum.
    ValueBelowMinimum = 9,
    /// A field value is above its declared maximum.
    ValueAboveMaximum = 10,
    /// A `Symbol` field is empty.
    EmptySymbol = 11,
    /// A numeric field is negative where non-negative is required.
    NegativeNotAllowed = 12,
    /// A numeric field is zero where strictly positive is required.
    ZeroNotAllowed = 13,
    /// The payload's schema version is not the one this build understands.
    SchemaMismatch = 14,

    // ── guards ──────────────────────────────────────────────────────────
    /// A price value was zero.
    ZeroPrice = 15,
    /// A price value was negative.
    NegativePrice = 16,
    /// A `u64` that must be positive was zero.
    ZeroValue = 17,
    /// An `i128` amount that must be positive was zero or negative.
    NonPositiveAmount = 18,
    /// An addition would overflow the target type.
    ArithmeticOverflow = 19,
    /// A multiplication would overflow the target type.
    ArithmeticOverflowMul = 20,
}

// ── Identifier registry ──────────────────────────────────────────────────────

/// The set of country and basket-category identifiers the protocol understands.
///
/// The registry is deliberately **not** encoded in any storage key. Every stored
/// record references an identifier by `Symbol`, and the identifier is checked
/// against this registry at the contract boundary, so adding a country or a
/// category later is a code change that leaves previously stored records — and
/// their keys — untouched. Extending it therefore needs no storage migration
/// and cannot invalidate existing submissions.
pub mod registry {
    use soroban_sdk::{Env, Symbol, Vec};

    /// Supported ISO 3166-1 alpha-2 country codes.
    ///
    /// A real subset of the standard: every entry is a designated ISO 3166-1
    /// alpha-2 identifier.
    pub const COUNTRY_CODES: &[&str] = &[
        "US", "GB", "NG", "KE", "IN", "BR", "DE", "FR", "JP", "CN", "ZA", "GH", "EG", "TZ", "UG",
        "ET", "PH", "ID", "MX", "AR",
    ];

    /// Supported cost-of-living basket categories.
    pub const CATEGORIES: &[&str] = &["Food", "Rent", "Transport", "Utilities", "Health"];

    /// Version of the identifier registry.
    ///
    /// Bump this when an identifier is added or removed so clients and indexers
    /// can tell which registry a deployment was built against.
    pub const VERSION: u32 = 1;

    /// Every supported country code, as `Symbol`s.
    pub fn countries(env: &Env) -> Vec<Symbol> {
        let mut codes = Vec::new(env);
        for code in COUNTRY_CODES.iter() {
            codes.push_back(Symbol::new(env, code));
        }
        codes
    }

    /// Every supported basket category, as `Symbol`s.
    pub fn categories(env: &Env) -> Vec<Symbol> {
        let mut keys = Vec::new(env);
        for key in CATEGORIES.iter() {
            keys.push_back(Symbol::new(env, key));
        }
        keys
    }

    /// Whether `country_iso` is a supported ISO 3166-1 alpha-2 identifier.
    pub fn is_supported_country(env: &Env, country_iso: &Symbol) -> bool {
        COUNTRY_CODES
            .iter()
            .any(|code| &Symbol::new(env, code) == country_iso)
    }

    /// Whether `category` is a supported basket category identifier.
    pub fn is_supported_category(env: &Env, category: &Symbol) -> bool {
        CATEGORIES
            .iter()
            .any(|key| &Symbol::new(env, key) == category)
    }
}

// ── Fail-safe arithmetic guards ──────────────────────────────────────────────

/// Centralised arithmetic safety guards.
///
/// Every guard is a pure function returning `Err` rather than panicking. A
/// panic in a Soroban contract aborts the transaction without emitting an error
/// code, burning caller fees and leaving no diagnosable trace — so overflow and
/// bad input are always values, never faults.
pub mod guards {
    use super::Error;

    /// Reject a price value that is zero or negative.
    ///
    /// # Errors
    /// * [`Error::NegativePrice`] — `value < 0`
    /// * [`Error::ZeroPrice`] — `value == 0`
    #[inline]
    pub fn require_positive_price(value: i128) -> Result<(), Error> {
        if value < 0 {
            return Err(Error::NegativePrice);
        }
        if value == 0 {
            return Err(Error::ZeroPrice);
        }
        Ok(())
    }

    /// Reject a `u64` value that is zero.
    ///
    /// # Errors
    /// * [`Error::ZeroValue`] — `value == 0`
    #[inline]
    pub fn require_positive_u64(value: u64) -> Result<(), Error> {
        if value == 0 {
            return Err(Error::ZeroValue);
        }
        Ok(())
    }

    /// Reject an `i128` amount that is zero or negative.
    ///
    /// # Errors
    /// * [`Error::NonPositiveAmount`] — `amount <= 0`
    #[inline]
    pub fn require_positive_i128(amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::NonPositiveAmount);
        }
        Ok(())
    }

    /// Checked `u64` addition.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflow`] — the result would exceed `u64::MAX`.
    #[inline]
    pub fn checked_add_u64(a: u64, b: u64) -> Result<u64, Error> {
        a.checked_add(b).ok_or(Error::ArithmeticOverflow)
    }

    /// Checked `i128` addition.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflow`] — the result would overflow `i128`.
    #[inline]
    pub fn checked_add_i128(a: i128, b: i128) -> Result<i128, Error> {
        a.checked_add(b).ok_or(Error::ArithmeticOverflow)
    }

    /// Checked `i128` multiplication.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflowMul`] — the result would overflow `i128`.
    #[inline]
    pub fn checked_mul_i128(a: i128, b: i128) -> Result<i128, Error> {
        a.checked_mul(b).ok_or(Error::ArithmeticOverflowMul)
    }
}

// ── Payload schema validation ────────────────────────────────────────────────

/// Schema validation for external payloads.
///
/// Every entry point that receives external input should:
/// 1. define a `*Payload` struct for its arguments,
/// 2. declare a `*_SCHEMA` constant (`&[Constraint]`),
/// 3. call `validate(&schema, &payload)?` **before** any auth check or state
///    mutation.
pub mod payload {
    use super::Error;
    use soroban_sdk::Symbol;

    /// Discriminant for the type of a validated field.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub enum FieldKind {
        /// A 64-bit unsigned integer (timestamps, counts).
        U64,
        /// A 128-bit signed integer (prices, amounts).
        I128,
        /// A Soroban `Symbol` (country codes, category labels).
        Symbol,
    }

    /// A single named constraint applied to a payload field.
    ///
    /// Constraints are evaluated in declaration order and the first failure
    /// returns immediately — there is no error accumulation, which is how
    /// Soroban contracts communicate a single failure to a caller.
    #[derive(Copy, Clone, Debug)]
    pub enum Constraint {
        /// The `u64` field must be strictly greater than zero.
        U64NonZero,
        /// The `u64` field must be at most this value.
        U64Max(u64),
        /// The `i128` field must be strictly positive.
        I128Positive,
        /// The `i128` field must be zero or positive.
        I128NonNegative,
        /// The `i128` field must be at most this value.
        I128Max(i128),
        /// The `Symbol` fields must each have at least one character.
        SymbolNonEmpty,
        /// The payload's schema version must equal this value.
        SchemaVersion(u32),
    }

    /// A plain-data view of the `submit` arguments.
    ///
    /// Decoupling validation from `Env` and `Address` keeps [`validate`] a pure
    /// function: no SDK imports in the validation logic, no mock environment
    /// needed to test it.
    pub struct SubmitPayload<'a> {
        /// ISO country code (e.g. `"NG"`).
        pub country_iso: &'a Symbol,
        /// Basket category (e.g. `"Food"`).
        pub category: &'a Symbol,
        /// Price value in the smallest fixed-point unit.
        pub value: i128,
        /// Observation timestamp.
        pub timestamp: u64,
        /// Schema version the caller expects.
        pub schema_version: u32,
    }

    /// Apply `schema` to `payload`, returning the first failing constraint.
    ///
    /// `#[inline]` so the optimiser can fold constant schemas at the call site
    /// in release builds.
    #[inline]
    pub fn validate(schema: &[Constraint], payload: &SubmitPayload<'_>) -> Result<(), Error> {
        for constraint in schema {
            match constraint {
                Constraint::U64NonZero => {
                    if payload.timestamp == 0 {
                        return Err(Error::ZeroNotAllowed);
                    }
                }
                Constraint::U64Max(max) => {
                    if payload.timestamp > *max {
                        return Err(Error::ValueAboveMaximum);
                    }
                }
                Constraint::I128Positive => {
                    if payload.value < 0 {
                        return Err(Error::NegativeNotAllowed);
                    }
                    if payload.value == 0 {
                        return Err(Error::ZeroNotAllowed);
                    }
                }
                Constraint::I128NonNegative => {
                    if payload.value < 0 {
                        return Err(Error::NegativeNotAllowed);
                    }
                }
                Constraint::I128Max(max) => {
                    if payload.value > *max {
                        return Err(Error::ValueAboveMaximum);
                    }
                }
                Constraint::SymbolNonEmpty => {
                    // A zero-length Symbol would produce an unresolvable key.
                    if payload.country_iso.len() == 0 || payload.category.len() == 0 {
                        return Err(Error::EmptySymbol);
                    }
                }
                Constraint::SchemaVersion(expected) => {
                    if payload.schema_version != *expected {
                        return Err(Error::SchemaMismatch);
                    }
                }
            }
        }
        Ok(())
    }
}

/// The schema applied to every `PriceVault::submit` call.
///
/// Ordered cheapest-first: integer bounds run before `Symbol` length checks.
pub const SUBMIT_SCHEMA: &[payload::Constraint] = &[
    payload::Constraint::U64NonZero,
    payload::Constraint::I128Positive,
    payload::Constraint::I128Max(MAX_PRICE),
    payload::Constraint::SymbolNonEmpty,
    payload::Constraint::SchemaVersion(SCHEMA_VERSION),
];

// ── Submission status ────────────────────────────────────────────────────────

/// Lifecycle state of a single price submission.
///
/// ```text
/// Pending ──► Verified   (terminal)
///         └─► Rejected   (terminal)
/// ```
///
/// Consumers matching on status should match exhaustively on all three
/// variants so they stay correct when more are added in a future schema
/// version.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SubmissionStatus {
    /// Recorded, awaiting verifier votes.
    Pending = 0,
    /// Quorum approval; accepted as a valid price observation.
    Verified = 1,
    /// Quorum rejection, or invalidated by governance.
    Rejected = 2,
}

impl SubmissionStatus {
    /// Whether this state absorbs: no transition leaves it.
    #[inline]
    pub fn is_terminal(self) -> bool {
        matches!(self, SubmissionStatus::Verified | SubmissionStatus::Rejected)
    }

    /// # The transition table
    ///
    /// Whether `self -> next` is a permitted transition.
    ///
    /// Exactly one edge exists: `Pending -> {Verified, Rejected}`. `Pending`
    /// is never a *target* (it is only ever the initial state assigned by
    /// `submit`), and terminal states have no outgoing edges. Encoding this as
    /// a function rather than as inline `if`s means the table lives in one
    /// place, so `set_status` and any future consumer cannot drift apart.
    #[inline]
    pub fn can_transition_to(self, next: SubmissionStatus) -> bool {
        matches!(
            (self, next),
            (SubmissionStatus::Pending, SubmissionStatus::Verified)
                | (SubmissionStatus::Pending, SubmissionStatus::Rejected)
        )
    }
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// `(country_iso, category, valid_from)` → [`PriceSubmission`].
    ///
    /// `valid_from` is the observation timestamp; it is both the storage key
    /// component and the start of the validity window.
    Price(Symbol, Symbol, u64),

    /// `(country_iso, category)` → `Vec<u64>` of observation timestamps, in
    /// insertion order.
    ///
    /// Lets history queries enumerate records without a full-storage scan.
    SubmissionIndex(Symbol, Symbol),
}

// ── Domain types ─────────────────────────────────────────────────────────────

/// A single raw price submission with an attached validity window.
///
/// The window `[valid_from, valid_until)` bounds the interval during which the
/// record is a fresh observation. After `valid_until` the record stays in
/// storage for historical audit but is excluded from live aggregation.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceSubmission {
    /// The submitting oracle/operator address.
    pub submitter: Address,
    /// ISO country code, kept readable for indexing.
    pub country_iso: Symbol,
    /// The price category, e.g. `Food` or `Rent`.
    pub category: Symbol,
    /// Raw price in the smallest fixed-point unit. Must be `> 0`.
    pub value: i128,
    /// Start of the validity window; also the storage key component.
    pub valid_from: u64,
    /// Exclusive expiry timestamp (`valid_from + validity_window_secs`).
    pub valid_until: u64,
    /// Observation timestamp. Mirrors `valid_from`.
    pub timestamp: u64,
    /// Current lifecycle state. `Pending` at creation.
    pub status: SubmissionStatus,
}

impl PriceSubmission {
    /// Whether `query_ts` falls within `[valid_from, valid_until)`.
    ///
    /// Lower bound inclusive, upper bound exclusive, so consecutive
    /// non-overlapping windows never both claim a timestamp.
    #[inline]
    pub fn is_valid_at(&self, query_ts: u64) -> bool {
        query_ts >= self.valid_from && query_ts < self.valid_until
    }
}

// ── Events ───────────────────────────────────────────────────────────────────

/// Emitted after a price submission is recorded.
///
/// `country_iso` and `category` are **topics** — the two dimensions an indexer
/// filters on — so a consumer can subscribe to just the submissions it cares
/// about without decoding every body. The body carries the rest of the
/// metadata so an indexer can build its record from the event alone.
///
/// Emitted only after both the record and its index entry are written, so a
/// consumer reacting to the event can read the submission back immediately. A
/// replayed submission is a no-op and emits nothing.
#[contractevent]
#[derive(Clone)]
pub struct PriceSubmitted {
    /// Topic — the country this observation belongs to.
    #[topic]
    pub country_iso: Symbol,
    /// Topic — the basket category.
    #[topic]
    pub category: Symbol,
    /// The submitting oracle/operator.
    pub submitter: Address,
    /// Raw price value.
    pub value: i128,
    /// Start of the validity window.
    pub valid_from: u64,
    /// Exclusive expiry timestamp.
    pub valid_until: u64,
    /// Status at creation, always `Pending`.
    pub status: SubmissionStatus,
    /// Schema version the record was written under.
    pub schema_version: u32,
}

// ── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct PriceVault;

#[contractimpl]
impl PriceVault {
    // ── Mutating entry points ───────────────────────────────────────────

    /// # Record a raw price submission
    ///
    /// `valid_from` is the observation timestamp (and the storage key
    /// component). `validity_window_secs` is how long the submission stays
    /// fresh; [`DEFAULT_VALIDITY_WINDOW_SECS`] (86 400 s) suits daily basket
    /// observations.
    ///
    /// Validation order is fixed and every step before auth is a pure
    /// predicate — see the module docs. If a record already exists for the same
    /// `(country_iso, category, valid_from)` triple the call is a **no-op**:
    /// the original is kept and the index is not duplicated, so replaying a
    /// transaction can never corrupt the historical index.
    ///
    /// # Errors
    /// * [`Error::InvalidCountry`] — country is not a registry identifier.
    /// * [`Error::InvalidCategory`] — category is not a registry identifier.
    /// * [`Error::ZeroNotAllowed`] — `valid_from` or `value` is zero.
    /// * [`Error::NegativeNotAllowed`] — `value` is negative.
    /// * [`Error::ValueAboveMaximum`] — `value` exceeds [`MAX_PRICE`].
    /// * [`Error::EmptySymbol`] — country or category is empty.
    /// * [`Error::SchemaMismatch`] — caller expects another schema version.
    /// * [`Error::InvalidWindow`] — zero window, or `valid_from + window`
    ///   overflows `u64`.
    pub fn submit(
        env: Env,
        submitter: Address,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        valid_from: u64,
        validity_window_secs: u64,
    ) -> Result<(), Error> {
        // 1. Identifier registry — pure, before auth and before storage.
        if !registry::is_supported_country(&env, &country_iso) {
            return Err(Error::InvalidCountry);
        }
        if !registry::is_supported_category(&env, &category) {
            return Err(Error::InvalidCategory);
        }

        // 2. Payload schema — pure, before auth and before storage.
        payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country_iso,
                category: &category,
                value,
                timestamp: valid_from,
                schema_version: SCHEMA_VERSION,
            },
        )?;

        // 3. Guards — defence in depth for the price itself.
        guards::require_positive_price(value)?;

        // Validity window. Zero or overflowing windows are rejected rather
        // than clamped: a clamped window would silently disagree with the
        // submitter's intent about how fresh the observation is.
        if validity_window_secs == 0 {
            return Err(Error::InvalidWindow);
        }
        let valid_until = valid_from
            .checked_add(validity_window_secs)
            .ok_or(Error::InvalidWindow)?;

        // Auth comes after every pure check.
        submitter.require_auth();

        let key = DataKey::Price(country_iso.clone(), category.clone(), valid_from);

        // Idempotent replay: keep the first write, emit nothing.
        if env.storage().persistent().has(&key) {
            return Ok(());
        }

        env.storage().persistent().set(
            &key,
            &PriceSubmission {
                submitter: submitter.clone(),
                country_iso: country_iso.clone(),
                category: category.clone(),
                value,
                valid_from,
                valid_until,
                timestamp: valid_from,
                status: SubmissionStatus::Pending,
            },
        );

        // Append to the per-(country, category) index so history queries need
        // no full-storage scan.
        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let mut index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));
        index.push_back(valid_from);
        env.storage().persistent().set(&idx_key, &index);

        PriceSubmitted {
            country_iso,
            category,
            submitter,
            value,
            valid_from,
            valid_until,
            status: SubmissionStatus::Pending,
            schema_version: SCHEMA_VERSION,
        }
        .publish(&env);

        Ok(())
    }

    /// # Transition a submission's status
    ///
    /// Moves a `Pending` submission to `Verified` or `Rejected`. The transition
    /// table is [`SubmissionStatus::can_transition_to`]: `Pending` is never a
    /// target, and `Verified`/`Rejected` are absorbing.
    ///
    /// The record is left byte-for-byte unchanged when a transition is refused.
    ///
    /// # Errors
    /// * [`Error::NotFound`] — no submission for the given key.
    /// * [`Error::InvalidTransition`] — `new_status` is `Pending`, or the
    ///   current status is already terminal.
    pub fn set_status(
        env: Env,
        verifier: Address,
        country_iso: Symbol,
        category: Symbol,
        timestamp: u64,
        new_status: SubmissionStatus,
    ) -> Result<(), Error> {
        verifier.require_auth();

        let key = DataKey::Price(country_iso.clone(), category.clone(), timestamp);
        let mut submission: PriceSubmission = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotFound)?;

        // One table, consulted here and nowhere else.
        if !submission.status.can_transition_to(new_status) {
            return Err(Error::InvalidTransition);
        }

        submission.status = new_status;
        env.storage().persistent().set(&key, &submission);

        Ok(())
    }

    // ── Read entry points ───────────────────────────────────────────────

    /// Read a stored submission by its exact composite key.
    ///
    /// Returns the record regardless of whether it is currently fresh — use
    /// [`Self::get_valid`] to gate on the validity window.
    pub fn get(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        valid_from: u64,
    ) -> Option<PriceSubmission> {
        env.storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, valid_from))
    }

    /// Read a submission only if it is fresh at `query_ts`.
    ///
    /// Returns `None` when the record is missing, expired
    /// (`query_ts >= valid_until`), or not yet valid (`query_ts < valid_from`).
    /// This is the recommended call for aggregation and index code, which must
    /// exclude stale prices.
    pub fn get_valid(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        valid_from: u64,
        query_ts: u64,
    ) -> Option<PriceSubmission> {
        let submission: PriceSubmission = env
            .storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, valid_from))?;
        if submission.is_valid_at(query_ts) {
            Some(submission)
        } else {
            None
        }
    }

    /// Whether a stored submission is fresh at `query_ts`.
    ///
    /// Returns `false` (not an error) for missing records so callers can use a
    /// plain boolean gate without matching on `Option`.
    pub fn is_valid_at(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        valid_from: u64,
        query_ts: u64,
    ) -> bool {
        let submission: Option<PriceSubmission> = env
            .storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, valid_from));
        match submission {
            Some(s) => s.is_valid_at(query_ts),
            None => false,
        }
    }

    /// Read just the lifecycle state of a submission, if present.
    ///
    /// Cheaper than [`Self::get`] when the caller only needs the status.
    pub fn get_status(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        timestamp: u64,
    ) -> Option<SubmissionStatus> {
        let submission: Option<PriceSubmission> = env
            .storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, timestamp));
        submission.map(|s| s.status)
    }

    /// All submissions for `(country_iso, category)`, in insertion order.
    ///
    /// Deterministic: identical contract state always produces an identical
    /// sequence. Returns an empty `Vec` when there are none — never panics.
    /// For bounded reads prefer [`Self::get_history_range`].
    pub fn get_history(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
    ) -> Vec<PriceSubmission> {
        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));

        Self::collect_submissions(&env, &country_iso, &category, &timestamps)
    }

    /// Submissions whose timestamp satisfies `from_ts <= ts <= to_ts`, in
    /// insertion order. Both bounds are inclusive.
    ///
    /// # Errors
    /// * [`Error::InvalidRange`] — `from_ts > to_ts`.
    pub fn get_history_range(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        from_ts: u64,
        to_ts: u64,
    ) -> Result<Vec<PriceSubmission>, Error> {
        if from_ts > to_ts {
            return Err(Error::InvalidRange);
        }

        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let all_timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));

        let mut filtered = Vec::new(&env);
        for ts in all_timestamps.iter() {
            if ts >= from_ts && ts <= to_ts {
                filtered.push_back(ts);
            }
        }

        Ok(Self::collect_submissions(&env, &country_iso, &category, &filtered))
    }

    /// The most recently **inserted** submission for `(country_iso, category)`,
    /// or `None` when there is none.
    pub fn get_latest(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
    ) -> Option<PriceSubmission> {
        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));

        // The index is append-only and in insertion order, so the last entry is
        // the most recently inserted timestamp.
        let latest_ts = timestamps.last()?;
        env.storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, latest_ts))
    }

    // ── Registry views ──────────────────────────────────────────────────

    /// The supported ISO 3166-1 alpha-2 country codes.
    pub fn supported_countries(env: Env) -> Vec<Symbol> {
        registry::countries(&env)
    }

    /// The supported cost-of-living basket categories.
    pub fn supported_categories(env: Env) -> Vec<Symbol> {
        registry::categories(&env)
    }

    /// The registry version this build was compiled against.
    pub fn registry_version(_env: Env) -> u32 {
        registry::VERSION
    }

    // ── Pure helpers, callable off-chain ────────────────────────────────

    /// Add two `i128` values, failing safely on overflow.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflow`] — result would overflow `i128`.
    pub fn safe_add(_env: Env, a: i128, b: i128) -> Result<i128, Error> {
        guards::checked_add_i128(a, b)
    }

    /// Multiply two `i128` values, failing safely on overflow.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflowMul`] — result would overflow `i128`.
    pub fn safe_mul(_env: Env, a: i128, b: i128) -> Result<i128, Error> {
        guards::checked_mul_i128(a, b)
    }

    /// Validate a submit payload without committing any state.
    ///
    /// Lets SDK clients and cross-contract callers confirm a payload is
    /// structurally sound before building or forwarding a transaction.
    pub fn validate_submit_payload(
        _env: Env,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        timestamp: u64,
    ) -> Result<(), Error> {
        payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country_iso,
                category: &category,
                value,
                timestamp,
                schema_version: SCHEMA_VERSION,
            },
        )
    }

    // ── Internal ────────────────────────────────────────────────────────

    /// Load records for each timestamp in `timestamps`, skipping any that are
    /// absent (defensive — under normal operation every indexed timestamp has
    /// a record).
    fn collect_submissions(
        env: &Env,
        country_iso: &Symbol,
        category: &Symbol,
        timestamps: &Vec<u64>,
    ) -> Vec<PriceSubmission> {
        let mut result = Vec::new(env);
        for ts in timestamps.iter() {
            let key = DataKey::Price(country_iso.clone(), category.clone(), ts);
            if let Some(submission) = env.storage().persistent().get::<DataKey, PriceSubmission>(&key)
            {
                result.push_back(submission);
            }
        }
        result
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    fn make_env() -> Env {
        Env::default()
    }

    fn deploy(env: &Env) -> PriceVaultClient {
        let contract_id = env.register(PriceVault, ());
        PriceVaultClient::new(env, &contract_id)
    }

    fn sym(env: &Env, s: &str) -> Symbol {
        Symbol::new(env, s)
    }

    /// Submit with the default validity window.
    fn submit_ok(
        client: &PriceVaultClient,
        submitter: &Address,
        country: &Symbol,
        category: &Symbol,
        value: i128,
        valid_from: u64,
    ) {
        client
            .submit(
                submitter,
                country,
                category,
                &value,
                &valid_from,
                &DEFAULT_VALIDITY_WINDOW_SECS,
            )
            .unwrap();
    }

    // ── guards ──────────────────────────────────────────────────────────

    #[test]
    fn guard_price_bounds() {
        assert_eq!(guards::require_positive_price(0), Err(Error::ZeroPrice));
        assert_eq!(
            guards::require_positive_price(-1),
            Err(Error::NegativePrice)
        );
        assert_eq!(
            guards::require_positive_price(i128::MIN),
            Err(Error::NegativePrice)
        );
        assert!(guards::require_positive_price(1).is_ok());
        assert!(guards::require_positive_price(i128::MAX).is_ok());
    }

    #[test]
    fn guard_u64_and_i128_bounds() {
        assert_eq!(guards::require_positive_u64(0), Err(Error::ZeroValue));
        assert!(guards::require_positive_u64(1).is_ok());

        assert_eq!(
            guards::require_positive_i128(0),
            Err(Error::NonPositiveAmount)
        );
        assert_eq!(
            guards::require_positive_i128(-1),
            Err(Error::NonPositiveAmount)
        );
        assert!(guards::require_positive_i128(1).is_ok());
    }

    #[test]
    fn guard_checked_arithmetic() {
        assert_eq!(guards::checked_add_u64(100, 200), Ok(300));
        assert_eq!(guards::checked_add_u64(u64::MAX, 0), Ok(u64::MAX));
        assert_eq!(
            guards::checked_add_u64(u64::MAX, 1),
            Err(Error::ArithmeticOverflow)
        );

        assert_eq!(guards::checked_add_i128(50, 50), Ok(100));
        assert_eq!(
            guards::checked_add_i128(i128::MAX, 1),
            Err(Error::ArithmeticOverflow)
        );
        assert_eq!(
            guards::checked_add_i128(i128::MIN, -1),
            Err(Error::ArithmeticOverflow)
        );

        assert_eq!(guards::checked_mul_i128(3, 4), Ok(12));
        assert_eq!(
            guards::checked_mul_i128(i128::MAX, 2),
            Err(Error::ArithmeticOverflowMul)
        );
        assert_eq!(
            guards::checked_mul_i128(i128::MIN, -1),
            Err(Error::ArithmeticOverflowMul)
        );
    }

    // ── payload validate ────────────────────────────────────────────────

    fn payload<'a>(
        country: &'a Symbol,
        category: &'a Symbol,
        value: i128,
        timestamp: u64,
    ) -> payload::SubmitPayload<'a> {
        payload::SubmitPayload {
            country_iso: country,
            category,
            value,
            timestamp,
            schema_version: SCHEMA_VERSION,
        }
    }

    #[test]
    fn validate_rejects_bad_values() {
        let env = make_env();
        let ng = sym(&env, "NG");
        let food = sym(&env, "Food");
        let empty = sym(&env, "");

        assert_eq!(
            payload::validate(SUBMIT_SCHEMA, &payload(&ng, &food, 0, 1)),
            Err(Error::ZeroNotAllowed)
        );
        assert_eq!(
            payload::validate(SUBMIT_SCHEMA, &payload(&ng, &food, -1, 1)),
            Err(Error::NegativeNotAllowed)
        );
        assert_eq!(
            payload::validate(SUBMIT_SCHEMA, &payload(&ng, &food, 100, 0)),
            Err(Error::ZeroNotAllowed)
        );
        assert_eq!(
            payload::validate(SUBMIT_SCHEMA, &payload(&ng, &food, MAX_PRICE + 1, 1)),
            Err(Error::ValueAboveMaximum)
        );
        assert_eq!(
            payload::validate(SUBMIT_SCHEMA, &payload(&empty, &food, 100, 1)),
            Err(Error::EmptySymbol)
        );
        assert!(payload::validate(SUBMIT_SCHEMA, &payload(&ng, &food, 100, 1)).is_ok());
        assert!(payload::validate(SUBMIT_SCHEMA, &payload(&ng, &food, MAX_PRICE, 1)).is_ok());
    }

    // ── registry ────────────────────────────────────────────────────────

    #[test]
    fn registry_recognises_known_identifiers() {
        let env = make_env();
        assert!(registry::is_supported_country(&env, &sym(&env, "NG")));
        assert!(registry::is_supported_country(&env, &sym(&env, "US")));
        assert!(!registry::is_supported_country(&env, &sym(&env, "ZZ")));
        assert!(registry::is_supported_category(&env, &sym(&env, "Food")));
        assert!(!registry::is_supported_category(&env, &sym(&env, "Weapons")));
        assert_eq!(registry::countries(&env).len(), registry::COUNTRY_CODES.len());
        assert_eq!(registry::categories(&env).len(), registry::CATEGORIES.len());
    }

    // ── submit ──────────────────────────────────────────────────────────

    #[test]
    fn submit_stores_window_bounds_and_pending_status() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        submit_ok(&client, &submitter, &country, &category, 1_000, 1_000);

        let s = client.get(&country, &category, &1_000).unwrap();
        assert_eq!(s.valid_from, 1_000);
        assert_eq!(s.valid_until, 1_000 + DEFAULT_VALIDITY_WINDOW_SECS);
        assert_eq!(s.timestamp, 1_000);
        assert_eq!(s.value, 1_000);
        assert_eq!(s.submitter, submitter);
        assert_eq!(s.status, SubmissionStatus::Pending);
    }

    #[test]
    fn submit_rejects_unknown_identifiers() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        env.mock_all_auths();

        assert_eq!(
            client
                .try_submit(
                    &submitter,
                    &sym(&env, "ZZ"),
                    &sym(&env, "Food"),
                    &100,
                    &1_000_u64,
                    &DEFAULT_VALIDITY_WINDOW_SECS,
                )
                .unwrap_err()
                .unwrap(),
            Error::InvalidCountry
        );
        assert_eq!(
            client
                .try_submit(
                    &submitter,
                    &sym(&env, "NG"),
                    &sym(&env, "Weapons"),
                    &100,
                    &1_000_u64,
                    &DEFAULT_VALIDITY_WINDOW_SECS,
                )
                .unwrap_err()
                .unwrap(),
            Error::InvalidCategory
        );
    }

    #[test]
    fn submit_rejects_bad_values_before_auth() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        // Zero price.
        assert_eq!(
            client
                .try_submit(
                    &submitter,
                    &country,
                    &category,
                    &0,
                    &1_000_u64,
                    &DEFAULT_VALIDITY_WINDOW_SECS,
                )
                .unwrap_err()
                .unwrap(),
            Error::ZeroNotAllowed
        );
        // Negative price.
        assert_eq!(
            client
                .try_submit(
                    &submitter,
                    &country,
                    &category,
                    &-1,
                    &1_000_u64,
                    &DEFAULT_VALIDITY_WINDOW_SECS,
                )
                .unwrap_err()
                .unwrap(),
            Error::NegativeNotAllowed
        );
        // Zero timestamp.
        assert_eq!(
            client
                .try_submit(
                    &submitter,
                    &country,
                    &category,
                    &100,
                    &0_u64,
                    &DEFAULT_VALIDITY_WINDOW_SECS,
                )
                .unwrap_err()
                .unwrap(),
            Error::ZeroNotAllowed
        );
        // Above the schema ceiling.
        assert_eq!(
            client
                .try_submit(
                    &submitter,
                    &country,
                    &category,
                    &(MAX_PRICE + 1),
                    &1_000_u64,
                    &DEFAULT_VALIDITY_WINDOW_SECS,
                )
                .unwrap_err()
                .unwrap(),
            Error::ValueAboveMaximum
        );
    }

    #[test]
    fn submit_rejects_bad_windows() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        // Zero window.
        assert_eq!(
            client
                .try_submit(&submitter, &country, &category, &100, &1_000_u64, &0_u64)
                .unwrap_err()
                .unwrap(),
            Error::InvalidWindow
        );
        // valid_from + window overflows u64.
        assert_eq!(
            client
                .try_submit(&submitter, &country, &category, &100, &u64::MAX, &1_u64)
                .unwrap_err()
                .unwrap(),
            Error::InvalidWindow
        );
    }

    #[test]
    fn resubmitting_the_same_key_is_a_no_op() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 100, 10);
        submit_ok(&client, &submitter, &country, &category, 999, 10);

        let history = client.get_history(&country, &category);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().value, 100);
    }

    // ── validity window ─────────────────────────────────────────────────

    #[test]
    fn validity_window_bounds_are_half_open() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 100, 1_000);

        // Lower bound inclusive.
        assert!(client.is_valid_at(&country, &category, &1_000, &1_000));
        // Interior.
        assert!(client.is_valid_at(
            &country,
            &category,
            &1_000,
            &1_000 + DEFAULT_VALIDITY_WINDOW_SECS - 1
        ));
        // Upper bound exclusive.
        assert!(!client.is_valid_at(
            &country,
            &category,
            &1_000,
            &1_000 + DEFAULT_VALIDITY_WINDOW_SECS
        ));
        // Before the window.
        assert!(!client.is_valid_at(&country, &category, &1_000, &999));
    }

    #[test]
    fn consecutive_windows_do_not_overlap() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        // Window 1 = [1_000, 2_000), window 2 = [2_000, 3_000).
        client
            .submit(&submitter, &country, &category, &100, &1_000_u64, &1_000_u64)
            .unwrap();
        client
            .submit(&submitter, &country, &category, &200, &2_000_u64, &1_000_u64)
            .unwrap();

        // At 2_000 only window 2 is fresh.
        assert!(!client.is_valid_at(&country, &category, &1_000, &2_000));
        assert!(client.is_valid_at(&country, &category, &2_000, &2_000));
    }

    #[test]
    fn get_valid_gates_on_freshness() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "KE");
        let category = sym(&env, "Rent");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 500, 2_000);

        assert!(client
            .get_valid(&country, &category, &2_000, &2_000)
            .is_some());
        assert!(client
            .get_valid(&country, &category, &2_000, &1_999)
            .is_none());
        assert!(client
            .get_valid(&country, &category, &2_000, &100_000)
            .is_none());
        // Missing record.
        assert!(client.get_valid(&country, &category, &9_999, &1).is_none());
    }

    #[test]
    fn is_valid_at_is_false_for_missing_records() {
        let env = make_env();
        let client = deploy(&env);
        assert!(!client.is_valid_at(&sym(&env, "NG"), &sym(&env, "Food"), &1, &1));
    }

    // ── status machine ──────────────────────────────────────────────────

    #[test]
    fn transition_table_allows_only_pending_to_a_terminal_state() {
        // Pending may reach either terminal state.
        assert!(SubmissionStatus::Pending.can_transition_to(SubmissionStatus::Verified));
        assert!(SubmissionStatus::Pending.can_transition_to(SubmissionStatus::Rejected));
        // Pending is never a target.
        assert!(!SubmissionStatus::Pending.can_transition_to(SubmissionStatus::Pending));
        // Terminal states are absorbing.
        assert!(!SubmissionStatus::Verified.can_transition_to(SubmissionStatus::Rejected));
        assert!(!SubmissionStatus::Verified.can_transition_to(SubmissionStatus::Pending));
        assert!(!SubmissionStatus::Rejected.can_transition_to(SubmissionStatus::Verified));
        assert!(!SubmissionStatus::Rejected.can_transition_to(SubmissionStatus::Pending));

        assert!(!SubmissionStatus::Pending.is_terminal());
        assert!(SubmissionStatus::Verified.is_terminal());
        assert!(SubmissionStatus::Rejected.is_terminal());
    }

    #[test]
    fn status_discriminants_are_stable() {
        // Pinned so external consumers can match on the u32 representation.
        assert_eq!(SubmissionStatus::Pending as u32, 0);
        assert_eq!(SubmissionStatus::Verified as u32, 1);
        assert_eq!(SubmissionStatus::Rejected as u32, 2);
    }

    #[test]
    fn pending_transitions_to_verified_and_rejected() {
        for target in [SubmissionStatus::Verified, SubmissionStatus::Rejected] {
            let env = make_env();
            let client = deploy(&env);
            let submitter = Address::generate(&env);
            let verifier = Address::generate(&env);
            let country = sym(&env, "NG");
            let category = sym(&env, "Food");
            env.mock_all_auths();

            submit_ok(&client, &submitter, &country, &category, 1_000, 100);
            client
                .set_status(&verifier, &country, &category, &100, &target)
                .unwrap();
            assert_eq!(client.get_status(&country, &category, &100), Some(target));
        }
    }

    #[test]
    fn terminal_states_reject_further_transitions() {
        for terminal in [SubmissionStatus::Verified, SubmissionStatus::Rejected] {
            for next in [
                SubmissionStatus::Verified,
                SubmissionStatus::Rejected,
                SubmissionStatus::Pending,
            ] {
                let env = make_env();
                let client = deploy(&env);
                let submitter = Address::generate(&env);
                let verifier = Address::generate(&env);
                let country = sym(&env, "NG");
                let category = sym(&env, "Food");
                env.mock_all_auths();

                submit_ok(&client, &submitter, &country, &category, 1_000, 100);
                client
                    .set_status(&verifier, &country, &category, &100, &terminal)
                    .unwrap();

                assert_eq!(
                    client
                        .try_set_status(&verifier, &country, &category, &100, &next)
                        .unwrap_err()
                        .unwrap(),
                    Error::InvalidTransition,
                    "{terminal:?} -> {next:?} must be refused"
                );
                // The record is unchanged.
                assert_eq!(
                    client.get_status(&country, &category, &100),
                    Some(terminal)
                );
            }
        }
    }

    #[test]
    fn pending_is_not_a_valid_target() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 1_000, 100);
        assert_eq!(
            client
                .try_set_status(
                    &verifier,
                    &country,
                    &category,
                    &100,
                    &SubmissionStatus::Pending,
                )
                .unwrap_err()
                .unwrap(),
            Error::InvalidTransition
        );
    }

    #[test]
    fn set_status_on_missing_record_is_not_found() {
        let env = make_env();
        let client = deploy(&env);
        let verifier = Address::generate(&env);
        env.mock_all_auths();

        assert_eq!(
            client
                .try_set_status(
                    &verifier,
                    &sym(&env, "NG"),
                    &sym(&env, "Food"),
                    &999,
                    &SubmissionStatus::Verified,
                )
                .unwrap_err()
                .unwrap(),
            Error::NotFound
        );
    }

    #[test]
    fn status_round_trips_inside_the_record() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);
        let country = sym(&env, "IN");
        let category = sym(&env, "Health");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 300, 75);
        assert_eq!(
            client.get(&country, &category, &75).unwrap().status,
            SubmissionStatus::Pending
        );

        client
            .set_status(
                &verifier,
                &country,
                &category,
                &75,
                &SubmissionStatus::Rejected,
            )
            .unwrap();

        let after = client.get(&country, &category, &75).unwrap();
        assert_eq!(after.status, SubmissionStatus::Rejected);
        // Other fields are untouched by a status change.
        assert_eq!(after.value, 300);
        assert_eq!(after.submitter, submitter);
    }

    // ── history ─────────────────────────────────────────────────────────

    #[test]
    fn history_is_empty_when_there_are_no_submissions() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(client.get_history(&sym(&env, "NG"), &sym(&env, "Food")).len(), 0);
        assert!(client.get_latest(&sym(&env, "NG"), &sym(&env, "Food")).is_none());
    }

    #[test]
    fn history_returns_submissions_in_insertion_order() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 100, 10);
        submit_ok(&client, &submitter, &country, &category, 200, 20);
        submit_ok(&client, &submitter, &country, &category, 300, 30);

        let history = client.get_history(&country, &category);
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().timestamp, 10);
        assert_eq!(history.get(1).unwrap().timestamp, 20);
        assert_eq!(history.get(2).unwrap().timestamp, 30);
    }

    #[test]
    fn history_is_scoped_per_country_and_category() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        env.mock_all_auths();

        submit_ok(&client, &submitter, &sym(&env, "NG"), &sym(&env, "Food"), 100, 10);
        submit_ok(&client, &submitter, &sym(&env, "KE"), &sym(&env, "Food"), 200, 20);
        submit_ok(&client, &submitter, &sym(&env, "NG"), &sym(&env, "Rent"), 300, 30);

        let ng_food = client.get_history(&sym(&env, "NG"), &sym(&env, "Food"));
        assert_eq!(ng_food.len(), 1);
        assert_eq!(ng_food.get(0).unwrap().value, 100);

        let ke_food = client.get_history(&sym(&env, "KE"), &sym(&env, "Food"));
        assert_eq!(ke_food.len(), 1);
        assert_eq!(ke_food.get(0).unwrap().value, 200);

        let ng_rent = client.get_history(&sym(&env, "NG"), &sym(&env, "Rent"));
        assert_eq!(ng_rent.len(), 1);
        assert_eq!(ng_rent.get(0).unwrap().value, 300);
    }

    #[test]
    fn history_carries_full_audit_metadata() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 450, 50);

        let record = client.get_history(&country, &category).get(0).unwrap();
        assert_eq!(record.submitter, submitter);
        assert_eq!(record.country_iso, country);
        assert_eq!(record.category, category);
        assert_eq!(record.value, 450);
        assert_eq!(record.timestamp, 50);
        assert_eq!(record.valid_from, 50);
        assert_eq!(record.valid_until, 50 + DEFAULT_VALIDITY_WINDOW_SECS);
        assert_eq!(record.status, SubmissionStatus::Pending);
    }

    #[test]
    fn history_range_filters_inclusively() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        for ts in [10_u64, 20, 30, 40, 50] {
            submit_ok(&client, &submitter, &country, &category, ts as i128 * 10, ts);
        }

        let range = client.get_history_range(&country, &category, &20, &40).unwrap();
        assert_eq!(range.len(), 3);
        assert_eq!(range.get(0).unwrap().timestamp, 20);
        assert_eq!(range.get(1).unwrap().timestamp, 30);
        assert_eq!(range.get(2).unwrap().timestamp, 40);

        // Both bounds inclusive.
        let single = client.get_history_range(&country, &category, &10, &10).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single.get(0).unwrap().timestamp, 10);

        // A window with nothing in it.
        let empty = client.get_history_range(&country, &category, &60, &70).unwrap();
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn history_range_rejects_an_inverted_range() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(
            client
                .try_get_history_range(&sym(&env, "NG"), &sym(&env, "Food"), &100, &50)
                .unwrap_err()
                .unwrap(),
            Error::InvalidRange
        );
    }

    #[test]
    fn latest_is_the_most_recently_inserted_record() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        // Inserted out of timestamp order; the index is insertion-ordered, so
        // "latest" is the last inserted, not the highest timestamp.
        submit_ok(&client, &submitter, &country, &category, 100, 10);
        submit_ok(&client, &submitter, &country, &category, 200, 30);
        submit_ok(&client, &submitter, &country, &category, 150, 20);

        let latest = client.get_latest(&country, &category).unwrap();
        assert_eq!(latest.timestamp, 20);
        assert_eq!(latest.value, 150);
    }

    #[test]
    fn history_is_deterministic_for_the_same_state() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");
        env.mock_all_auths();

        submit_ok(&client, &submitter, &country, &category, 1, 1);
        submit_ok(&client, &submitter, &country, &category, 2, 2);

        let first = client.get_history(&country, &category);
        let second = client.get_history(&country, &category);
        assert_eq!(first.len(), second.len());
        for i in 0..first.len() {
            assert_eq!(
                first.get(i).unwrap().timestamp,
                second.get(i).unwrap().timestamp
            );
        }
    }

    // ── pure helpers ────────────────────────────────────────────────────

    #[test]
    fn safe_arithmetic_entry_points() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(client.safe_add(&10, &20), Ok(30));
        assert_eq!(client.safe_mul(&6, &7), Ok(42));
        assert_eq!(
            client.try_safe_add(&i128::MAX, &1).unwrap_err().unwrap(),
            Error::ArithmeticOverflow
        );
        assert_eq!(
            client.try_safe_mul(&i128::MAX, &2).unwrap_err().unwrap(),
            Error::ArithmeticOverflowMul
        );
    }

    #[test]
    fn validate_submit_payload_commits_no_state() {
        let env = make_env();
        let client = deploy(&env);

        assert!(client
            .validate_submit_payload(&sym(&env, "KE"), &sym(&env, "Rent"), &500, &50)
            .is_ok());
        assert_eq!(
            client
                .try_validate_submit_payload(&sym(&env, "KE"), &sym(&env, "Rent"), &-1, &50)
                .unwrap_err()
                .unwrap(),
            Error::NegativeNotAllowed
        );
        // Nothing was written.
        assert!(client.get(&sym(&env, "KE"), &sym(&env, "Rent"), &50).is_none());
    }

    // ── constants ───────────────────────────────────────────────────────

    #[test]
    fn documented_constants_hold() {
        assert_eq!(DEFAULT_VALIDITY_WINDOW_SECS, 86_400);
        assert_eq!(SCHEMA_VERSION, 1);
        assert_eq!(registry::VERSION, 1);
    }
}
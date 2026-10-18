# Soroban Contract Workspace (CT-001)

The four Kovara Soroban contracts, as a reproducible Rust Cargo workspace:

| Contract     | Crate                  | Purpose                                                                    |
| ------------ | ---------------------- | -------------------------------------------------------------------------- |
| PriceVault   | `kovara-price-vault`   | Stores raw price submissions keyed by `(country_iso, category, timestamp)` |
| SentinelPool | `kovara-sentinel-pool` | Manages verifier staking, quorum logic, and slashing                       |
| FlowRewards  | `kovara-flow-rewards`  | Releases XLM / Stellar USDC to verified submitters and verifiers           |
| KovaraIndex  | `kovara-index`         | Aggregates verified prices into the daily `KVI` per country                |

## PriceVault storage layout

`PriceVault` uses typed Soroban keys rather than concatenated strings:

| Key                                            | Stored value                                                   | Lookup                                                 |
| ---------------------------------------------- | -------------------------------------------------------------- | ------------------------------------------------------ |
| `DataKey::Price(country, category, timestamp)` | Complete `PriceSubmission`, including price and audit metadata | Single-record reads (`get`, `get_valid`, `get_status`) |
| `DataKey::SubmissionIndex(country, category)`  | `Vec<u64>` of observation timestamps                           | Grouped history and latest-record reads                |
| `DataKey::SubmitterIndex(submitter)`           | `Vec<SubmitterEntry>` — `(country, category, timestamp)`       | Cursor-paginated per-submitter history reads           |

The index timestamps point to `Price` keys in the same country/category scope.
The observation timestamp is `valid_from`; duplicate submissions for the same
composite key leave the original value and index entry unchanged. Keeping
metadata inside `PriceSubmission` makes the record and its metadata atomic.

`SubmitterIndex` is appended in the same transaction as the record and the
group index, so the three views can never disagree about which records exist.

Treat existing key variants and component types as persistent schema. For an
incompatible layout change, add a versioned key and an explicit migration;
never silently repurpose or rename an existing key.

## PriceVault submission status

Each submission moves through a strict, one-way state machine:

```text
Pending ──► Verified
        └─► Rejected
```

`Verified` and `Rejected` are **terminal**. There is no path back to `Pending`
and no path between them. The allowed edges live in
`SubmissionStatus::can_transition_to`, which `set_status` consults — one table,
so the transition rules and the tests that exercise them cannot drift apart.

`Error` is `#[repr(u32)]`: its discriminants are part of the contract ABI,
because deployed clients map integer codes back to meanings. They are allocated
**contiguously from 1** and are **append-only**. A new condition gets the next
free number; an existing number is never reused or renumbered, even if the
condition it described is later removed.

## Observing status changes

`set_status` emits `StatusChanged` after the record is written:

| Field | Why it is there |
| --- | --- |
| `country_iso`, `category` | **Topics**, so an indexer can subscribe to the records it cares about without decoding every body |
| `timestamp` | Third component of the record's key — without it an event cannot address a record |
| `prior_status` | Distinguishes "became `Verified`" from "was already `Verified`" |
| `new_status` | The state the record now holds |
| `actor` | The address whose `require_auth` authorised the transition — the part an audit needs |

A refused transition emits nothing, and the record is left unchanged.

## Bounded history queries

`get_submitter_history(submitter, cursor, limit)` pages one submitter's
records.

`cursor` is an **offset into an append-only index**, which is what makes the
pagination stable: new submissions are appended, so an offset already handed
to a caller keeps pointing at the same position. A caller paging through
history never sees a record twice and never skips one because an unrelated
submission landed mid-pagination. An index entry whose record has since been
removed is skipped rather than failing the page.

`limit` must be `1..=MAX_HISTORY_PAGE_SIZE` (50); anything else returns
`Error::InvalidPageSize`. An unbounded history read is a denial-of-service
vector — a submitter with thousands of records would force a caller to pay for
reading all of them in one transaction — so the cap turns that into a bounded,
predictable cost and makes an over-large request fail loudly rather than being
silently truncated.

## The withdrawal lock is a window, not a countdown

A sentinel's lock is stored as one absolute ledger — `staked_at` — and runs
until `unlock_at = staked_at + lock_period`. `unlock_at(env, sentinel)` returns
that ledger, or `None` for an address that has never staked.

### Lock arithmetic is checked

`unlock_at` is derived with `checked_add`. An unrepresentable window returns
`InvalidLockPeriod`; it is never saturated to `u64::MAX`, because a saturated
window reads as "locked forever" without saying so.

`withdraw` compares ledgers directly — `now < unlock_at` — rather than computing
`now - staked_at` and comparing the difference. A saturating subtraction would
silently report "the lock has elapsed" for a window that has not been reached;
the direct comparison is exact at both boundaries.

`lock_period` is additionally bounded by `MAX_LOCK_PERIOD` (1 000 000) at
initialization, so every reachable unlock ledger is representable. The runtime
check is kept anyway: bounding the parameter at configuration time and checking
the arithmetic at use time are different guarantees.

### A top-up never extends a running lock

`stake` leaves `staked_at` untouched while `now < unlock_at`, and sets it to the
current ledger once that window has elapsed.

| Event | `staked_at` becomes |
| --- | --- |
| First deposit | current ledger |
| Top-up **while the lock is running** | unchanged — the window does not move |
| Top-up **at or after expiry** | current ledger — a fresh window |

Both halves of that matter:

* Letting a top-up move the window would let a sentinel push their own unlock
  date out indefinitely by depositing one unit repeatedly — the lock becomes a
  fee rather than a constraint.
* Resetting it unconditionally on every deposit is worse: a top-up landing one
  ledger before expiry would silently extend the lock by the full
  `lock_period`, which is a trap rather than a policy.

`stake` reads the existing start as an `Option`, not `unwrap_or(0)`, so a
deposit that lands on genesis ledger `0` is a real recorded window and not
mistaken for "never staked".

## Slash amounts are bounded before anything is written

`slash` checks three bounds, in order, all before the first write:

| Bound | Error |
| --- | --- |
| `proof_id` is non-empty | `InvalidProofId` |
| `amount > 0` | `InvalidSlashAmount` |
| `amount <= current stake` | `SlashExceedsStake` |
| remainder `>= min_stake` | `StakeBelowMinimum` |

The three amount bounds are distinct conditions with distinct remedies — a
malformed call, an unaffordable one, and one that would de-register the
sentinel — so each gets its own error instead of collapsing into one.

Because the balance map and the proof map are written only after every check
passes, a refused slash leaves **both** the stake and the proof identifier
untouched. An identifier that failed a call is still unused and still usable
exactly once.

## Slash proof identity

Replay protection keys on a caller-supplied `proof_id`, not on
`(sentinel, reason)`.

That pair was not unique in either direction:

* **False rejection.** Two genuinely distinct incidents against the same
  sentinel sharing a reason code (`fraudulent_submission`) collided, and the
  second was refused as a replay of the first.
* **False acceptance.** One incident relabelled with a different reason code
  passed twice.

`proof_id` is a non-empty `String` chosen by the caller and required to be unique
within the pool. `reason` stays a `Symbol` and an event topic for filtering, but
carries no replay semantics.

Uniqueness is global rather than per-sentinel: re-submitting an identifier is
refused identically whether the sentinel, the reason, or both differ. That is the
point — under the old `(sentinel, reason)` key the same case file could be
applied against a second address simply by changing one component of the key.

`slash_proof(proof_id)` returns the stored [`SlashProof`] — sentinel, reason,
amount and ledger — so an auditor holding an identifier can ask the contract
what it was applied to without replaying the event stream.

### Event schema

`SlashedEvent` gains `proof_id`, `remaining` and `slashed_at`. `proof_id` is
deliberately **not** a topic: it is high-cardinality, and adding it as a topic
would bloat the topic set of a consumer subscribed to "every slash for this
reason".

## Prerequisites

- Rust **stable** (1.84+ recommended; the Soroban environment supports 1.82+)
- The appropriate WebAssembly build target:
  - Rust 1.82+: `rustup target add wasm32v1-none`
  - Rust 1.81 or earlier: `rustup target add wasm32-unknown-unknown`
- `soroban-cli` (for deploying, optional for plain builds):
  ```sh
  cargo install --locked stellar-cli
  ```

## Build

Build the whole workspace, passing the target matching your toolchain (the
`.cargo/config.toml` sets the matching linker flags automatically):

```sh
# Rust 1.82+
cargo build --release --target wasm32v1-none

# Rust 1.81 or earlier
cargo build --release --target wasm32-unknown-unknown
```

Build a single contract:

```sh
cargo build --release --target wasm32v1-none -p kovara-index
```

The `.wasm` artifacts land in `target/<target>/release/`.

For the first-time dependency resolution / reproducible lockfile:

```sh
cargo generate-lockfile
# after a lockfile is present, `--locked` bakes reproducibility in:
cargo build --release --locked --target wasm32v1-none
```

## Test

```sh
cargo test
```

## Lint / format

```sh
cargo fmt --check
cargo clippy --release --target wasm32v1-none -- -D warnings
```

## Deploy (Testnet)

With `soroban-cli` installed (which selects the matching build target for you):

```sh
soroban contract build
soroban contract deploy \
  --wasm target/wasm32v1-none/release/kovara_index.wasm \
  --network testnet
```

Use the `kovara-index` contract address reported by the command as the
`CONTRACT_ID` for the indexer backend.

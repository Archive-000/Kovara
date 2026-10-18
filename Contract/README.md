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

## SentinelPool initialization

`SentinelPool` is only meaningful once it has been configured. Every
state-changing entry point — `stake`, `withdraw`, `set_threshold`,
`set_min_stake` and `slash` — opens by calling `require_initialized`, which
returns `Error::NotInitialized` when `DataKey::Admin` is absent. `is_initialized`
exposes the same fact as a view.

Without that guard the read-path defaults (`min_stake -> 0`,
`lock_period -> 0`) describe a real pool with **no minimum stake and no
withdrawal lock**: anyone could stake nothing, and `withdraw` would behave as
though no lock existed. The guard converts that accidental configuration into an
explicit, stable error.

`initialize` is all-or-nothing. Parameters are validated in a fixed order, each
step a pure predicate over the arguments, and all of them run **before**
`require_auth` and before the first write:

| Parameter | Rule | Error |
| --- | --- | --- |
| `min_stake` | `> 0` | `InvalidStake` |
| `lock_period` | `1 <= n <= MAX_LOCK_PERIOD` | `InvalidLockPeriod` |
| `threshold` | `> 0` | `InvalidThreshold` |

A refused call therefore leaves **no** partial configuration behind:
`is_initialized` keeps reporting `false`, `min_stake` and `threshold` keep
reporting `0`, and a later corrected call still succeeds. A second successful
initialization is refused with `Unauthorized`.

`MAX_LOCK_PERIOD` (1 000 000 ledgers) exists because the lock is measured in
`ledger.sequence()`, a `u32`. A lock that could not elapse would freeze every
sentinel's stake permanently, so the parameter is bounded at configuration time
rather than trusted.

## SentinelPool authorization

`set_threshold`, `set_min_stake` and `slash` all resolve the admin the same way,
through `require_admin`:

```text
stored Admin == caller  ->  authorized
anything else           ->  Unauthorized
```

The stored value is **mandatory**. The previous lookup fell back to
`unwrap_or_else(|| caller.clone())`, so on an unconfigured pool the comparison
`admin != stored_admin` was always false and whichever address authorized the
call became the admin by default. Requiring a stored admin removes that path;
combined with the initialization guard, an unconfigured pool cannot be
configured *or* governed by anyone.

## SentinelPool checked arithmetic

| Operation | Guard | Failure |
| --- | --- | --- |
| `stake` balance | `checked_add` | `InvalidStake` |
| `withdraw` remainder | `checked_sub` | `InsufficientBalance` |
| `slash` remainder | `checked_sub` | `InsufficientBalance` |

All three run **before** the balance map is written, so an overflow or an
excessive withdrawal leaves the sentinel's stake exactly as it was — no partial
write, no event. `stake(&sentinel, &i128::MAX)` therefore succeeds once (the
balance *is* `i128::MAX`) and is refused on replay, because the second call is
the one that would overflow.

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

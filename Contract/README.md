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

## The `min_stake` floor is a registration rule

An address holding a non-zero stake is a **registered sentinel**, and a
registered sentinel is expected to keep at least `min_stake` deposited for as
long as it stays registered. `stake` enforces the floor on the way in and
`withdraw` enforces it on the way out, so it holds at every balance the pool
ever exposes — including the very first one, because `stake` compares the
*resulting* balance rather than the increment.

The rule, stated plainly:

| Resulting balance | `stake` / `withdraw` |
| --- | --- |
| `== min_stake` | accepted |
| `> min_stake` | accepted |
| `< min_stake` | `StakeBelowMinimum` |
| `0` | `StakeBelowMinimum` |

So a **partial withdrawal may take a sentinel down to exactly `min_stake` and no
further**. Withdrawing the whole balance is not available through `withdraw`,
because a zero balance would silently de-register the address — and the pool has
no separate step recording *why* an address stopped being a sentinel. Leaving
the set is a lifecycle event with its own rules, not an arithmetic side effect
of an amount.

Raising `min_stake` above balances that already exist does not retroactively
invalidate them, and it does not trap those sentinels: every withdrawal they
attempt is refused with `StakeBelowMinimum` and their balance is preserved
intact.

## SentinelPool withdrawal accounting

`withdraw` runs its checks in a fixed order and writes the balance map only once
all of them pass:

| # | Check | Error |
| --- | --- | --- |
| 1 | `amount > 0` | `InvalidStake` |
| 2 | `amount <= balance` | `InsufficientBalance` |
| 3 | lock elapsed | `LockPeriodActive` |
| 4 | `balance - amount` via `checked_sub` | `InsufficientBalance` |
| 5 | `balance - amount >= min_stake` | `StakeBelowMinimum` |

Step 2 makes step 4 unreachable in practice; `checked_sub` is kept anyway so
that `remaining` is derived by checked arithmetic on the line that consumes it.
Because nothing is written until step 5 succeeds, **every** rejection — invalid
amount, excessive amount, active lock, below-minimum remainder — leaves the
sentinel's balance byte-for-byte unchanged and emits no event.

## SentinelPool stake queries

All three are views: they read storage and the ledger, write nothing, and emit
nothing, so an indexer can poll them without perturbing the pool.

| Call | Returns |
| --- | --- |
| `balance(sentinel)` | The deposited amount |
| `staked_at(sentinel)` | Ledger sequence number the current lock started on |
| `stake_of(sentinel)` | `SentinelStake { amount, staked_at, unlock_at, unlocked }` |
| `total_active_stake()` | Sum of every deposited amount |

`stake_of` is total: an address that never staked reports the zero record
(`amount == 0`, `staked_at == 0`, `unlocked == true`) rather than failing, so a
caller enumerating candidates never has to distinguish "not a sentinel yet" from
"the read failed". `unlock_at` saturates rather than overflowing — it is a
reporting value, and "the unlock ledger is unreachable" is more useful to a
caller than a failed read.

`total_active_stake` is **computed at read time**, not stored. A cached running
total is one forgotten update away from disagreeing with the per-address
balances it is supposed to summarize; folding the map costs one pass, which is
the same order as any governance read that must see every sentinel. Sums
saturate at `i128::MAX` instead of wrapping.

## Stake events

`StakedEvent` carries the resulting balance and the ledger the lock runs from:

| Field | Why it is there |
| --- | --- |
| `sentinel` | **Topic** — who deposited |
| `amount` | The increment, which alone does not reconstruct the position |
| `total` | The balance *after* this deposit; two equal deposits against different starting balances are very different events |
| `staked_at` | The ledger the deposit landed on, and therefore where the withdrawal lock starts |

`amount` and `total` are both needed by an indexer that rebuilds balances from
the event stream rather than by polling `balance`. The ledger sequence is read
once inside `stake` and reused for both `StakedAt` storage and the event, so the
stored timestamp and the published one cannot disagree.

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

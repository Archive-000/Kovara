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

## The pool-wide aggregate

`total_stake()` is a **stored running total**, seeded to `0` at initialization
and updated by `stake`, `withdraw` and `slash` in the same call that updates the
per-address balance.

Every mutation derives both values with checked arithmetic **before** either
write:

```text
stake:      new_aggregate = total.checked_add(amount)
withdraw:   new_aggregate = total.checked_sub(amount)
slash:      new_aggregate = total.checked_sub(amount)
```

The increment is read once and reused, so the balance and the total come from
one arithmetic step rather than two that could drift. A refused operation moves
neither side.

`the_aggregate_never_disagrees_with_the_balances` states the invariant
explicitly: `total_stake() == balance(a) + balance(b) + ...`. It is checked after
a withdrawal and again after a refused one.

The total saturating against a maximal balance is deliberate, not a bug:
`stake(i128::MAX)` succeeds, and the *next* deposit is refused with
`InvalidStake` because the aggregate would overflow — with the second address's
balance left at `0` rather than wrapped negative.

## The emergency pause

`pause` stops value movement without freezing the pool's configuration.

| Call | Paused | Not paused |
| --- | --- | --- |
| `stake` | `PoolPaused` | succeeds |
| `withdraw` | `PoolPaused` | succeeds |
| `slash` | `PoolPaused` | succeeds |
| `deregister` | `PoolPaused` | succeeds |
| `pause` | succeeds (idempotent) | succeeds |
| `unpause` | succeeds | refused — nothing to undo |
| `set_threshold`, `set_min_stake` | succeeds | succeeds |
| `set_governance`, `set_challenge_period` | succeeds | succeeds |
| every read | succeeds | succeeds |

Value movement stops; configuration and the emergency controls do not. A pause
that also froze `unpause` or the configuration setters would be a one-way trip
for the operator dealing with the incident.

`PauseChangedEvent` carries the pool-wide aggregate, so an indexer can see the
pause without a follow-up read.

Pausing is held by **governance**, not by the admin — stopping the pool from
moving value is an operational power, not a configuration one. `set_governance`
assigns it. Until that call is made, `pause` and `unpause` are refused with
`PauseNotAuthorized`; there is **no fallback to the caller**.

## Leaving the set

A registered sentinel's withdrawal may take the balance down to `min_stake` and
no further. Taking it to zero is what "leaving the set" means, and it is a
separate lifecycle step so that the reason is recorded:

```text
stake ──► registered ──deregister──► deregistered ──wait lock + challenge──► exited
```

* `deregister(sentinel)` records the ledger and the challenge period in force.
  The stake stays in the pool throughout — deregistering is not a withdrawal, and
  the balance is still there for a slash under review.
* A full exit (`remaining == 0`) requires a recorded deregistration
  (`NotDeregistered` otherwise) and an elapsed challenge period
  (`ChallengePeriodActive` otherwise). The `min_stake` floor does not apply to
  an exit.
* Partial withdrawals by a deregistered sentinel still respect the floor: the
  challenge period gates the *exit*, not ordinary withdrawals, which were always
  available down to the floor.
* `stake` is refused for a deregistered address (`AlreadyDeregistered`). Allowing
  a one-unit top-up during the challenge period would let a sentinel reset the
  window and skip the delay.
* Completing an exit clears the record, so a returning sentinel registers clean
  and a later exit needs its own deregistration and its own challenge period.

### The challenge period is not retroactive

`Deregistration { at, challenge_period }` copies the period that was in force at
deregistration. `set_challenge_period` afterwards cannot cut short a challenge
period that is already running — otherwise shortening it would be a way to
retroactively strip an exit right. `withdrawable_at(sentinel)` reports the
recorded deadline.

The period is bounded by `MAX_CHALLENGE_PERIOD_LEDGERS` (20 000 000) so
`at + period` stays representable in a `u64`; the default is
`DEFAULT_CHALLENGE_PERIOD_LEDGERS` (2 592 000 — thirty days), long enough for a
slash under review to conclude.

## Sentinel eligibility

`is_eligible(sentinel, at_ledger)` answers "would this address count as an
active sentinel at ledger N?" in one read:

1. the balance is at or above `min_stake`;
2. the address is not deregistered;
3. the withdrawal lock has elapsed at `at_ledger`.

The ledger is a parameter so a caller can ask the hypothetical question — "will
this sentinel be eligible at the next halving?" — without waiting for it.
Asking about a past ledger gives that ledger's answer. An unknown address
returns `false`, not an error: "not eligible" is the answer, not a failure.

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

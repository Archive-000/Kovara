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

## Submission windows

A submission's freshness is the half-open window `[valid_from, valid_until)`,
where `valid_until = valid_from + validity_window_secs`.

| `query_ts` | Fresh? |
| --- | --- |
| `< valid_from` | no — not yet valid |
| `>= valid_from` and `< valid_until` | **yes** |
| `>= valid_until` | no — expired |

Lower bound inclusive, upper bound exclusive, so consecutive non-overlapping
windows `[t, t+w)` and `[t+w, t+2w)` never both claim a ledger.

`window_bounds(valid_from, validity_window_secs)` is the one definition of the
window, reachable off-chain and callable by a cross-contract caller before a
transaction is built. Two inputs are refused rather than adjusted:

* a **zero** window — the record would be born already stale, and the
  submitter's stated intent about freshness would be silently discarded;
* an **overflowing** sum — wrapping or saturating `valid_from + window` would
  produce a window nobody asked for, and saturating would read as "valid
  forever" without saying so.

Both return `InvalidWindow`. `valid_from == 0` is separately refused as
`ZeroNotAllowed` by the payload schema, since a zero observation timestamp is
reserved.

## Duplicate submissions

A submission is refused with `Error::DuplicateSubmission` when
`(country_iso, category, valid_from)` is already occupied. The stored record and
its index entries are left untouched.

The refusal is explicit rather than a silent success. A caller that submits and
receives `Ok(())` has no way to distinguish "written" from "ignored", and neither
does an indexer — and the value it wanted to publish is *not* the value in
storage. Failing loudly is the only way the caller learns that.

The key is the record's identity, so neither the validity window nor the
submitter is part of it: varying either does not avoid the collision.

## Freshness reads

| Call | Returns |
| --- | --- |
| `is_fresh(country, category, valid_from, query_ts)` | `bool`; `false` for a missing record |
| `is_valid_at(...)` | Alias, delegating to `is_fresh` — one predicate, one answer |
| `get_fresh_history(country, category, query_ts)` | Only the records fresh at `query_ts` |
| `get_history(country, category)` | Everything, expired records included |

`get_fresh_history` is the read aggregation code should call. `get_history` is
what an audit wants and exactly what an index must not consume: a stale
observation folded into a median is a wrong index value with no error anywhere.

## Admin configuration

`configure_admin(admin)` is one-shot — a second call is refused with
`AdminNotConfigured`, so the authority cannot be handed to a new address as a
side effect of a retried transaction. The lookup has **no fallback to the
caller**: an unconfigured vault cannot be configured by whoever signs the
transaction.

Three policies, all admin-only, all event-emitting:

| Setter | Effect on `submit` | Event |
| --- | --- | --- |
| `set_max_validity_window(actor, Option<u64>)` | A longer window is `WindowTooLong` | `ValidityWindowPolicyUpdated` |
| `set_category_enabled(actor, category, bool)` | A disabled category is `CategoryDisabled` | `CategoryPolicyUpdated` |
| `set_submitter_allowed(actor, submitter, bool)` | A denied submitter is `SubmitterNotAllowed` | `SubmitterPolicyUpdated` |

Each policy is **opt-in and absent-means-unrestricted**: no entry for a category
means enabled, no entry for a submitter means allowed, no bound means any
window. A vault with no admin configured therefore behaves exactly as it did
before these setters existed, and a policy entry disables precisely what it
names and nothing else.

Policy checks run after the pure validation predicates and still **before**
`require_auth` and before any storage access, so a policy-refused submission
costs the caller one failed transaction and leaves storage untouched.

Each event carries `actor` as a topic — the address whose `require_auth`
authorised the change, which is the part an audit needs — and the subject
(category or submitter) as a second topic so a consumer can subscribe to one
category or one submitter without decoding every body.

## KovaraIndex freshness

`set_daily_index` cannot tell whether the observation behind a value was still
fresh when it was written, so an expired price folded into a daily index is
indistinguishable from a correct one.

`set_daily_index_windowed(updater, country, date, value, valid_from, valid_until, query_ts)`
applies the same half-open predicate and stores the window alongside the value:

| Condition | Error |
| --- | --- |
| `date == 0` | `InvalidDate` |
| `valid_until <= valid_from` | `InvalidWindow` |
| `query_ts < valid_from` | `NotYetValid` |
| `query_ts >= valid_until` | `StaleSubmission` |

All four are pure predicates evaluated before `require_auth` and before any
write, so a refused call leaves the previous index value intact. Value and
window are written together, so an entry can never carry a value without the
window that justifies it.

Reads: `is_fresh(valid_from, valid_until, query_ts)` is the pure predicate,
`get_daily_window` returns the window an entry was written under, and
`get_fresh_daily_index` returns the value only while that window is still fresh.
`None` from `get_daily_window` means "written without a window" — which
`get_daily_index` still returns, so the raw record stays auditable.

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

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

## Slashing is a governed, reversible process

A slash takes a sentinel's stake immediately, but it is the start of a process
rather than the end of one:

```text
 slash ──► Pending ──appeal──► Pending ──resolve──► Upheld | Reversed
              │                  │
              └──────────────────┘
         appeal only until `appeal_deadline`
```

### Governance authorization

Every slash operation — `slash`, `appeal_slash`, `resolve_slash` — requires the
configured governance address, resolved by `require_governance`:

| Stored `DataKey::Governance` | Caller | Result |
| --- | --- | --- |
| `Some(g)` | `g` | authorized |
| `Some(g)` | anything else | `NotGovernance` |
| absent | anything | `GovernanceNotConfigured` |

There is **no fallback to the caller**. An ungoverned pool cannot be slashed at
all, rather than being governed by whoever happened to sign the transaction.

Governance is deliberately a separate key from `Admin`. Collapsing them would
mean whoever can change `min_stake` can also confiscate stake. `set_governance`
lets the admin rotate it; the old address loses the power on the next ledger.

`initialize` takes `governance` explicitly rather than inferring it from `admin`
— the two may be the same address for a single-operator deployment, but the
contract never *assumes* that.

### Evidence references

`slash` takes a `SlashEvidence { proof_ref, uri, content_hash }`, stored on the
`SlashRecord` and republished on `SlashedEvent`. The three fields answer the
three questions an auditor asks: *which case*, *where is the record*, and *is
this the record I am looking at* — the hash pins the content so a mutable URI
cannot quietly change what the evidence says.

A record with an empty `proof_ref` or an empty `uri` is refused with
`InvalidEvidence`. The stake is taken immediately; a confiscation that cannot be
pointed at its justification cannot be appealed on evidence either.

`proof_ref` is a **data** field on the event, not a topic — it is
high-cardinality, and adding it as a topic would bloat the topic set of a
consumer filtering on "every slash for this reason".

### The appeal window

Each record carries the window in force when the slash was applied:

```text
appeal_deadline = slashed_at + appeal_window
```

An appeal is accepted while `ledger.sequence() <= appeal_deadline` and refused
from the ledger after it with `AppealWindowClosed`.

| Check | Meaning |
| --- | --- |
| `now <= appeal_deadline` | inside the window |
| `now > appeal_deadline` | `AppealWindowClosed` |
| `appeal_window == 0` | `AppealWindowClosed` — a zero window closes the deadline on the ledger the slash was applied |

**The window is copied onto the record, so it is not retroactive.**
`set_appeal_window` afterwards cannot close an appeal a sentinel is already
entitled to file — otherwise shortening the window would be a way to retroactively
strip an open appeal right.

`appeal_window` is bounded by `MAX_APPEAL_WINDOW_LEDGERS` (6 000 000) so the
deadline is representable in a `u64` for every reachable `slashed_at`. The
default is `DEFAULT_APPEAL_WINDOW_LEDGERS` (604 800 — seven days).

An appeal does **not** restore the stake. The slash stands until governance
resolves it; the appeal only marks the record as disputed.

The appellant may be the slashed sentinel or the governance address — the two
parties to the dispute. Anyone else is refused.

### Resolution is terminal and happens exactly once

`resolve_slash(governance, proof_ref, uphold)` upholds or reverses a slash once.
A second call is `AlreadyResolved`, whichever outcome it asks for. Without that
constraint a reversed slash could be reversed again, restoring the same stake
twice.

| `uphold` | Effect |
| --- | --- |
| `true` | Stake stays removed; `restored == 0`; outcome `Upheld` |
| `false` | `amount` added back with `checked_add`; outcome `Reversed` |

A reversal that would overflow `i128` is refused with `InsufficientBalance`
rather than wrapping the balance. The `outcome` check happens before every write,
so a refused resolution leaves both the record and the stake untouched and the
record is never half-resolved.

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

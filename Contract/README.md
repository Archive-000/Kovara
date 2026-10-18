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

## Access control

The static `registry` decides which identifiers the protocol *understands*. On
top of it, an admin-managed layer decides which of them are *currently
accepted*, so a governance action can narrow or widen acceptance without a
storage migration.

| Key | Stored value | Meaning when empty |
| --- | --- | --- |
| `DataKey::Admin` | `Address` | no admin configured |
| `DataKey::CountryAllowlist` | `Vec<Symbol>` | every registry country accepted |
| `DataKey::CategoryAllowlist` | `Vec<Symbol>` | every registry category accepted |
| `DataKey::Authorizations` | `Vec<Authorization>` | every submitter may write every category |

"Empty means unrestricted" is the property that makes this layer additive: a
contract that never calls `initialize` behaves exactly as it did before, and
clearing any list restores registry-only behaviour.

`Authorization { submitter, categories }` grants a submitter the named
categories; an **empty** `categories` grants every category. An entry that does
not cover a category is a refusal rather than a fall-through to a later entry,
so a narrow grant cannot be widened by a duplicate entry for the same address.

### The allowlists can only narrow

Every entry in an allowlist or authorization policy is validated against the
static registry before it is stored, so the configurable layer can never widen
what the registry accepts. `set_country_allowlist` with `"ZZ"` returns
`Error::InvalidCountry`; the same holds for categories and for categories named
inside an authorization policy.

### Refused before auth and before storage

All three checks run in `submit` **after** the static registry check but
**before** `require_auth` and before any storage access:

```text
registry::is_supported_country  ->  InvalidCountry
registry::is_supported_category ->  InvalidCategory
country_allowed                  ->  CountryNotAllowed
category_allowed                 ->  CategoryNotAllowed
submitter_allowed                ->  SubmitterNotAllowed
payload::validate / guards       ->  schema and arithmetic errors
                                  ---- require_auth ----
                                  storage writes
```

An unauthorised or disallowed call therefore costs the caller a handful of
reads and no auth prompt, and can never leave a partial write behind.

### Reading the policy before submitting

`is_submitter_authorized`, `is_country_allowed` and `is_category_allowed` let a
client check the current policy instead of discovering a refusal by submitting.

## Submission status transitions

```text
Pending ──► Verified
        └─► Rejected
```

`Verified` and `Rejected` are **terminal**. The allowed edges live in
`SubmissionStatus::can_transition_to`, which `set_status` consults — one table,
so the transition rules and the tests that exercise them cannot drift apart.

`can_transition(from, to)` publishes that same table on chain, so a sentinel
daemon can ask "would this transition be accepted?" without submitting a
transaction that is going to be rejected. It returns `false` rather than an
error for any terminal or invalid pair, so it can be used as a plain boolean
gate. Every refusal is the single stable code `Error::InvalidTransition`, and a
refused transition leaves the record byte-for-byte unchanged.

`Error` is `#[repr(u32)]`: its discriminants are part of the contract ABI,
because deployed clients map integer codes back to meanings. They are allocated
**contiguously from 1** and are **append-only**. A new condition gets the next
free number; an existing number is never reused or renumbered, even if the
condition it described is later removed.

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

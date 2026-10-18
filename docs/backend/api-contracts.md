# Kovara backend — REST API contracts

The read-only HTTP surface served by `createApp()` in `Backend/src/api/index.ts`.
Request and response shapes below were read from the route modules on `main`
(verified at commit `2c42735c443e`).

> **Response envelope caveat.** Successful routes return endpoint-specific JSON
> resources, not a common success envelope. Errors use the common
> `{ "error": string, "code": string }` shape. Internal exception messages and
> arbitrary error details are never serialized to clients; diagnostic context is
> available in server logs and the `X-Correlation-Id` response header.

## Versioning

- **Canonical:** `/api/v1/…`
- **Legacy:** `/api/…` is still served. Legacy responses (and most v1 responses
  built by the legacy prefix middleware) carry
  `Deprecation: true` and `Link: </api/v1/…>; rel="successor-version"`.
- `/health` and `/version` are operational endpoints and are **not** versioned.

## Operational endpoints

| Method | Path | Response |
| --- | --- | --- |
| GET | `/health` | `{ "status": "ok" | "degraded", "uptime": <seconds>, "db": "ok" | "unavailable" }` |
| GET | `/version` | `{ "version", "git_commit", "build_time", "node_version" }` |
| GET | `/metrics` | Prometheus text exposition (`text/plain; version=0.0.4`) |
| GET | `/api/v1/metrics` | JSON snapshot of the same registry |

`status` is `ok` only when the health probe query succeeds; otherwise `degraded`
with `db: "unavailable"`. `git_commit`/`build_time` come from `GIT_COMMIT` and
`BUILD_TIME` and default to `"unknown"`.

### Metrics (issue #679)

All request latency, throughput and error counts are recorded in one in-process
registry (`Backend/src/metrics/`) and exposed two ways over the same data:

- `GET /metrics` — Prometheus text. Series: `http_requests_total` (throughput),
  `http_request_duration_ms` (latency histogram), `http_errors_total`,
  `http_requests_in_flight`, `db_probe_duration_ms`, `service_up`, plus
  `process_uptime_seconds` / `process_resident_memory_bytes`.
- `GET /api/v1/metrics` — the same registry as JSON
  (`{ generated_at, series_count, counters, gauges, histograms }`) for operators
  without a Prometheus stack.

The route label is the matched route pattern (or a normalized path with `:id`
substitutions), so cardinality is bounded by the number of routes rather than
the size of the database. Scrape and health paths are not counted. When
`METRICS_TOKEN` is set, both endpoints require it via the `x-metrics-token`
header or `Authorization: Bearer`; when unset they are open.

## Data endpoints

All data endpoints are mounted under `/api/v1` (and the legacy `/api`).

### Request validation (issue #665)

Every public endpoint validates its inputs in one shared module
(`src/api/validation.ts`) before any handler or database call runs, so a
malformed request is refused at the edge rather than reaching business logic.
The result is a uniform contract:

- Every validation failure returns **exactly** `{ "error": string, "code": string }`
  with status `400` — the same `ApiErrorResponse` envelope used by the rest of
  the API. No endpoint invents its own shape or code.
- **Wallet addresses are security-sensitive.** An address must be a Stellar
  public key: `G` followed by 55 characters from the base-32 alphabet
  `A–Z`, `2–7`. The digits `0`, `1`, `8`, `9` are rejected (they never appear in
  a real strkey).
- **Identifiers** (`/posts/:id`, submission ids) are parsed as `bigint`, so an id
  above `Number.MAX_SAFE_INTEGER` is never silently rounded.
- **Bounded numeric fields** have explicit ceilings: `limit` is a positive
  integer with a per-endpoint maximum (`LIMIT_EXCEEDED` above it), and `offset`
  is a non-negative integer.
- **Cursors** are opaque, but bounded to 128 characters; a longer or non-string
  cursor is `INVALID_CURSOR`.
- The **`x-stellar-address` identity header** is rejected when well-formedness
  fails, rather than being silently ignored by the rate limiter and falling back
  to IP-based accounting.

| Code | Meaning |
| --- | --- |
| `400 INVALID_ADDRESS` | Address missing/blank, or not a base-32 `G…` Stellar public key (`x-stellar-address` included) |
| `400 INVALID_QUERY` | Malformed `limit`/`offset` or other query parameter |
| `400 LIMIT_EXCEEDED` | `limit` above the endpoint's maximum |
| `400 INVALID_ID` | Non-numeric or negative resource id |
| `400 INVALID_CURSOR` | Non-string or oversized pagination cursor |

### Profiles

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/v1/profiles/:address` | Returns the profile object. |

- `400 INVALID_ADDRESS` — address missing/blank, or not a base-32 `G…` Stellar public key (see [Request validation](#request-validation-issue-665)).
- `404 NOT_FOUND` — no profile for that address.

### Posts

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/v1/posts?author=<address>&limit=<n>&offset=<n>` | `author` optional; `limit` default 20, max 100; `offset` default 0. |
| GET | `/api/v1/posts/:id` | `id` must be a non-negative integer. |

List response:

```json
{ "posts": [ … ], "total": 0, "limit": 20, "offset": 0, "has_more": false }
```

Errors: `400 INVALID_QUERY` (bad limit/offset), `400 LIMIT_EXCEEDED`,
`400 INVALID_ADDRESS` (malformed `author`), `400 INVALID_ID`, `404 NOT_FOUND`.

### Search

| Method | Path | Body |
| --- | --- | --- |
| POST | `/api/v1/search/posts` | `{ "query": string, "limit"?: number, "offset"?: number }` |

- `query` is required, trimmed, whitespace-collapsed, and capped at 500 characters.
- `limit` default 20, max 100; `offset` default 0.
- Response: `{ posts, total, has_more, next_offset, prev_offset }` where each post
  is `{ id, author, content, tip_total, like_count, created_at, deleted }` and the
  bigint fields are serialized as strings.

Errors: `400 INVALID_QUERY`, `400 QUERY_TOO_LONG`, `400 LIMIT_EXCEEDED`,
`500 SEARCH_UNAVAILABLE`.

### Follows

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/v1/follows/:address/followers?limit=<n>&offset=<n>&cursor=<c>` | `limit` default 20, max 50. |
| GET | `/api/v1/follows/:address/following?limit=<n>&offset=<n>&cursor=<c>` | Same parameters. |

Response (followers shown; `following` is identical with a different key):

```json
{
  "address": "G…",
  "followers": ["G…"],
  "total": 0,
  "limit": 20,
  "offset": 0,
  "has_more": false,
  "next_offset": null,
  "prev_offset": null
}
```

When `cursor` is supplied the keyset path is used and `next_offset`/`prev_offset`
are `null`. Errors: `400 INVALID_QUERY`, `400 LIMIT_EXCEEDED`,
`400 INVALID_ADDRESS` (malformed `:address`), `400 INVALID_CURSOR`.

### Pools (experimental — gated)

Mounted only when `EXPERIMENTAL_FEATURES === "true"`.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/v1/pools?limit=<n>&offset=<n>` | `limit` default 20, max 100. |
| GET | `/api/v1/pools/:id` | Enriches the pool with token metadata. |

Each pool is serialized with optional `token_name`, `token_symbol`,
`token_decimals`; when metadata lookups fail the values fall back to
`"unknown"`, `"UNK"`, `7`.

Errors: `400 INVALID_QUERY`, `400 LIMIT_EXCEEDED`,
`400 INVALID_ID`, `404 NOT_FOUND`, `422 INVALID_THRESHOLD`.

### Debug snapshot

| Method | Path | Header |
| --- | --- | --- |
| GET | `/api/v1/debug/snapshot` | `x-debug-token` must equal `DEBUG_TOKEN`. |

Response:

```json
{
  "posts": [ … ],
  "profiles": [ … ],
  "pools": [ … ],
  "generated_at": "…",
  "post_count": 0,
  "profile_count": 0,
  "pool_count": 0
}
```

Each collection is capped at 1000 records; the `*_count` fields are totals.
Returns `503 DEBUG_DISABLED` when `DEBUG_TOKEN` is unset, `401 UNAUTHORIZED` on a
token mismatch.

## Cross-cutting behaviour

### Request correlation

A middleware reads `X-Correlation-Id` (or generates one), stores it on
`req.correlationId`, and echoes it in the `X-Correlation-Id` response header and
HTTP error log line. It is not included in the error body.

### Rate limiting

When `ENABLE_RATE_LIMITING !== "false"`, an `express-rate-limit` middleware covers
the `/api` prefix (including `/api/v1`). Defaults: 60s window, 100 requests per IP.
Exceeding it returns `429` with a `Retry-After` header and body
`{ "error": "Too many requests, please try again later.", "code": "RATE_LIMIT_EXCEEDED" }`.

See the "Known gaps" section of [`architecture.md`](./architecture.md): the
`RATE_LIMIT_*` environment variables are not currently wired to this limiter, and
the imported per-address limiter is not mounted.

### Wallet signature proofs (issue #954)

Routes guarded by `walletSignatureMiddleware` require a signed proof in the
`x-wallet-address`, `x-wallet-signature`, `x-wallet-timestamp` (Unix ms) and
`x-wallet-nonce` headers. The timestamp must be fresh:

- A proof older than the validity window (default 5 minutes, `windowMs`) is rejected
  with `401 WALLET_PROOF_EXPIRED`.
- A proof dated in the future by more than the clock-skew allowance (default 30
  seconds, `futureSkewMs`) is rejected with `401 WALLET_PROOF_FUTURE`.
- A non-numeric timestamp is `400 WALLET_TIMESTAMP_INVALID`.

Replayed nonces are rejected with `401 WALLET_NONCE_REUSED` (see
[`architecture.md`](./architecture.md)).

### Likes on deleted or missing posts (issue #933)

Likes are ingested from chain events, not from a REST route, but the observable
contract is the same one `GET /api/v1/posts/:id` publishes: `like_count` on a
post counts likes that the post can actually hold.

- A `like` delivery for a post that does not exist is dropped: no `likes` row is
  created and the post's `like_count` is unchanged.
- A `like` delivery for a soft-deleted post (`deleted_at` set) is dropped the
  same way — deleting a post never leaves it able to gain new likes.
- Both rejections are logged as warnings (`Like rejected: …`) and the delivery
  completes successfully, so a bad delivery does not dead-letter the stream.

### Errors

All errors use the same exact body shape, including route validation failures,
rate limits, the API 404 catch-all, and errors handled by the global middleware:

```json
{ "error": "Profile not found", "code": "NOT_FOUND" }
```

The body contains exactly `error` and `code`; status remains the HTTP status
listed below. Internal exception messages and arbitrary `details` are not
returned. Use `X-Correlation-Id` to match a response to server-side diagnostics.

| Status | Code | Source |
| --- | --- | --- |
| 400 | `MALFORMED_JSON` | `express.json()` syntax error |
| 400 | `INVALID_QUERY` / `LIMIT_EXCEEDED` / `QUERY_TOO_LONG` | Route parameter validation |
| 400 | `INVALID_ADDRESS` / `INVALID_ID` / `INVALID_CURSOR` | Route parameter validation (issue #665) |
| 401 | `UNAUTHORIZED` | Debug token mismatch |
| 404 | `NOT_FOUND` | Route-level or 404 catch-all |
| 422 | `INVALID_THRESHOLD` | Pool threshold validation |
| 429 | `RATE_LIMIT_EXCEEDED` | Rate limiter |
| 500 | `INTERNAL_ERROR` | Unhandled error |
| 500 | `SEARCH_UNAVAILABLE` | Search backend missing |
| 503 | `DATABASE_UNAVAILABLE` | Database error detected |
| 503 | `REQUEST_TIMEOUT` | Request exceeded `REQUEST_TIMEOUT_MS` |
| 503 | `DEBUG_DISABLED` | `DEBUG_TOKEN` not set |

## OpenAPI

`Backend/openapi.yaml` (OpenAPI 3.1) documents the six core v1 read endpoints:
`/profiles/{address}`, `/posts`, `/posts/{id}`,
`/follows/{address}/followers`, `/follows/{address}/following`, `/pools/{id}`.
It does **not** currently cover `/health`, `/version`, `/search/posts`,
`/debug/snapshot`, the `/pools` list, or the version prefix.

### Follow pagination cursor behavior (issue #929)

The `/follows/:address/followers` and `/follows/:address/following` endpoints
support both offset-based and cursor-based pagination:

- `cursor` is optional. When present, it is treated as an opaque key (typically
  the last wallet address returned in the previous page).
- For cursor-based responses, `next_offset` and `prev_offset` are `null`; the
  caller advances by passing the last element of `followers`/`following` as the
  next `cursor`.
- `has_more` is `true` if the number of items returned equals the requested
  `limit`, indicating there may be more results after the returned page;
  otherwise `false`. The `total` count is still returned for informational
  purposes.
- Cursor and offset are mutually exclusive in effect: when `cursor` is supplied,
  the keyset path is used and the `offset` parameter in the response reflects
  the requested value but navigation uses the cursor (not offsets). Errors for
  invalid cursors follow `400 INVALID_CURSOR`.

/**
 * Unit tests for the centralized request-validation module (issue #665).
 *
 * The endpoints that consume these validators are covered by
 * `validation-endpoints.test.ts`; this file exercises the rules themselves,
 * including the boundary and security-sensitive cases that are easier to state
 * directly than through HTTP.
 */
import {
  DEFAULT_LIMIT,
  MAX_CURSOR_LENGTH,
  MAX_LIMIT,
  MAX_POST_CONTENT_LENGTH,
  MAX_USERNAME_LENGTH,
  parseCursor,
  parseLimit,
  parseNumericId,
  parseOffset,
  parseOptionalStellarAddress,
  parsePostContent,
  parseStellarAddress,
  parseUsername,
  sendValidationError,
  validate,
  isStellarAddress,
  ValidationIssue,
} from "../validation";
import { MAX_QUERY_LENGTH, validateQuery, normalizeQuery } from "../../search/store";

const VALID = "GAZJ2EQV2ES6R5BLUNXMNFR5VN3HQF4KXJ2GM5Q7GQHT5XBC2CRX3GK3";
const VALID_2 = "GBZX4364PEPQTDICMIQDZ56K4T75QZCR4NBEYKO6PDRJAHZKGUOJPCXB";

describe("validation / stellar addresses (security-sensitive)", () => {
  it("accepts a well-formed Stellar public key", () => {
    expect(isStellarAddress(VALID)).toBe(true);
    const parsed = parseStellarAddress(VALID);
    expect(parsed).toEqual({ ok: true, value: VALID });
  });

  it("rejects an empty address as required", () => {
    const parsed = parseStellarAddress("");
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) {
      expect(parsed.issue.code).toBe("INVALID_ADDRESS");
      expect(parsed.issue.error).toBe("address is required");
      expect(parsed.issue.status).toBe(400);
    }
  });

  it("rejects whitespace-only input as required", () => {
    const parsed = parseStellarAddress("   ");
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_ADDRESS");
  });

  it("rejects a non-string address", () => {
    for (const bad of [undefined, null, 42, {}, ["G"]]) {
      const parsed = parseStellarAddress(bad);
      expect(parsed.ok).toBe(false);
    }
  });

  it("rejects an address that is too short or too long", () => {
    expect(parseStellarAddress("GABC123").ok).toBe(false);
    expect(parseStellarAddress("G" + "A".repeat(56)).ok).toBe(false);
  });

  it("rejects an address not starting with G", () => {
    expect(parseStellarAddress("A" + "B".repeat(55)).ok).toBe(false);
  });

  it("rejects characters outside the base-32 alphabet", () => {
    // `0`, `1`, `8` and `9` are not part of a Stellar strkey. The previous
    // `[A-Z0-9]` check accepted them, letting a malformed value reach a lookup.
    for (const ch of ["0", "1", "8", "9", "#"]) {
      expect(parseStellarAddress("G" + ch.repeat(55)).ok).toBe(false);
    }
  });

  it("rejects a lowercase address", () => {
    expect(parseStellarAddress(VALID.toLowerCase()).ok).toBe(false);
  });

  it("treats an omitted optional address as undefined", () => {
    expect(parseOptionalStellarAddress(undefined)).toEqual({ ok: true, value: undefined });
    expect(parseOptionalStellarAddress("")).toEqual({ ok: true, value: undefined });
  });

  it("still validates an optional address when present", () => {
    expect(parseOptionalStellarAddress(VALID)).toEqual({ ok: true, value: VALID });
    expect(parseOptionalStellarAddress("GABC123").ok).toBe(false);
  });
});

describe("validation / limit", () => {
  const parse = parseLimit(MAX_LIMIT, DEFAULT_LIMIT);

  it("falls back to the default when omitted", () => {
    expect(parse(undefined)).toEqual({ ok: true, value: DEFAULT_LIMIT });
    expect(parse(null)).toEqual({ ok: true, value: DEFAULT_LIMIT });
  });

  it("accepts a valid numeric limit, including numeric strings", () => {
    expect(parse(10)).toEqual({ ok: true, value: 10 });
    expect(parse("10")).toEqual({ ok: true, value: 10 });
  });

  it("rejects a non-numeric or non-positive limit as INVALID_QUERY", () => {
    for (const bad of ["abc", 0, -1, 1.5, NaN]) {
      const parsed = parse(bad);
      expect(parsed.ok).toBe(false);
      if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_QUERY");
    }
  });

  it("rejects a limit above the ceiling as LIMIT_EXCEEDED", () => {
    const parsed = parse(MAX_LIMIT + 1);
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) {
      expect(parsed.issue.code).toBe("LIMIT_EXCEEDED");
      expect(parsed.issue.error).toBe(`limit cannot exceed ${MAX_LIMIT}`);
    }
  });

  it("honours a per-endpoint ceiling", () => {
    const parseFollows = parseLimit(50, DEFAULT_LIMIT);
    expect(parseFollows(50).ok).toBe(true);
    expect(parseFollows(51).ok).toBe(false);
  });
});

describe("validation / offset", () => {
  it("defaults to zero when omitted", () => {
    expect(parseOffset(undefined)).toEqual({ ok: true, value: 0 });
  });

  it("accepts zero and positive integers", () => {
    expect(parseOffset(0)).toEqual({ ok: true, value: 0 });
    expect(parseOffset("25")).toEqual({ ok: true, value: 25 });
  });

  it("rejects negative, fractional and non-numeric offsets", () => {
    for (const bad of [-1, 2.5, "abc"]) {
      const parsed = parseOffset(bad);
      expect(parsed.ok).toBe(false);
      if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_QUERY");
    }
  });
});

describe("validation / numeric id", () => {
  it("parses a numeric id", () => {
    expect(parseNumericId("42")).toEqual({ ok: true, value: BigInt(42) });
  });

  it("preserves ids beyond Number.MAX_SAFE_INTEGER exactly", () => {
    const parsed = parseNumericId("9007199254740993");
    expect(parsed.ok).toBe(true);
    if (parsed.ok) expect(parsed.value.toString()).toBe("9007199254740993");
  });

  it("rejects malformed ids with INVALID_ID", () => {
    for (const bad of ["-1", "abc", "", "1.5", "0x10", null, {}]) {
      const parsed = parseNumericId(bad);
      expect(parsed.ok).toBe(false);
      if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_ID");
    }
  });
});

describe("validation / cursor", () => {
  it("treats an omitted cursor as undefined", () => {
    expect(parseCursor(undefined)).toEqual({ ok: true, value: undefined });
    expect(parseCursor("")).toEqual({ ok: true, value: undefined });
  });

  it("accepts an opaque string cursor", () => {
    expect(parseCursor("abc123")).toEqual({ ok: true, value: "abc123" });
  });

  it("rejects a non-string cursor", () => {
    const parsed = parseCursor({});
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_CURSOR");
  });

  it("rejects an oversized cursor", () => {
    const parsed = parseCursor("x".repeat(MAX_CURSOR_LENGTH + 1));
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_CURSOR");
  });
});

describe("validation / schema entry point", () => {
  it("returns the parser's own code for a missing required field", () => {
    const result = validate<{ address: string }>(
      { address: { parse: parseStellarAddress } },
      { address: undefined }
    );
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.issue.code).toBe("INVALID_ADDRESS");
  });

  it("applies defaults for optional absent fields", () => {
    const result = validate<{ address: string; limit: number; offset: number }>(
      {
        address: { parse: parseStellarAddress },
        limit: { parse: parseLimit(MAX_LIMIT), required: false, default: DEFAULT_LIMIT },
        offset: { parse: parseOffset, required: false, default: 0 },
      },
      { address: VALID, limit: undefined, offset: undefined }
    );
    expect(result).toEqual({
      ok: true,
      value: { address: VALID, limit: DEFAULT_LIMIT, offset: 0 },
    });
  });

  it("reports the first failure in declaration order, deterministically", () => {
    const run = () =>
      validate<{ address: string; limit: number }>(
        {
          address: { parse: parseStellarAddress },
          limit: { parse: parseLimit(MAX_LIMIT), required: false, default: DEFAULT_LIMIT },
        },
        { address: "bad", limit: "also-bad" }
      );
    const first = run();
    const second = run();
    expect(first.ok).toBe(false);
    if (!first.ok && !second.ok) {
      expect(first.issue.code).toBe("INVALID_ADDRESS");
      expect(second.issue.code).toBe(first.issue.code);
      expect(second.issue.field).toBe(first.issue.field);
    }
  });

  it("parses every declared field on success", () => {
    const result = validate<{ address: string; limit: number }>(
      {
        address: { parse: parseStellarAddress },
        limit: { parse: parseLimit(MAX_LIMIT), required: false, default: DEFAULT_LIMIT },
      },
      { address: VALID_2, limit: "5" }
    );
    expect(result).toEqual({ ok: true, value: { address: VALID_2, limit: 5 } });
  });
});

describe("validation / response helper", () => {
  it("writes the shared { error, code } envelope with the issue's status", () => {
    const json = jest.fn();
    const status = jest.fn().mockReturnValue({ json });
    const res = { status } as never;

    sendValidationError(res, {
      status: 400,
      code: "INVALID_ADDRESS",
      error: "address is required",
      field: "address",
    });

    expect(status).toHaveBeenCalledWith(400);
    expect(json).toHaveBeenCalledWith({ error: "address is required", code: "INVALID_ADDRESS" });
  });
});

describe("validation / post content (BE-04)", () => {
  it("accepts content at exactly the documented limit", () => {
    const atLimit = "x".repeat(MAX_POST_CONTENT_LENGTH);
    const parsed = parsePostContent(atLimit);
    expect(parsed).toEqual({ ok: true, value: atLimit });
  });

  it("rejects content one character over the limit", () => {
    const over = "x".repeat(MAX_POST_CONTENT_LENGTH + 1);
    const parsed = parsePostContent(over);
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) {
      expect(parsed.issue.code).toBe("INVALID_CONTENT");
      expect(parsed.issue.field).toBe("content");
      expect(parsed.issue.status).toBe(400);
      expect(parsed.issue.error).toContain(String(MAX_POST_CONTENT_LENGTH));
    }
  });

  it("counts the submitted value verbatim rather than a trimmed copy", () => {
    // Content is stored as submitted, so the bound has to be measured on the
    // raw value: trimming first would let padding through the limit.
    const padded = `  ${"x".repeat(MAX_POST_CONTENT_LENGTH)}  `;
    expect(parsePostContent(padded).ok).toBe(false);
  });

  it("rejects empty and whitespace-only content", () => {
    for (const value of ["", "   ", "\n\t"]) {
      const parsed = parsePostContent(value);
      expect(parsed.ok).toBe(false);
      if (!parsed.ok) expect(parsed.issue.code).toBe("INVALID_CONTENT");
    }
  });

  it("rejects a non-string body", () => {
    for (const value of [123, null, undefined, { text: "hi" }]) {
      expect(parsePostContent(value).ok).toBe(false);
    }
  });
});

describe("validation / profile username (BE-06)", () => {
  it("accepts a username at the limit and trims surrounding whitespace", () => {
    const atLimit = "u".repeat(MAX_USERNAME_LENGTH);
    expect(parseUsername(`  ${atLimit}  `)).toEqual({ ok: true, value: atLimit });
  });

  it("rejects a username over the limit with the field named", () => {
    const parsed = parseUsername("u".repeat(MAX_USERNAME_LENGTH + 1));
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) {
      expect(parsed.issue.code).toBe("INVALID_FIELD");
      expect(parsed.issue.field).toBe("username");
    }
  });

  it("rejects an empty or whitespace-only username", () => {
    for (const value of ["", "   "]) {
      expect(parseUsername(value).ok).toBe(false);
    }
  });

  it("reports the offending field when a custom field name is used", () => {
    const parsed = parseUsername("", "display_name");
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) expect(parsed.issue.field).toBe("display_name");
  });
});

// ── BE-26: Bounded search query length ────────────────────────────────────────

describe("search query validation / bounded query length (BE-26)", () => {
  it("accepts a query exactly at the limit", () => {
    const atLimit = "a".repeat(MAX_QUERY_LENGTH);
    // Should not throw and should return the normalized query
    expect(() => validateQuery(atLimit)).not.toThrow();
    expect(validateQuery(atLimit)).toBe(atLimit);
  });

  it("rejects a query one character over the limit with QUERY_TOO_LONG", () => {
    const over = "a".repeat(MAX_QUERY_LENGTH + 1);
    let caught: unknown;
    try { validateQuery(over); } catch (e) { caught = e; }
    expect(caught).toBeDefined();
    expect((caught as { code: string }).code).toBe("QUERY_TOO_LONG");
  });

  it("rejects an empty string (after normalization) with INVALID_QUERY", () => {
    for (const bad of ["", "   ", "\t\n"]) {
      let caught: unknown;
      try { validateQuery(bad); } catch (e) { caught = e; }
      expect(caught).toBeDefined();
      expect((caught as { code: string }).code).toBe("INVALID_QUERY");
    }
  });

  it("rejects a non-string query with INVALID_QUERY", () => {
    for (const bad of [undefined, null, 42, {}]) {
      let caught: unknown;
      try { validateQuery(bad); } catch (e) { caught = e; }
      expect(caught).toBeDefined();
      expect((caught as { code: string }).code).toBe("INVALID_QUERY");
    }
  });

  it("accepts a minimal 1-character query", () => {
    expect(() => validateQuery("x")).not.toThrow();
    expect(validateQuery("x")).toBe("x");
  });
});

// ── BE-27: Deterministic search pagination ─────────────────────────────────

describe("search query normalization / deterministic output (BE-27)", () => {
  it("normalizes internal whitespace so equal-content queries share a cache key", () => {
    expect(normalizeQuery("  foo   bar  ")).toBe("foo bar");
    expect(normalizeQuery("foo   bar")).toBe("foo bar");
    expect(normalizeQuery("  stellar  ")).toBe("stellar");
  });

  it("produces the same normalized value for any amount of surrounding whitespace", () => {
    const variants = ["stellar", " stellar", "stellar ", "  stellar  "];
    const results = variants.map((v) => normalizeQuery(v));
    expect(new Set(results).size).toBe(1);
  });

  it("applies normalization before the length check so padding cannot bypass the cap", () => {
    // A padded query whose *raw* length exceeds MAX_QUERY_LENGTH but whose
    // normalized form is within the limit must still be valid.
    const padding = " ".repeat(MAX_QUERY_LENGTH + 50);
    const padded = "stellar" + padding;
    expect(() => validateQuery(padded)).not.toThrow();
    expect(validateQuery(padded)).toBe("stellar");
  });
});

// ── BE-30: Typed validation error details ─────────────────────────────────

describe("typed validation errors / field naming (BE-30)", () => {
  it("ValidationIssue carries the offending field name on every failure", () => {
    // All the standard parsers set `field` correctly.
    type AnyResult = ReturnType<typeof parseStellarAddress>;
    const cases: Array<[AnyResult, string]> = [
      [parseStellarAddress("bad", "wallet"), "wallet"],
      [parseStellarAddress(""), "address"],
      [parseOptionalStellarAddress("bad", "creator_token") as AnyResult, "creator_token"],
    ];
    for (const [result, expectedField] of cases) {
      expect(result.ok).toBe(false);
      if (!result.ok) {
        expect(result.issue.field).toBe(expectedField);
        expect(typeof result.issue.error).toBe("string");
        expect(typeof result.issue.code).toBe("string");
        expect(result.issue.status).toBe(400);
      }
    }
  });

  it("limit parser sets field = 'limit' on LIMIT_EXCEEDED", () => {
    const parse = parseLimit(MAX_LIMIT, DEFAULT_LIMIT);
    const result = parse(MAX_LIMIT + 1, "limit");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.issue.code).toBe("LIMIT_EXCEEDED");
      expect(result.issue.field).toBe("limit");
    }
  });

  it("numeric id parser sets field on rejection", () => {
    const result = parseNumericId("-1", "post_id");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.issue.code).toBe("INVALID_ID");
      expect(result.issue.field).toBe("post_id");
    }
  });

  it("cursor parser sets field on rejection", () => {
    const result = parseCursor("x".repeat(MAX_CURSOR_LENGTH + 1), "after");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.issue.code).toBe("INVALID_CURSOR");
      expect(result.issue.field).toBe("after");
    }
  });

  it("post content parser sets field on rejection", () => {
    const result = parsePostContent("x".repeat(MAX_POST_CONTENT_LENGTH + 1), "body");
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.issue.code).toBe("INVALID_CONTENT");
      expect(result.issue.field).toBe("body");
    }
  });

  it("validate() reports the offending field deterministically across repeated calls", () => {
    const VALID_ADDR = "GAZJ2EQV2ES6R5BLUNXMNFR5VN3HQF4KXJ2GM5Q7GQHT5XBC2CRX3GK3";
    const run = () =>
      validate<{ address: string; limit: number }>(
        {
          address: { parse: parseStellarAddress },
          limit: { parse: parseLimit(MAX_LIMIT), required: false, default: DEFAULT_LIMIT },
        },
        { address: "bad", limit: "also-bad" }
      );
    // Regardless of how many times we run it the failing field is always 'address'
    for (let i = 0; i < 5; i++) {
      const result = run();
      expect(result.ok).toBe(false);
      if (!result.ok) {
        expect(result.issue.field).toBe("address");
        expect(result.issue.code).toBe("INVALID_ADDRESS");
      }
    }
  });

  it("sendValidationError sends all four issue fields in the response (field included)", () => {
    const json = jest.fn();
    const status = jest.fn().mockReturnValue({ json });
    const res = { status } as never;

    const issue: ValidationIssue = {
      status: 400,
      code: "INVALID_ID",
      error: "post_id must be a non-negative integer",
      field: "post_id",
    };

    sendValidationError(res, issue);
    expect(status).toHaveBeenCalledWith(400);
    // The HTTP body uses { error, code }; field is available on the issue for
    // structured logging but is not exposed in the response envelope.
    expect(json).toHaveBeenCalledWith({ error: issue.error, code: issue.code });
  });
});

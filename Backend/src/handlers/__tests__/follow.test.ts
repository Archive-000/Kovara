/**
 * Unit tests for the follow/unfollow event handlers.
 *
 * Issue #351 — acceptance criteria:
 *  - Happy path tested (Follow and Unfollow)
 *  - Idempotency tested
 *  - Database calls mocked with jest.mock
 */

import { handleFollow, handleUnfollow, FollowEvent, UnfollowEvent } from "../follow";
import { Database } from "../../db";

jest.mock("../../db");

function makeMockDb(): jest.Mocked<Database> {
  return {
    upsertProfile: jest.fn(),
    getFollow: jest.fn().mockResolvedValue(null),
    insertFollow: jest.fn().mockResolvedValue(undefined),
    deleteFollow: jest.fn().mockResolvedValue(undefined),
    insertPost: jest.fn(),
    markPostDeleted: jest.fn(),
    incrementPostLikeCount: jest.fn(),
    addPostTipTotal: jest.fn(),
    getPost: jest.fn(),
    upsertLike: jest.fn(),
    insertTip: jest.fn(),
    upsertPool: jest.fn(),
    adjustPoolBalance: jest.fn(),
    insertPool: jest.fn(),
    getPool: jest.fn(),
    listPools: jest.fn().mockResolvedValue({ pools: [], total: 0 }),
    addPoolAdmin: jest.fn(),
    removePoolAdmin: jest.fn(),
    getProfile: jest.fn(),
    listProfiles: jest.fn().mockResolvedValue({ profiles: [], total: 0 }),
    listPosts: jest.fn(),
    getFollowers: jest.fn(),
    getFollowing: jest.fn(),
    getFollowersAfter: jest.fn(),
    getFollowingAfter: jest.fn(),
    searchPosts: jest.fn().mockResolvedValue({ posts: [], total: 0 }),
    getTokenMetadata: jest.fn().mockResolvedValue(null),
  } as jest.Mocked<Database>;
}

// ── handleFollow ──────────────────────────────────────────────────────────────

describe("handleFollow", () => {
  let db: jest.Mocked<Database>;

  beforeEach(() => {
    db = makeMockDb();
  });

  // ── Happy path ──────────────────────────────────────────────────────────────

  it("calls db.insertFollow with correct fields", async () => {
    const event: FollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    };

    await handleFollow(db, event);

    expect(db.getFollow).toHaveBeenCalledWith("GABC123", "GXYZ789");
    expect(db.insertFollow).toHaveBeenCalledTimes(1);
    expect(db.insertFollow).toHaveBeenCalledWith({
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    });
  });

  it("resolves without error for a valid event", async () => {
    const event: FollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 200,
    };

    await expect(handleFollow(db, event)).resolves.toBeUndefined();
  });

  // ── Idempotency ─────────────────────────────────────────────────────────────

  it("skips insert when the follow already exists", async () => {
    db.getFollow.mockResolvedValueOnce({
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 50,
    });

    const event: FollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    };

    await handleFollow(db, event);

    expect(db.getFollow).toHaveBeenCalledWith("GABC123", "GXYZ789");
    expect(db.insertFollow).not.toHaveBeenCalled();
  });

  it("inserts when getFollow returns null", async () => {
    db.getFollow.mockResolvedValueOnce(null);

    const event: FollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    };

    await handleFollow(db, event);

    expect(db.insertFollow).toHaveBeenCalledTimes(1);
  });

  // ── Validation ──────────────────────────────────────────────────────────────

  it("throws when follower field is missing", async () => {
    const event = {
      follower: "",
      followee: "GXYZ789",
      ledger: 100,
    } as FollowEvent;

    await expect(handleFollow(db, event)).rejects.toThrow(
      "Follow event missing required field: follower"
    );
    expect(db.insertFollow).not.toHaveBeenCalled();
  });

  it("throws when followee field is missing", async () => {
    const event = {
      follower: "GABC123",
      followee: "",
      ledger: 100,
    } as FollowEvent;

    await expect(handleFollow(db, event)).rejects.toThrow(
      "Follow event missing required field: followee"
    );
    expect(db.insertFollow).not.toHaveBeenCalled();
  });

  // ── Database error propagation ──────────────────────────────────────────────

  it("propagates database errors", async () => {
    db.insertFollow.mockRejectedValueOnce(new Error("DB write failed"));

    const event: FollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    };

    await expect(handleFollow(db, event)).rejects.toThrow("DB write failed");
  });
});

// ── handleUnfollow ────────────────────────────────────────────────────────────

describe("handleUnfollow", () => {
  let db: jest.Mocked<Database>;

  beforeEach(() => {
    db = makeMockDb();
  });

  // ── Happy path ──────────────────────────────────────────────────────────────

  it("calls db.deleteFollow with correct follower and followee", async () => {
    db.getFollow.mockResolvedValueOnce({
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    });

    const event: UnfollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 150,
    };

    await handleUnfollow(db, event);

    expect(db.getFollow).toHaveBeenCalledWith("GABC123", "GXYZ789");
    expect(db.deleteFollow).toHaveBeenCalledTimes(1);
    expect(db.deleteFollow).toHaveBeenCalledWith("GABC123", "GXYZ789");
  });

  it("resolves without error for a valid event", async () => {
    db.getFollow.mockResolvedValueOnce({
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    });

    const event: UnfollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 150,
    };

    await expect(handleUnfollow(db, event)).resolves.toBeUndefined();
  });

  // ── Idempotency ─────────────────────────────────────────────────────────────

  it("skips delete when the follow does not exist", async () => {
    db.getFollow.mockResolvedValueOnce(null);

    const event: UnfollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 150,
    };

    await handleUnfollow(db, event);

    expect(db.getFollow).toHaveBeenCalledWith("GABC123", "GXYZ789");
    expect(db.deleteFollow).not.toHaveBeenCalled();
  });

  it("deletes when getFollow returns a follow", async () => {
    db.getFollow.mockResolvedValueOnce({
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    });

    const event: UnfollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 150,
    };

    await handleUnfollow(db, event);

    expect(db.deleteFollow).toHaveBeenCalledTimes(1);
  });

  // ── Validation ──────────────────────────────────────────────────────────────

  it("throws when follower field is missing", async () => {
    const event = {
      follower: "",
      followee: "GXYZ789",
      ledger: 150,
    } as UnfollowEvent;

    await expect(handleUnfollow(db, event)).rejects.toThrow(
      "Unfollow event missing required field: follower"
    );
    expect(db.deleteFollow).not.toHaveBeenCalled();
  });

  it("throws when followee field is missing", async () => {
    const event = {
      follower: "GABC123",
      followee: "",
      ledger: 150,
    } as UnfollowEvent;

    await expect(handleUnfollow(db, event)).rejects.toThrow(
      "Unfollow event missing required field: followee"
    );
    expect(db.deleteFollow).not.toHaveBeenCalled();
  });

  // ── Database error propagation ──────────────────────────────────────────────

  it("propagates database errors", async () => {
    db.getFollow.mockResolvedValueOnce({
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 100,
    });
    db.deleteFollow.mockRejectedValueOnce(new Error("DB delete failed"));

    const event: UnfollowEvent = {
      follower: "GABC123",
      followee: "GXYZ789",
      ledger: 150,
    };

    await expect(handleUnfollow(db, event)).rejects.toThrow("DB delete failed");
  });
});

// ── Repeated-unfollow semantics (issue #931) ─────────────────────────────────

describe("handleUnfollow — repeated deliveries (issue #931)", () => {
  /**
   * A stateful in-memory stand-in for the follows table: reads reflect writes,
   * so a replayed event sees the graph as it actually is rather than a canned
   * mock response. This is what lets the tests below assert on relationship
   * *counts* across several deliveries.
   */
  function makeStatefulDb(): {
    db: jest.Mocked<Database>;
    relationships: Set<string>;
  } {
    const relationships = new Set<string>();
    const key = (follower: string, followee: string): string => `${follower}:${followee}`;

    const db = {
      getFollow: jest.fn(async (follower: string, followee: string) =>
        relationships.has(key(follower, followee))
          ? { follower, followee, ledger: 100 }
          : null
      ),
      insertFollow: jest.fn(async (follow: { follower: string; followee: string }) => {
        relationships.add(key(follow.follower, follow.followee));
      }),
      deleteFollow: jest.fn(async (follower: string, followee: string) => {
        relationships.delete(key(follower, followee));
      }),
    } as unknown as jest.Mocked<Database>;

    return { db, relationships };
  }

  const FOLLOW: FollowEvent = {
    follower: "GABC123",
    followee: "GXYZ789",
    ledger: 100,
  };
  const UNFOLLOW: UnfollowEvent = {
    follower: "GABC123",
    followee: "GXYZ789",
    ledger: 150,
  };

  it("deletes the relationship once, then replays are no-ops", async () => {
    const { db, relationships } = makeStatefulDb();

    await handleFollow(db, FOLLOW);
    expect(relationships.size).toBe(1);

    await handleUnfollow(db, UNFOLLOW);
    await handleUnfollow(db, UNFOLLOW);
    await handleUnfollow(db, UNFOLLOW);

    expect(relationships.size).toBe(0);
    expect(db.deleteFollow).toHaveBeenCalledTimes(1);
  });

  it("leaves relationship counts unchanged when the edge is already gone", async () => {
    const { db, relationships } = makeStatefulDb();

    await handleUnfollow(db, UNFOLLOW);
    await handleUnfollow(db, UNFOLLOW);

    expect(db.deleteFollow).not.toHaveBeenCalled();
    expect(relationships.size).toBe(0);
  });

  it("does not touch unrelated relationships in the graph", async () => {
    const { db, relationships } = makeStatefulDb();

    await handleFollow(db, FOLLOW);
    await handleFollow(db, { follower: "GOTHER1", followee: "GOTHER2", ledger: 101 });
    expect(relationships.size).toBe(2);

    await handleUnfollow(db, UNFOLLOW);
    await handleUnfollow(db, UNFOLLOW);

    expect(relationships.size).toBe(1);
    expect(relationships.has("GOTHER1:GOTHER2")).toBe(true);
    expect(relationships.has("GABC123:GXYZ789")).toBe(false);
  });

  it("converges on the same count for an interleaved replay sequence", async () => {
    const { db, relationships } = makeStatefulDb();

    await handleFollow(db, FOLLOW);
    await handleUnfollow(db, UNFOLLOW);
    await handleFollow(db, FOLLOW);
    await handleUnfollow(db, UNFOLLOW);
    await handleUnfollow(db, UNFOLLOW);
    expect(relationships.size).toBe(0);

    await handleFollow(db, FOLLOW);
    expect(relationships.size).toBe(1);
  });

  it("rejects a malformed replayed unfollow without writing", async () => {
    const { db, relationships } = makeStatefulDb();

    await expect(
      handleUnfollow(db, { ...UNFOLLOW, follower: "" })
    ).rejects.toThrow("Unfollow event missing required field: follower");

    expect(db.deleteFollow).not.toHaveBeenCalled();
    expect(relationships.size).toBe(0);
  });
});

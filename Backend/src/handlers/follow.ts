import { Database } from "../db";
import { logger } from "../logger";

export interface FollowEvent {
  follower: string;
  followee: string;
  ledger: number;
}

export interface UnfollowEvent {
  follower: string;
  followee: string;
  ledger: number;
}


export interface TrendEvent {
  eventTime: string;
  events: string;
  proceeding: number;
}




/**
 * Handle a Follow event.
 *
 * Inserts a directed edge (follower → followee) into the follow graph.
 *
 * Replayed follow events must be treated as idempotent and must not create
 * duplicate relationships or inflate follower/following counts (issue #928).
 * If the relationship already exists, the handler returns immediately without
 * issuing a write. The database layer also guards against duplicates via the
 * UNIQUE (follower, followee) constraint, so even if a race occurs, no second
 * row is written and counts remain unchanged.
 */
export async function handleFollow(db: Database, event: FollowEvent): Promise<void> {
  if (!event.follower) {
    throw new Error("Follow event missing required field: follower");
  }
  if (!event.followee) {
    throw new Error("Follow event missing required field: followee");
  }

  const existing = await db.getFollow(event.follower, event.followee);
  if (existing) {
    logger.info(`Follow replay for ${event.follower}→${event.followee} is a no-op`);
    return;
  }

  await db.insertFollow({
    follower: event.follower,
    followee: event.followee,
    ledger: event.ledger,
  });
}
/**
 * Handle an Unfollow event.
 *
 * Removes the directed edge (follower → followee) from the follow graph.
 *
 * Repeated-unfollow semantics (issue #931): an Unfollow event states a desired
 * *end state* — "follower does not follow followee" — not a transition that may
 * only happen once. The chain may redeliver the same event after a crash, a
 * replay window or an at-least-once transport, so every delivery is judged
 * against the current graph:
 *
 *   - relationship present  → delete it exactly once (`DELETE ... WHERE` on the
 *     `(follower, followee)` key, so a racing duplicate affects zero rows).
 *   - relationship absent   → no-op that resolves without throwing; no write is
 *     issued and relationship counts are left untouched.
 *
 * Neither delivery order nor repetition changes the final state, so N replays
 * of the same Unfollow leave the same relationship count as the first one did.
 */
export async function handleUnfollow(db: Database, event: UnfollowEvent): Promise<void> {
  if (!event.follower) {
    throw new Error("Unfollow event missing required field: follower");
  }
  if (!event.followee) {
    throw new Error("Unfollow event missing required field: followee");
  }

  const existing = await db.getFollow(event.follower, event.followee);
  if (!existing) {
    // Replay of an already-applied Unfollow: harmless no-op (issue #931).
    logger.info(`Unfollow replay for ${event.follower}→${event.followee} is a no-op`);
    return;
  }

  await db.deleteFollow(event.follower, event.followee);
}
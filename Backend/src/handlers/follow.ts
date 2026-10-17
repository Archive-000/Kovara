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
 * Handle a Follow event.
 *
 * Inserts a directed edge (follower → followee) into the follow graph.
 * Idempotent: if the follow already exists the handler returns immediately
 * without issuing a database write.
 */
/**
 * Handle an Unfollow event.
 *
 * Removes the directed edge (follower → followee) from the follow graph.
 * Idempotent: if the follow doesn't exist the handler returns immediately
 * without issuing a database write.
 */
export async function handleUnfollow(db: Database, event: UnfollowEvent): Promise<void> {
  if (!event.follower) {
    throw new Error("Unfollow event missing required field: follower");
  }
  if (!event.followee) {
    throw new Error("Unfollow event missing required field: followee");
  }

  const existing = await db.getFollow(event.follower, event.followee);
  if (!existing) return;

  await db.deleteFollow(event.follower, event.followee);
}
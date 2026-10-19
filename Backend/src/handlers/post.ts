import { Pool } from "pg";
import { logger } from "../logger";

export interface PostCreatedEvent {
  id: bigint;
  author: string;
}

export interface PostDeletedEvent {
  post_id: bigint;
  author: string;
}
 // const applied = await getAppliedMigrations(pool);
/**
 * Handle a Follow event.
 *
 * Inserts a directed edge (follower → followee) into the follow graph.
 * Idempotent: if the follow already exists the handler returns immediately
 * without issuing a database write.
 */
export interface PostEventContext {
  txHash: string;
  ledgerSeq: number;
  timestamp: Date;
  content?: string;
}

/**
 * Validate a raw event object has the shape expected for a PostCreated event.
 * Throws a descriptive error for any missing or incorrectly typed field.
 */
export function validatePostCreatedEvent(event: unknown): asserts event is PostCreatedEvent {
  if (!event || typeof event !== "object") {
    throw new Error("PostCreated event must be a non-null object");
  }
  const e = event as Record<string, unknown>;
  if (typeof e.id !== "bigint" && typeof e.id !== "number" && typeof e.id !== "string") {
    throw new Error("PostCreated event missing or invalid field: id");
  }
  if (typeof e.author !== "string" || e.author.trim() === "") {
    throw new Error("PostCreated event missing or invalid field: author");
  }
}

/**
 * Handle PostCreatedEvent
 * Inserts a new post row into the posts table.
 *
 * Replay safety (BE-02) rests on **event identity**: a post's id is assigned by
 * the contract and is unique, so `(id)` is the identity of "this post was
 * created". Re-applying the same event therefore has to converge on the same
 * state as applying it once, which `ON CONFLICT (id) DO NOTHING` gives us
 * atomically — the uniqueness check and the insert are one statement, so a
 * redelivery racing the original cannot produce two rows.
 *
 * What `DO NOTHING` deliberately does *not* do is rewrite the row. A replay
 * must never reset `tip_total` or `like_count` back to the values carried by the
 * event (both `0` here): those counters have been moving forward with real tips
 * and likes since the post was created, and an at-least-once redelivery would
 * otherwise zero them. The conflict path is also silent about content, so a
 * late-arriving content fetch cannot overwrite what was already stored.
 */
export async function handlePostCreated(
  pool: Pool,
  event: PostCreatedEvent,
  context: PostEventContext
): Promise<void> {
  const { id, author } = event;
  const { timestamp, content } = context;
  const postContent = content || "";

  const query = `
    INSERT INTO posts (id, author, content, tip_total, like_count, created_at)
    VALUES ($1, $2, $3, $4, $5, $6)
    ON CONFLICT (id) DO NOTHING
    RETURNING id
  `;

  const values = [
    id.toString(),
    author,
    postContent,
    0,
    0,
    timestamp,
  ];

  try {
    const result = await pool.query(query, values);

    // `rowCount === 0` means the conflict path was taken: this exact post id was
    // already stored, so the delivery was a replay and the existing row — with
    // whatever engagement it has since accumulated — is left untouched.
    if (result.rowCount === 0) {
      logger.info(`Post ${id} already exists (idempotent skip)`);
    } else {
      logger.always(`Post ${id} created by ${author}`);
    }
  } catch (error) {
    logger.error(`Error handling PostCreatedEvent for post ${id}:`, error);
    throw error;
  }
}

/**
 * Handle PostDeletedEvent
 * Marks a post as deleted (soft delete) by setting deleted_at timestamp.
 * Uses the event timestamp (from ledger close time) rather than NOW() so
 * that the recorded deletion time is consistent with on-chain data, even
 * when the indexer replays old events.
 * Idempotent: Only updates if deleted_at is NULL.
 */
export async function handlePostDeleted(
  pool: Pool,
  event: PostDeletedEvent,
  context: PostEventContext
): Promise<void> {
  const { post_id, author } = event;
  // Use the event's ledger timestamp, falling back to now only if unavailable.
  // This ensures replayed events preserve the original deletion time.
  const deletedAt: Date = context.timestamp instanceof Date && !isNaN(context.timestamp.getTime())
    ? context.timestamp
    : new Date();

  const query = `
    UPDATE posts
    SET deleted_at = $1
    WHERE id = $2 AND author = $3 AND deleted_at IS NULL
  `;

  const values = [deletedAt, post_id.toString(), author];

  try {
    const result = await pool.query(query, values);

    if (result.rowCount === 0) {
      logger.info(`Post ${post_id} already deleted or not found (idempotent skip)`);
    } else {
      logger.always(`Post ${post_id} deleted by ${author}`);
    }
  } catch (error) {
    logger.error(`Error handling PostDeletedEvent for post ${post_id}:`, error);
    throw error;
  }
}

export async function fetchPostContent(_contractId: string, _postId: bigint): Promise<string> {
  return "";
}

export function createMockPostCreatedEvent(
  id: bigint = 1n,
  author: string = "GXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
): { event: PostCreatedEvent; context: PostEventContext } {
  return {
    event: { id, author },
    context: {
      txHash: "0x1234567890abcdef",
      ledgerSeq: 12345,
      timestamp: new Date(),
      content: "Test post content",
    },
  };
}

export function createMockPostDeletedEvent(
  post_id: bigint = 1n,
  author: string = "GXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
): { event: PostDeletedEvent; context: PostEventContext } {
  return {
    event: { post_id, author },
    context: {
      txHash: "0xabcdef1234567890",
      ledgerSeq: 12346,
      timestamp: new Date(),
    },
  };
}
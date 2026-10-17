import { Database } from "../db";
import { logger } from "../logger";

export interface ProfileSetEvent {
  user: string;
  username: string;
  creator_token: string;
  ledger: number;
}

export function validateProfileEvent(event: unknown): asserts event is ProfileSetEvent {
  if (!event || typeof event !== "object") {
    throw new Error("ProfileSet event must be a non-null object");
  }
  const e = event as Record<string, unknown>;
  if (typeof e.user !== "string" || e.user.trim() === "") {
    throw new Error("ProfileSet event missing or invalid field: user");
  }
  if (typeof e.username !== "string" || e.username.trim() === "") {
    throw new Error("ProfileSet event missing or invalid field: username");
  }
  if (e.creator_token !== undefined && typeof e.creator_token !== "string") {
    throw new Error("ProfileSet event field 'creator_token' must be a string");
  }
  if (typeof e.ledger !== "number" || !Number.isInteger(e.ledger)) {
    throw new Error("ProfileSet event missing or invalid field: ledger");
  }
}

/**
 * Handle a ProfileSet event emitted by the Kovara contract.
 *
 * Protects against out-of-order delivery (issue #927): stale events — ones
 * with an `updated_ledger` that is <= the stored profile's `updated_ledger` —
 * must be ignored so newer profile state is not overwritten by a replayed
 * older event. If no profile exists, apply the first event unconditionally.
 */
export async function handleProfileSet(db: Database, event: ProfileSetEvent): Promise<void> {
  if (!event.user) {
    throw new Error("ProfileSet event missing required field: user");
  }
  if (!event.username) {
    throw new Error("ProfileSet event missing required field: username");
  }

  const trimmedUsername = event.username.trim();
  if (trimmedUsername === "") {
    throw new Error("ProfileSet event username must be non-empty after trimming");
  }

  const existing = await db.getProfile(event.user);
  if (existing && event.ledger <= existing.updated_ledger) {
    logger.info(
      `Skipping stale ProfileSet for ${event.user} (event ledger ${event.ledger}, stored ledger ${existing.updated_ledger})`
    );
    return;
  }

  await db.upsertProfile({
    address: event.user,
    username: trimmedUsername,
    creator_token: event.creator_token ?? "",
    updated_ledger: event.ledger,
  });
}

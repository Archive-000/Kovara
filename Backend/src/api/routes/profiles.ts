import { Router, Request, Response } from "express";
import { Database, Profile } from "../../db";
import { ApiErrorResponse, ProfileResponse } from "../contracts";
import {
  isStellarAddress,
  parseStellarAddress,
  sendValidationError,
} from "../validation";

/**
 * Validates that the given string is a well-formed Stellar public key.
 *
 * Kept as a named export for existing callers, but delegates to the shared
 * validator (issue #665) so this endpoint cannot drift from the rest of the
 * API: Stellar strkeys use the base-32 alphabet `A–Z`, `2–7`, not `[A-Z0-9]`.
 */
export function isValidStellarAddress(addr: string): boolean {
  return isStellarAddress(addr);
}

/**
 * Serialize a stored profile to its API shape (BE-09).
 *
 * The response always carries **every** documented key. A row that has never
 * had a `creator_token`, or that predates a column being added, comes back from
 * Postgres with those properties absent or `undefined`; passing that straight
 * through `res.json` drops the keys entirely, so the same account serializes
 * differently depending on which columns happen to be populated.
 *
 * Clients then have to treat a missing key and an explicit `null` as two
 * different things — `profile.creator_token ?? fallback` works, but
 * `'creator_token' in profile` and any generated OpenAPI type disagree with the
 * documented shape. Normalizing absent/undefined to `null` here makes the
 * contract stable: the keys are always present, and "not set" is always `null`.
 */
function serializeProfile(profile: Profile): ProfileResponse {
  return {
    address: profile.address,
    username: profile.username ?? "",
    creator_token: profile.creator_token ?? "",
    updated_ledger: profile.updated_ledger ?? 0,
  };
}

export function createProfilesRouter(db: Database): Router {
  const router = Router();

  /**
   * GET /profiles/:address
   * Returns the profile for the given Stellar address.
   */
  router.get(
    "/:address",
    async (req: Request, res: Response<ProfileResponse | ApiErrorResponse>): Promise<void> => {
      const parsed = parseStellarAddress(req.params.address);
      if (!parsed.ok) {
        sendValidationError(res, parsed.issue);
        return;
      }

      const profile = await db.getProfile(parsed.value);
      if (!profile) {
        res.status(404).json({ error: "Profile not found", code: "NOT_FOUND" });
        return;
      }

      res.json(serializeProfile(profile));
    }
  );

  return router;
}

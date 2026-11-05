import { describe, expect, it } from "vitest";
import { loadDraft, saveDraft } from "./storage";
import type { ReviewDraft } from "./contracts";

describe("browser draft storage", () => {
  it("restores a pending edit after reopening the database", async () => {
    const draft: ReviewDraft = {
      key: "user-1:video-1",
      userId: "user-1",
      videoId: "video-1",
      clientId: "client-1",
      intervals: [{ id: "interval-1", start_ms: 1000, end_ms: 2400 }],
      pendingStartMs: null,
      status: "in_progress",
      baseRevision: 2,
      pendingMutationId: "mutation-1",
      positionMs: 1800,
      zoom: 2,
      updatedAtMs: 1234,
    };

    await saveDraft(draft);

    await expect(loadDraft(draft.key)).resolves.toEqual(draft);
  });
});

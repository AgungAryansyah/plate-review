import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import App from "./App";

const account = { id: "user-1", username: "reviewer", role: "reviewer" as const };
const exportJob = {
  id: "job-1", status: "complete", phase: "complete", total_items: 1, completed_items: 1,
  failed_items: 0, progress: 1, error: null, created_at_ms: 0, started_at_ms: 0, ended_at_ms: 1,
  items: [{
    id: "item-1", video_id: "video-1", status: "complete", progress: 1, error: null,
    outputs: [{
      file_name: "camera-91_abc_clip_0-1000.mp4", interval_ids: ["interval-1"],
      requested_start_ms: 100, requested_end_ms: 900, padded_start_ms: 0, padded_end_ms: 1000,
      actual_start_ms: 0, actual_end_ms: 1000, duration_ms: 1000, bytes: 2048,
    }],
  }],
};

function response(status: number, body: unknown) {
  return { ok: status >= 200 && status < 300, status, json: async () => body } as Response;
}

describe("sign-in and keyboard help", () => {
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("opens the video library and shortcut guide after sign-in", async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const path = String(input);
      if (path === "/api/auth/me") return response(401, { error: "Sign in to continue" });
      if (path === "/api/auth/login") return response(200, { user: account });
      if (path.startsWith("/api/videos?")) return response(200, []);
      if (path === "/api/exports") return response(200, [exportJob]);
      return response(404, { error: "Not found" });
    });
    vi.stubGlobal("fetch", fetchMock);
    const user = userEvent.setup();
    render(<App />);

    await user.type(screen.getByLabelText("Username"), "reviewer");
    await user.type(screen.getByLabelText("Password"), "correct horse battery");
    await user.click(screen.getByRole("button", { name: "Sign in" }));

    expect(await screen.findByRole("heading", { name: "Recordings" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Export jobs/ }));
    expect(await screen.findByRole("link", { name: /camera-91_abc_clip_0-1000\.mp4/ })).toHaveAttribute("href", "/api/exports/job-1/files/camera-91_abc_clip_0-1000.mp4");
    expect(screen.getByRole("link", { name: "Download manifest" })).toHaveAttribute("href", "/api/exports/job-1/manifest");
    fireEvent.keyDown(window, { key: "?" });
    expect(await screen.findByRole("dialog", { name: "Review without reaching for the mouse." })).toBeInTheDocument();
    await waitFor(() => expect(fetchMock).toHaveBeenCalledWith("/api/exports", expect.any(Object)));
  });

  it("supports interval navigation, deletion, and escape shortcuts in clip review", async () => {
    const video = {
      id: "video-1",
      source_path: "session-1/chunk.mp4",
      camera_id: "91",
      camera_name: "Gate_Cam",
      session_id: "session-1",
      session_started_at: "2026-10-09T10:00:00+07:00",
      session_kind: "recording",
      source_status: "available" as const,
      duration_ms: 10_000,
      width: 1920,
      height: 1080,
      codec: "h264",
      frame_rate: "25/1",
      interruption_count: 0,
      review: {
        revision: 1,
        status: "in_progress" as const,
        intervals: [
          { id: "interval-1", start_ms: 1000, end_ms: 2000 },
          { id: "interval-2", start_ms: 4000, end_ms: 5000 },
        ],
        updated_by: "reviewer",
        updated_at_ms: 1000,
      },
      claim: null,
    };
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input);
      if (path === "/api/auth/me") return response(200, { user: account });
      if (path.startsWith("/api/videos?")) return response(200, [video]);
      if (path === "/api/videos/video-1") return response(200, { video, position_ms: 0, zoom: 1 });
      if (path === "/api/videos/video-1/claim" && init?.method === "POST") return response(200, video);
      if (path === "/api/videos/video-1/claim" && init?.method === "DELETE") return { ok: true, status: 204, json: async () => ({}) } as Response;
      if (path === "/api/videos/video-1/review" && init?.method === "PUT") {
        const body = JSON.parse(String(init.body));
        return response(200, {
          revision: 2,
          status: body.status,
          intervals: body.intervals,
          updated_by: "reviewer",
          updated_at_ms: 2000,
        });
      }
      if (path === "/api/exports") return response(200, []);
      return response(404, { error: "Not found" });
    });
    vi.stubGlobal("fetch", fetchMock);
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: /91 · Gate Cam/ }));
    const timeline = await screen.findByRole("slider", { name: "Video timeline" });
    await waitFor(() => expect(screen.getByRole("button", { name: /Mark reviewed/ })).toBeEnabled());
    expect(timeline).toHaveAttribute("aria-valuenow", "0");

    fireEvent.keyDown(window, { key: "]" });
    expect(timeline).toHaveAttribute("aria-valuenow", "1000");

    fireEvent.keyDown(window, { key: "]" });
    expect(timeline).toHaveAttribute("aria-valuenow", "4000");

    fireEvent.keyDown(window, { key: "[" });
    expect(timeline).toHaveAttribute("aria-valuenow", "1000");

    expect(screen.getByRole("heading", { name: /Intervals/ })).toHaveTextContent("Intervals 2");
    fireEvent.keyDown(window, { key: "Delete" });
    await waitFor(() => expect(screen.getByRole("heading", { name: /Intervals/ })).toHaveTextContent("Intervals 1"));

    fireEvent.keyDown(window, { key: "?" });
    expect(await screen.findByRole("dialog", { name: "Review without reaching for the mouse." })).toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("slider", { name: "Video timeline" })).toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape" });
    expect(await screen.findByText("Review at your own pace.")).toBeInTheDocument();
  });
});

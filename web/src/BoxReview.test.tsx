import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import BoxReview from "./BoxReview";

const clip = { id: "clip-1", source_video_id: "video-1", export_job_id: "export-1", name: "drive-a-clip.mp4", bytes: 1500, duration_ms: 2100, status: "complete" as const, progress: 1, error: null };
const detail = { clip, meta: { width: 1280, height: 720, fps: 25, frame_count: 53 }, tracks: [{ id: "box-01", selected: false, plate_text: "", detections: [0, 40, 80].map((timestamp_ms, frame) => ({ frame, timestamp_ms, x_min: 100, y_min: 200, x_max: 220, y_max: 240, confidence: 0.9 })) }], settings: { min_confidence: 0.45, jitter_px: 3, smoothing_radius: 2, minimum_frames: 2, max_area_fraction: 0.08, max_size_ratio: 3 }, revision: 7, job: null };
function response(body: unknown) { return { ok: true, status: 200, json: async () => body } as Response; }

describe("Plate boxes", () => {
  afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

  it("starts with clip selection and does not load video or segment until requested", async () => {
    const initialDetail = { ...detail, clip: { ...clip, status: "unsegmented" } };
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input);
      if (path === "/api/boxes/clips") return response([clip]);
      if (path === "/api/boxes/clips/clip-1") return response(initialDetail);
      if (path.endsWith("/segment")) return response({ status: "queued", progress: 0, phase: "queued", logs: [], error: null });
      if (path.endsWith("/review") && init?.method === "PUT") return response(initialDetail);
      return response({});
    });
    vi.stubGlobal("fetch", fetchMock);
    const user = userEvent.setup();
    render(<BoxReview onBack={() => undefined} />);

    expect(await screen.findByRole("heading", { name: "Choose a trimmed clip" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: /drive-a-clip\.mp4/ })).toBeInTheDocument();
    expect(fetchMock).not.toHaveBeenCalledWith("/api/boxes/clips/clip-1/stream", expect.anything());
    expect(fetchMock).not.toHaveBeenCalledWith("/api/boxes/clips/clip-1/segment", expect.anything());

    await user.click(screen.getByRole("button", { name: /drive-a-clip\.mp4/ }));
    expect(await screen.findByRole("heading", { name: /Plate candidates/ })).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "Include box box-01" })).not.toBeChecked();
    expect(screen.getByRole("textbox", { name: "Plate text for box box-01" })).toBeInTheDocument();
    expect(screen.getByRole("slider", { name: /Maximum area fraction/ })).toHaveValue("0.08");
    expect(screen.getByRole("slider", { name: /Maximum size ratio/ })).toHaveValue("3");
    expect(screen.getByRole("img", { name: "Detected plate boxes" }).querySelectorAll("g")).toHaveLength(1);
    // A large seek into a gap must not leave the last nearby track box stuck on screen.
    fireEvent.change(screen.getByRole("slider", { name: "Clip timeline" }), { target: { value: "300" } });
    expect(screen.getByRole("img", { name: "Detected plate boxes" }).querySelectorAll("g")).toHaveLength(0);
    expect(fetchMock).not.toHaveBeenCalledWith("/api/boxes/clips/clip-1/segment", expect.anything());

    await user.click(screen.getByRole("button", { name: "Segment plates" }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledWith("/api/boxes/clips/clip-1/segment", expect.objectContaining({ method: "POST", body: "{}" })));
  });

  it("keeps segmentation logs visible after the job completes and disables edits while it runs", async () => {
    const activeDetail = { ...detail, clip: { ...clip, status: "running" as const, progress: 0.5 }, job: { status: "running" as const, progress: 0.5, phase: "Tracking", logs: ["Tracking frames"], error: null } };
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => String(input) === "/api/boxes/clips" ? response([activeDetail.clip]) : response(activeDetail));
    vi.stubGlobal("fetch", fetchMock);
    const user = userEvent.setup();
    render(<BoxReview onBack={() => undefined} />);
    await user.click(await screen.findByRole("button", { name: /drive-a-clip\.mp4/ }));
    expect(await screen.findByText("Tracking frames")).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "Plate text for box box-01" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Segmentation running" })).toBeDisabled();

    const completedDetail = { ...detail, job: { status: "complete" as const, progress: 1, phase: "Complete", logs: ["Export ready"], error: null } };
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => String(input) === "/api/boxes/clips" ? response([clip]) : response(completedDetail));
    await user.click(screen.getByRole("button", { name: /^←$/ }));
    await user.click(await screen.findByRole("button", { name: /drive-a-clip\.mp4/ }));
    expect(await screen.findByText("Export ready")).toBeInTheDocument();
  });
});

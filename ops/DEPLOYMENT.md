# Plate boxes deployment

The existing Plate Review service remains on port 8080 with its existing accounts, source mount, exports mount, and database volume. The new **Plate boxes** view lists all completed exported clips before opening candidates. Clip discovery never loads a GPU model; **Segment plates** creates a durable job on explicit request.

## SAM bridge

The web container uses a group-writable spool at `/data/boxes/sam-jobs`, backed by `/mnt/data/plate-box-review/sam-jobs` on labkc2-amd. `plate-sam-bridge.service` runs as the existing `faris` host user. It transfers source clips and checksummed manifests to `pityudhistira28@gpu-labkc.ub.ac.id:1004`, below `~/faris/plate-review-worker`, through SSH. Existing host SSH access remains outside the web container.

Each remote inference process loads the existing SAM 3.1 checkpoint, processes every original frame, derives boxes from masks, records original frame timestamps, releases the model, and exits. No OCR runs. The process has a 30 decimal GB GPU guard, four GB of free VRAM headroom, a host RAM startup estimate based on the clip length, and two GB of host RAM headroom. Logs and progress are persisted for the UI. Remote PID records let the bridge reconnect without starting duplicate inference. A `PAUSED` file in either the host spool or the remote worker directory prevents new work.

Host service command:

```sh
python3 /home/faris/plate-review/ops/sam/bridge.py \
  --queue /mnt/data/plate-box-review/sam-jobs \
  --exports /mnt/data/plate-number-trimmed/review
```

The bridge expects the previously installed remote environment at `~/faris/plate-sam-camera92/venv/bin/python` and checkpoint at `~/faris/plate-sam-camera92/models/sam3.1_multiplex.pt`. These paths are not included in the Docker image.

## Export format

One **Export dataset** action saves the review and downloads a ZIP containing `annotations.csv`, `<video_id>.mp4`, and a provenance manifest. The MP4 is copied byte for byte from the raw trimmed clip. `video_id` is the stable clip UUID; the manifest maps it to the original source recording and export filename. The CSV columns are exactly:

```
video_id,box_id,timestamp_ms,x_min,y_min,x_max,y_max,plate_text
```

A box ID has one manually entered plate text across all its rows. Only checked tracks with nonempty text and detections surviving the configured filters are exported. Boxes remain in original-video pixel coordinates. PNG source-frame preview is available when browser video decoding is blank or unsupported; it does not alter the exported video.

## CI and CD

`.github/workflows/ci.yml` runs Rust formatting/tests, frontend tests/build, Python syntax checks, and a deployment container build for main pushes and pull requests. `plate-review-deploy.timer` on the host invokes `ops/deploy.sh` every two minutes. It waits for successful main-branch CI, builds the version, preserves the previous image, backs up the app state while this service is stopped, deploys, and health-checks before recording the revision. Failed health checks restore the previous image. Existing database/users and unrelated containers are preserved.

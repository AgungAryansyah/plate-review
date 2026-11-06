# Plate Review

Plate Review is a private web app for marking video intervals before a later SAM segmentation step. It reads verified MP4 chunks from the recorder metadata, saves review revisions in SQLite, and exports selected ranges as individual H.264 stream-copy files.

The Rust server provides sign-in, edit claims, video streaming, review storage, and a resumable export queue. The TypeScript app provides the video timeline, zoom, interval controls, and keyboard workflow. Each browser also stores an unsent draft in IndexedDB so a reload or short connection loss does not discard edits.

## Run on the video server with Docker

Install Docker Engine and the Docker Compose plugin. Copy `.env.example` to `.env` and set the host paths to the recorder source directory and a separate export directory. The app mounts the source read-only.

The export directory must exist and be writable by UID `10001`, which the container uses. The database is stored in a Docker volume.

```sh
mkdir -p /mnt/data/plate-number-trimmed/review
sudo chown 10001:10001 /mnt/data/plate-number-trimmed/review
docker compose build
docker compose run --rm plate-review create-user admin --role admin
docker compose up -d
```

The create-user command prompts for a password. Add reviewer accounts with `docker compose run --rm plate-review create-user NAME`. Reset or disable an account with `reset-password NAME` or `disable-user NAME`.

Open `http://127.0.0.1:8080` on the server. From another computer, connect with an SSH tunnel such as `ssh -L 8080:127.0.0.1:8080 SERVER` and open that address locally. For later public access, place an HTTPS reverse proxy on the same host. Keep the app port bound to localhost by default. Set `PLATE_REVIEW_APP_ORIGIN` to the exact public origin, set `PLATE_REVIEW_SECURE_COOKIE=true`, and set `PLATE_REVIEW_TRUST_PROXY_HEADERS=true` only when a trusted reverse proxy sends `X-Forwarded-For`.

For a private Tailscale HTTP experiment, set `PLATE_REVIEW_HOST_BIND` to the server's Tailscale IPv4 address and set `PLATE_REVIEW_APP_ORIGIN` to the matching `http://IP:8080` origin. Set `PLATE_REVIEW_ALLOW_TAILSCALE_HTTP=true` and `PLATE_REVIEW_SECURE_COOKIE=false`. This exception accepts only IPv4 addresses in Tailscale's `100.64.0.0/10` range. HTTP session cookies do not use the Secure flag, so use this only on a private tailnet. Bind the port to the Tailscale address only.

## Run for local development

Install Rust, Node.js, FFmpeg, and FFprobe. Set the server variables before creating the first account:

```sh
export PLATE_REVIEW_SOURCE_DIR=/path/to/recorder-data
export PLATE_REVIEW_EXPORT_DIR=/path/to/clip-exports
export PLATE_REVIEW_STATE_DIR=./var/state
export PLATE_REVIEW_APP_ORIGIN=http://127.0.0.1:5173
export PLATE_REVIEW_SECURE_COOKIE=false
cargo run -- create-user admin --role admin
cargo run -- serve
```

In another terminal, run the web app:

```sh
cd web
npm ci
npm run dev
```

Open `http://127.0.0.1:5173`. Vite proxies API and video requests to the Rust server on port 8080.

## Review workflow

- Select a recording from the searchable camera and status filters.
- Press `Space` or `K` to play or pause. Use `I` and `O` to mark a range.
- Use the mouse wheel or `+` and `−` to zoom. Drag the image while zoomed.
- Press `R` to mark the recording reviewed and `N` to open the next available recording.
- Select reviewed recordings for export. The worker scans keyframes, adds 500 ms of padding, merges overlapping ranges, and writes each output clip separately.
- Download finished clips and the JSON manifest from the export panel.

Press `?` in the app to view all keyboard shortcuts. Exports preserve the original video stream and may begin or end on a nearby keyframe. The manifest records the requested, padded, and actual output boundaries.

The source library is scanned at server startup. Admins can rescan it in the app. If a source file changes, an admin must accept the updated file before it can be reviewed or exported.

## Checks

```sh
cargo test
cd web
npm test
npm run build
npx playwright install chromium
npm run e2e
```

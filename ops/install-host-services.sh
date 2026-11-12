#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
set -a
source .env
set +a
plate_data=${PLATE_REVIEW_BOX_HOST:-$HOME/plate-review-data}
mkdir -p "$HOME/.config/systemd/user"
cat >"$HOME/.config/systemd/user/plate-sam-bridge.service" <<UNIT
[Unit]
Description=Plate Review L40 SAM queue bridge
After=network-online.target
[Service]
WorkingDirectory=$HOME/plate-review
ExecStart=/usr/bin/python3 $HOME/plate-review/ops/sam/bridge.py --queue $plate_data/sam-jobs --exports /mnt/data/plate-number-trimmed/review
Restart=on-failure
RestartSec=10
UMask=0002
[Install]
WantedBy=default.target
UNIT
cat >"$HOME/.config/systemd/user/plate-review-deploy.service" <<UNIT
[Unit]
Description=Deploy Plate Review main after successful CI
After=network-online.target
[Service]
Type=oneshot
WorkingDirectory=$HOME/plate-review
ExecStart=/usr/bin/sg docker -c "$HOME/plate-review/ops/deploy.sh"
TimeoutStartSec=1800
SuccessExitStatus=75
UNIT
cat >"$HOME/.config/systemd/user/plate-review-deploy.timer" <<'UNIT'
[Unit]
Description=Check Plate Review main for validated releases
[Timer]
OnBootSec=2min
OnUnitInactiveSec=2min
Persistent=true
[Install]
WantedBy=timers.target
UNIT
systemctl --user daemon-reload
systemctl --user enable --now plate-sam-bridge.service
systemctl --user enable plate-review-deploy.timer

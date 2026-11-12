#!/usr/bin/env bash
# Run from faris's host checkout. Successful main-branch CI is the deployment gate.
set -euo pipefail
cd "$(dirname "$0")/.."
exec 9>/mnt/data/plate-box-review/deploy.lock
flock -n 9 || exit 0
git fetch --quiet origin main
revision=$(git rev-parse origin/main)
if [[ -f /mnt/data/plate-box-review/deployed-commit ]] && [[ $(cat /mnt/data/plate-box-review/deployed-commit) == "$revision" ]]; then
  exit 0
fi
python3 - "$revision" <<'PY'
import json, sys, urllib.request
sha=sys.argv[1]
url=f'https://api.github.com/repos/AgungAryansyah/plate-review/actions/runs?head_sha={sha}&event=push&per_page=20'
request=urllib.request.Request(url,headers={'User-Agent':'plate-review-deploy','Accept':'application/vnd.github+json'})
with urllib.request.urlopen(request,timeout=30) as response:
    runs=json.load(response)['workflow_runs']
ci=[r for r in runs if r['name']=='CI' and r['head_sha']==sha]
if not ci or ci[0]['conclusion']!='success':
    print('Main CI is not successful yet; retaining the deployed version.')
    sys.exit(75)
PY
# Refuse to discard any tracked edits in the deployment checkout.
[[ -z $(git status --porcelain --untracked-files=no) ]] || { echo 'Deployment checkout has tracked changes'; exit 1; }
git checkout --quiet --detach "$revision"
docker compose --project-name plate-review --env-file .env build
old_image=$(docker inspect plate-review-plate-review-1 --format '{{.Image}}')
docker tag "$old_image" plate-review-rollback:latest
backup=/mnt/data/plate-box-review/backups/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$backup"
# Capture the SQLite database and WAL together while only this app is stopped.
docker stop --time 30 plate-review-plate-review-1 >/dev/null
docker cp plate-review-plate-review-1:/app/state "$backup/state"
docker compose --project-name plate-review --env-file .env up -d --no-build
healthy=false
for attempt in $(seq 1 30); do
  if curl -fsS --max-time 3 http://100.98.169.42:8080/api/health >/dev/null; then healthy=true; break; fi
  sleep 2
done
if [[ "$healthy" != true ]]; then
  docker logs --tail 100 plate-review-plate-review-1
  docker tag plate-review-rollback:latest plate-review-plate-review:latest
  docker compose --project-name plate-review --env-file .env up -d --no-build --force-recreate
  echo 'New version failed health check; restored previous image.'
  exit 1
fi
printf '%s\n' "$revision" >/mnt/data/plate-box-review/deployed-commit
systemctl --user try-restart plate-sam-bridge.service
echo "Deployed $revision"

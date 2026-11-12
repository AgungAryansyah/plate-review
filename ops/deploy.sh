#!/usr/bin/env bash
# Run from faris's host checkout. Successful main-branch CI is the deployment gate.
set -euo pipefail
cd "$(dirname "$0")/.."
set -a
source .env
set +a
plate_data=${PLATE_REVIEW_BOX_HOST:-$HOME/plate-review-data}
exec 9>"$plate_data/deploy.lock"
flock -n 9 || exit 0
git fetch --quiet origin main
revision=$(git rev-parse origin/main)
if [[ -f "$plate_data/deployed-commit" ]] && [[ $(cat "$plate_data/deployed-commit") == "$revision" ]]; then
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
if [[ $(git rev-parse HEAD) != "$revision" ]]; then
  git checkout --quiet --detach "$revision"
  exec "$PWD/ops/deploy.sh"
fi
old_image=$(docker inspect plate-review-plate-review-1 --format '{{.Image}}')
if docker image inspect "$old_image" >/dev/null 2>&1; then
  docker tag "$old_image" plate-review-rollback:latest
else
  # A running container can outlive missing Docker image-store blobs.
  runtime_backup="$plate_data/backups/runtime-$(date -u +%Y%m%dT%H%M%SZ).tar.gz"
  docker export plate-review-plate-review-1 | gzip -1 >"$runtime_backup"
  docker import --change 'ENTRYPOINT ["plate-review"]' --change 'CMD ["serve"]' --change 'USER 10001' --change 'WORKDIR /app' "$runtime_backup" plate-review-rollback:latest >/dev/null
fi
# Retain the rollback image before the build moves the latest tag.
docker compose --project-name plate-review --env-file .env build
app_stopped=false
restore_previous() {
  if [[ "$app_stopped" == true ]]; then
    docker tag plate-review-rollback:latest plate-review-plate-review:latest
    docker compose --project-name plate-review --env-file .env up -d --no-build --force-recreate
  fi
}
trap 'result=$?; echo "Deployment failed at line $LINENO"; restore_previous; exit "$result"' ERR
backup=$plate_data/backups/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$backup"
# Capture the SQLite database and WAL together while only this app is stopped.
docker stop --time 30 plate-review-plate-review-1 >/dev/null
app_stopped=true
docker cp plate-review-plate-review-1:/app/state "$backup/state"
docker compose --project-name plate-review --env-file .env up -d --no-build
healthy=false
for attempt in $(seq 1 30); do
  if curl -fsS --max-time 3 http://100.98.169.42:8080/api/health >/dev/null; then healthy=true; break; fi
  sleep 2
done
if [[ "$healthy" != true ]]; then
  docker logs --tail 100 plate-review-plate-review-1
  echo 'New version failed health check; restoring previous image.'
  false
fi
app_stopped=false
printf '%s\n' "$revision" >"$plate_data/deployed-commit"
systemctl --user try-restart plate-sam-bridge.service
echo "Deployed $revision"

# Repository Notes

- The current local Docker deploy for this repo is running on port `18081`.
- Use `docker compose -f compose.dev.yaml up -d --build` for local Docker here. Do not recreate a local `docker-compose.yml`; that stale file spins up an unnecessary `nginx` container alongside `redlib`.
- Local development deploy:
  Run `docker compose -f compose.dev.yaml up -d --build`.
  This binds Redlib to `127.0.0.1:18081`.
- Raspberry Pi deploy:
  Run `./deploy.sh` from the repo root.
  That script builds a `linux/arm64` image locally, pushes it to the Pi registry at `100.119.233.126:5010`, copies `compose.pi.yaml` to `~/redlib` on the Pi, and recreates the `redlib` container there.
  The Pi deploy serves Redlib on port `8081`.
  `./deploy.sh --deploy-only` skips the local build and push.
  `./deploy.sh --follow-logs` tails the remote Redlib logs after deployment.
- Pi runtime details:
  The active Pi deploy directory is `~/redlib`.
  The Pi compose file is `compose.pi.yaml`, not `compose.yaml`.
  The Pi container pulls `127.0.0.1:5010/redlib:latest` by default.
- Cloudflare / tunnel routing:
  Do not treat the local `cloudflared` YAML on the Pi as the source of truth for Redlib hostnames.
  The active public hostname routing is managed in the Cloudflare dashboard.
  If a Redlib domain fails externally but `http://127.0.0.1:8081/settings` works on the Pi, inspect the Cloudflare dashboard/tunnel config before changing any local YAML.

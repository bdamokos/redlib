# Repository Notes

- The current local Docker deploy for this repo is running on port `18081`.
- Use `docker compose -f compose.dev.yaml up -d --build` for local Docker here. Do not recreate a local `docker-compose.yml`; that stale file spins up an unnecessary `nginx` container alongside `redlib`.
- Local development deploy:
  Run `docker compose -f compose.dev.yaml up -d --build`.
  This binds Redlib to `127.0.0.1:18081`.
- Default change workflow:
  When the user asks to fix or change something in this repo, assume they expect the change to be committed and deployed after validation unless they explicitly say otherwise.
  If there are parallel active threads or workstreams that could also affect deployment, spawn or defer to an orchestrator thread that can coordinate those parallel changes and manage the final deploy ordering.
- Persistent deployment thread:
  The designated Redlib deployment coordinator thread is `019f0411-87a6-7112-a4a8-a005a28d3ee6`.
  Defer Raspberry Pi deployment requests to that thread instead of running `./deploy.sh` directly from feature or fix threads, unless the user explicitly tells the current thread to bypass the coordinator.
  Deployment requests should include the originating thread id, branch, commit SHA, validation summary, and whether the request is urgent.
  The deployment coordinator queues requests, batches compatible requests when practical, and runs `./deploy.sh` from the repo root once the deployable work is committed.
  The coordinator should periodically push committed Redlib work to the Forgejo remote `private`, especially after accepting or completing a deployment batch, so the private remote does not lag behind local deployment state.
  Once the coordinator acknowledges a valid queued deployment request, the originating thread may close unless the coordinator sends it back for a commit, clarification, or fix.
  If requested work is still uncommitted or the deployable commit is unclear, the coordinator sends the request back to the originating thread asking it to commit or clarify, and does not deploy.
  If deployment fails because of an implementation or build error, the coordinator reports the failure back to the originating thread with the relevant command output so that thread can fix it without staying in a polling loop.
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

- `settings.txt`:
  This file is not a line-oriented config file; it is an **import string payload** (may be non-UTF8/binary-looking when opened).

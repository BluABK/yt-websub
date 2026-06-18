# yt-websub

A tiny, headless **YouTube WebSub (PubSubHubbub) notification server** for use with
[streamarchiver](../streamarchiver). It runs on a public Debian VPS, subscribes to YouTube's
push-notification hub for a set of channels, durably records each notification, and exposes them over
a token-authenticated HTTPS API that streamarchiver (which is **not** reachable from the internet)
**polls** from home.

A WebSub notification is **not** an authoritative "is live" signal — YouTube fires it for uploads,
go-lives, *and* metadata edits, and it's neither perfectly reliable nor perfectly timely. So
streamarchiver treats each event as a **"check this channel now" trigger** that feeds its existing
liveness detection, and keeps a slow fallback poll as a safety net. This server's only jobs are:
subscribe, verify, receive, persist, and serve.

## Design at a glance

```
            internet (public)               HTTPS (Let's Encrypt, public CA)
 YouTube hub ───────────────► [ yt-websub ] ◄─────────────── streamarchiver (home, polls)
 (pubsubhubbub.appspot)        one HTTPS listener :443
                                 /yt/cb/<token>  callback (HMAC-SHA1 verified, per-sub secret)
   ▲ subscribe/renew             /api/*          control + poll (Bearer token)
   └── ureq/rustls (outbound) ─┘  append-only event log + subs state (fsync'd, monotonic seq)
                                  background thread renews leases before expiry
```

- **No async runtime.** A small blocking accept pool (`tiny_http`) plus one renewal/compaction
  timer thread. Idle CPU ~0%, RSS a few MB, ~2 MB binary.
- **Few dependencies:** `tiny_http` (HTTP/1.1 + rustls TLS), `ureq` (outbound HTTPS), `hmac` + `sha1`
  (signature verification), `getrandom` (per-sub secrets). No tokio, no serde, no SQLite.
- **Durable & crash-safe:** every event is `fsync`'d before the hub gets its 2xx; the subscription
  state file is rewritten atomically (tmp + rename); a torn final log line is skipped on load.
- **Per-subscription callback path** (`/yt/cb/<token>`) identifies which subscription — and thus
  which secret — a notification belongs to, so the body is never trusted to pick the verification key.

## Build

Requires a Rust toolchain (stable). Build the release binary:

```sh
cargo build --release
# -> target/release/yt-websub
cargo test            # 20 unit tests (config, HMAC vectors, atom, store durability, ...)
```

Build on the VPS, or cross-compile and copy the binary to `/usr/local/bin/yt-websub`.

## Deploy on Debian

### 1. DNS + TLS (Let's Encrypt)

Point a record (e.g. `hooks.example.com`) at the VPS, then issue a cert:

```sh
apt-get install -y certbot
certbot certonly --standalone -d hooks.example.com
# HTTP-01 needs port 80 reachable during issuance/renewal.
# Prefer DNS-01 (certbot --dns-<provider>) if you'd rather not open 80.
```

Make the live cert reload automatically on renewal:

```sh
# /etc/letsencrypt/renewal-hooks/deploy/restart-yt-websub.sh  (chmod +x)
systemctl restart yt-websub
```

(Restart is cheap; subscriptions reload from disk, so no events are lost.)

### 2. Service user, config, state

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin yt-websub
install -d -o yt-websub -g yt-websub -m 0750 /var/lib/yt-websub

cp deploy/yt-websub.env.example /etc/yt-websub.env
chown root:yt-websub /etc/yt-websub.env && chmod 0640 /etc/yt-websub.env
# Edit /etc/yt-websub.env: set YTWEBSUB_CALLBACK_BASE, the TLS paths, and a
# strong YTWEBSUB_BEARER_TOKEN (openssl rand -hex 32).

cp deploy/channels.txt.example /var/lib/yt-websub/channels.txt
chown yt-websub:yt-websub /var/lib/yt-websub/channels.txt
# Add your channel ids / @handles / URLs.
```

Ensure the service user can read the cert (LE keys are `root:root 0600` by default). Simplest:
grant the private-key dirs to a group the service user is in, or have the deploy-hook copy
`fullchain.pem`/`privkey.pem` into `/var/lib/yt-websub/` readable by `yt-websub` and point
`YTWEBSUB_TLS_*` there.

### 3. systemd

```sh
cp deploy/yt-websub.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now yt-websub
journalctl -u yt-websub -f
```

### 4. Firewall

```sh
ufw allow 443/tcp      # callback + /api
ufw allow 80/tcp       # only if using certbot HTTP-01 renewal
# Optional: restrict :443 source to your home IP for extra /api safety.
```

## Managing channels

Edit `channels.txt` (one `UC...`, `@handle`, or channel URL per line). The server reconciles within
~60 s of an mtime change and on startup: it subscribes new channels, unsubscribes removed ones, and
renews the rest. Handles/URLs are resolved to `UC...` ids and cached in `resolve.cache`.

Alternatively drive it from the API (this is how streamarchiver will manage it):
`POST /api/channels` replaces the active entries in `channels.txt` and reconciles
immediately. Operator comment/blank lines in the file are preserved; only the
channel entries are rewritten.

## HTTP API (consumed by streamarchiver)

All `/api/*` routes require `Authorization: Bearer <YTWEBSUB_BEARER_TOKEN>`.

| Method | Path | Body / Query | Response |
|---|---|---|---|
| GET | `/api/health` | – | `{"ok":true,"subs_active":k,"max_seq":N,"now":t}` |
| GET | `/api/events` | `?after=<seq>&max=<n≤2000>` | `{"events":[…],"max_seq":N}` |
| POST | `/api/channels` | `{"channels":["UC..","@handle",…]}` | `{"subscribed":n,"unsubscribed":m,"active":k}` |
| POST | `/api/ack` | `{"through":<seq>}` | `{"ok":true}` (advances compaction horizon) |

Event object:

```json
{"seq":42,"received_at":1750000000,"kind":"new",
 "channel_id":"UC...","video_id":"abc","ts":"2026-06-18T12:00:00+00:00","title":"..."}
```

`kind` is `new` | `updated` | `deleted`. Poll incrementally: start at `after=0`, remember the highest
`seq` you processed, and pass it as `after` next time. Delivery is at-least-once — make your handling
idempotent (which a "check the channel now" trigger naturally is).

The callback routes are for YouTube's hub only:
- `GET /yt/cb/<token>` — subscription verification (echoes `hub.challenge`).
- `POST /yt/cb/<token>` — content notification (HMAC-SHA1 verified, then appended).

## Verifying end-to-end (on the VPS)

1. Put one test channel in `channels.txt`; `journalctl -u yt-websub -f` should show the hub's
   verification GET and `[verify] active channel=UC… lease=…s`.
2. Upload/start a video on that channel; the log shows `[event] seq=… kind=… video=…` and a line
   lands in `/var/lib/yt-websub/events.log`.
3. From home: `curl -H "Authorization: Bearer <token>" https://hooks.example.com/api/events?after=0`
   returns the event (a public CA cert means no `--cacert`). A missing/wrong token returns 401.
4. `curl -H "Authorization: Bearer <token>" https://hooks.example.com/api/health` shows
   `subs_active`.

## streamarchiver integration (planned, separate change)

streamarchiver will gain a `src/websub.rs` task (modeled on its `eventsub.rs`) that:
1. `POST /api/channels` with the monitored YouTube `UC...` set (reconcile).
2. `GET /api/events?after=<cursor>` on a short timer; persist the cursor in `app_settings`.
3. For each new event whose `channel_id` maps to a monitor, send `ManualCommand::Start(monitor_id)`
   into the existing channel — which checks liveness now and records **only if actually live**
   (idempotent). A long fallback poll interval stays on those monitors.

Settings used on the streamarchiver side: `websub_vps_url`, `websub_token`, `websub_cursor`.

## Files & state (under `YTWEBSUB_STORAGE_DIR`)

| File | Purpose |
|---|---|
| `events.log` | append-only TSV event log (`seq`-ordered, fsync'd, compacted) |
| `subs.tsv` | subscription registry (channel id, callback token, secret, state, lease/expiry) |
| `ack.txt` | compaction horizon (highest acked `seq`) |
| `resolve.cache` | handle/URL → `UC...` resolution cache |
| `channels.txt` | desired channel list (operator- or API-managed) |

## Security notes

- Inbound callback authenticity rests on a **per-subscription random secret** + constant-time
  HMAC-SHA1 over the body; bad/missing signatures are dropped (acked with 204, never appended).
- `/api` is protected by a bearer token over TLS; optionally also firewall it to your home IP.
- Secrets are generated per subscription (OS RNG) — a single leak doesn't compromise other channels.
- Run as the unprivileged `yt-websub` user under the provided systemd sandbox.

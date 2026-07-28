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
- **Durable & crash-safe:** every event is `fsync`'d before the hub gets its 2xx; state files
  (`subs.tsv`, `ack.txt`) are rewritten atomically (tmp + fsync + rename); a torn final log line is
  skipped on load.
- **Per-subscription callback path** (`/yt/cb/<token>`) identifies which subscription — and thus
  which secret — a notification belongs to, so the body is never trusted to pick the verification key.

## Build

Requires a Rust toolchain (stable). Build the release binary:

```sh
cargo build --release
# -> target/release/yt-websub
cargo test            # 30 tests: config, HMAC vectors, atom, store durability, SSRF allowlist,
                      # + DoS regressions (oversized headers, huge Content-Length)
```

`tiny_http` is **vendored** under `vendor/tiny_http/` (via `[patch.crates-io]`) with several local
hardening patches for the internet-facing listener — all marked `LOCAL PATCH (yt-websub)` and listed
in the `Cargo.toml` patch comment. See **Security notes** below.

Build on the VPS, or cross-compile and copy the binary to `/usr/local/bin/yt-websub`.

## Deploy on Debian

### 1. DNS + TLS (Let's Encrypt)

Point an **A record** (e.g. `hooks.example.com`) at the VPS — needed so the hub and streamarchiver
can reach `:443`. Then issue the cert with the **DNS-01** challenge so **no inbound port (not even 80)
is ever required**. Example for Cloudflare DNS (swap `--dns-cloudflare` for your provider's plugin):

```sh
apt-get install -y certbot python3-certbot-dns-cloudflare

# Cloudflare API token scoped to Zone:DNS:Edit on your zone:
printf 'dns_cloudflare_api_token = REPLACE_with_token\n' > /etc/letsencrypt/cloudflare.ini
chmod 600 /etc/letsencrypt/cloudflare.ini

certbot certonly \
  --dns-cloudflare \
  --dns-cloudflare-credentials /etc/letsencrypt/cloudflare.ini \
  --dns-cloudflare-propagation-seconds 30 \
  -d hooks.example.com
# -> /etc/letsencrypt/live/hooks.example.com/{fullchain,privkey}.pem
```

Let's Encrypt keys are root-only, so the service reads **copies** under `/var/lib/yt-websub/` (which
`YTWEBSUB_TLS_*` already point at). The initial copy is made in step 2; automatic refresh on renewal
is wired up in **step 4** — *after* the service exists, so the hook's restart has something to
restart. (Don't create the deploy hook yet.)

> Avoid `certbot --manual --preferred-challenge dns` — it makes you paste a TXT record by hand on
> every renewal. Use a provider plugin so renewals stay unattended.

### 2. Service user, config, state

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin yt-websub
install -d -o yt-websub -g yt-websub -m 0750 /var/lib/yt-websub
install -m 0755 target/release/yt-websub /usr/local/bin/yt-websub

# Initial cert copy (step 4's hook refreshes these automatically on renewal):
install -o yt-websub -g yt-websub -m 0644 \
  /etc/letsencrypt/live/hooks.example.com/fullchain.pem /var/lib/yt-websub/cert.pem
install -o yt-websub -g yt-websub -m 0600 \
  /etc/letsencrypt/live/hooks.example.com/privkey.pem  /var/lib/yt-websub/key.pem

cp deploy/yt-websub.env.example /etc/yt-websub.env
chown root:yt-websub /etc/yt-websub.env && chmod 0640 /etc/yt-websub.env
# Edit /etc/yt-websub.env: set YTWEBSUB_CALLBACK_BASE and a strong
# YTWEBSUB_BEARER_TOKEN (openssl rand -hex 32). The TLS paths already point at
# the /var/lib/yt-websub copies.

cp deploy/channels.txt.example /var/lib/yt-websub/channels.txt
chown yt-websub:yt-websub /var/lib/yt-websub/channels.txt
# Add your channel ids / @handles / URLs.
```

### 3. systemd

```sh
cp deploy/yt-websub.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now yt-websub
journalctl -u yt-websub -f
```

### 4. Auto-renewal hook

Now that the service exists, wire up cert refresh on renewal. Create
`/etc/letsencrypt/renewal-hooks/deploy/yt-websub.sh` (`chmod +x`):

```sh
#!/bin/sh
install -o yt-websub -g yt-websub -m 0644 \
  /etc/letsencrypt/live/hooks.example.com/fullchain.pem /var/lib/yt-websub/cert.pem
install -o yt-websub -g yt-websub -m 0600 \
  /etc/letsencrypt/live/hooks.example.com/privkey.pem  /var/lib/yt-websub/key.pem
systemctl restart yt-websub || true
```

certbot runs this after each successful renewal — refreshing the copies and reloading the cert
(restart is cheap; subscriptions reload from disk). Test it with `certbot renew --dry-run` (the hook
won't run on a dry-run, but it confirms renewal works), or run the script by hand once.

### 5. Firewall

```sh
ufw default deny incoming
ufw allow 443/tcp      # callback + /api — the only inbound port needed
ufw enable
# DNS-01 means port 80 stays closed. Optional: restrict :443 source to your
# home IP for extra /api safety.
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
| GET | `/api/health` | – | `{"ok":true,"subs_active":k,"max_seq":N,"now":t,"uptime_secs":f,"version":"x.y.z"}` |
| GET | `/api/channels` | – | `{"channels":[{"channel_id":"UC..","state":"active","lease_seconds":L,"expires_at":t,"fail_count":0,"topic":"…"}],"count":k}` |
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

**Authentication & integrity**
- Inbound callback authenticity rests on a **per-subscription random secret** + constant-time
  HMAC-SHA1 over the body; bad/missing signatures are dropped (acked with 204, never appended).
  Stored events are attributed to the subscription they authenticated on — not to the `channel_id`
  claimed in the (signed) body — and every stored field is encoded so a malformed feed can't corrupt
  or forge log rows.
- `/api` is protected by a constant-time bearer-token compare over TLS; optionally also firewall it
  to your home IP. The server **refuses to start** with the shipped placeholder or a bearer token
  shorter than 32 chars.
- Secrets are generated per subscription (OS RNG) — a single leak doesn't compromise other channels.

**Secrets at rest**
- `subs.tsv` (per-sub secrets + callback tokens) and the copied TLS `key.pem` are written `0600`; the
  state directory is `0750` with `UMask=0077`, so no other local user can read them.

**Denial-of-service resistance** (the listener is internet-facing)
- Per-socket read/write timeouts drop slowloris clients; the `tiny_http` worker pool is bounded and
  never panics on a thread-spawn failure, so hitting a resource ceiling drops one connection instead
  of aborting the process.
- Request-line length, header size, header count, and the unread-body drain are all capped, so no
  single request can force unbounded allocation. (These are the `vendor/tiny_http` local patches.)
- The event log is streamed (never read whole into memory) and has an ack-independent retention
  floor, so a stalled consumer can't grow it into an out-of-memory crash-loop.

**Outbound (SSRF)**
- Channel-handle resolution only ever fetches `https://` youtube.com hosts, follows no redirects, and
  caps the response size — so an API-supplied channel entry can't make the server probe internal or
  cloud-metadata endpoints.

**Process**
- Runs as the unprivileged `yt-websub` user under the provided systemd sandbox. `panic = "abort"`
  plus `StartLimitIntervalSec`/`StartLimitBurst` mean an unexpected crash restarts cleanly while a
  genuine crash-loop surfaces as a failed unit instead of flapping silently (set
  `StartLimitIntervalSec=0` to prefer availability over surfacing).

# Package management API

ControlDeck detects the running distro (from `/etc/os-release` `ID`, then `ID_LIKE`)
and exposes a uniform HTTP API over its native package manager.

| Distro family | Manager | Security flags |
|---|---|---|
| Debian, Ubuntu, Mint, … | `apt` | `full` — suites ending in `-security` |
| Fedora, RHEL, Rocky, Alma, … | `dnf` | `full` — `dnf updateinfo list --security` |
| openSUSE, SLE | `zypper` | `full` — `zypper list-patches --category security` |
| Arch, Manjaro, EndeavourOS, … | `pacman` | `partial` — needs `arch-audit` |
| Gentoo | `emerge` | `partial` — needs `glsa-check` (gentoolkit) |
| Alpine | `apk` | `unavailable` — `security` is always `false` |

Unknown derivatives can be forced with `DECK_PKG_MANAGER=apt|dnf|zypper|pacman|emerge|apk`.
On an unsupported distro every operation returns `501`.

## Privileges and security

> **There is no authentication.** Anyone who can reach the API can install software.
> Keep the localhost bind or put an authenticating reverse proxy in front.

- The HTTP API is served by the unprivileged `controldeck` process, which only forwards
  to the `packages` plugin of `controldeck-agent` (root) over a Unix socket that only the
  `controldeck` user can use; see [architecture.md](architecture.md). Every operation
  (including read-only ones) runs in the agent; the web process never executes package
  commands. If the agent is down, these endpoints return `503`.
- Package settings belong to the agent: `DECK_PKG_MANAGER` and `DECK_PKG_READONLY` go in
  `/etc/controldeck/agent.env`, then `systemctl restart controldeck-agent`.
- `DECK_PKG_READONLY=1` disables every mutating endpoint (`403`).
- Package names are restricted to `[A-Za-z0-9+._:@/=~-]`, ≤128 chars, never starting with `-`
  (max 100 per request), and are validated again in the agent. Commands are executed
  directly, never through a shell, with a fixed environment.
- Job output may contain anything the package manager prints (including package scripts' output); treat it as untrusted text and render it as text, never as HTML.
- One mutating operation (job) runs at a time (`409` otherwise). Jobs live in the agent,
  so they keep running, and their output stays replayable, if the web service restarts.
- Operations time out (refresh 5 min, installs 30 min, Gentoo 2 h); returned output is capped at 1 MiB per stream.

## Conventions

- JSON in/out. Errors: `{"error": "message"}` (plus `running_job` on `409`).
- Status codes: `400` invalid input · `403` read-only mode · `404` unknown job ·
  `409` another operation is running · `500` a read command failed · `501` unsupported distro · `503` privileged agent unavailable.
- Updates reflect the current cache. Call `refresh` first for fresh data.
- Read operations (`info`, `updates`, `search`) answer directly. **Mutating operations
  (`refresh`, `install`, `remove`, `updates/install`) are background jobs**: they return `202`
  immediately and their output is streamed (see [Jobs and output streaming](#jobs-and-output-streaming)).
  Failures (non-zero exit, timeout, unsupported operation) show up as a `failed` job, not an HTTP error.

## Endpoints

### `GET /api/packages/info`

```json
{ "distro": "ubuntu", "manager": "apt", "security_support": "full", "read_only": false }
```

`manager` and `security_support` are `null` on unsupported distros.

### `GET /api/packages/updates[?security=true]`

Lists pending updates; `security=true` keeps only security fixes.

```json
{
  "manager": "apt",
  "security_support": "full",
  "count": 1,
  "updates": [{
    "name": "curl",
    "current_version": "8.5.0-2ubuntu10.5",
    "new_version": "8.5.0-2ubuntu10.6",
    "repository": "noble-updates,noble-security",
    "security": true,
    "advisory": null
  }]
}
```

`advisory` holds a CVE list (pacman), GLSA id (emerge), `FEDORA-…`/`RHSA-…` id (dnf) or patch
summary (zypper). On zypper, security fixes are *patches* that can't be mapped reliably to
packages, so each appears as its own entry (`name` = patch name, `new_version` = `null`).

### `GET /api/packages/search?q=<text>`

```json
{ "manager": "apt", "query": "nginx", "count": 1,
  "results": [{ "name": "nginx", "version": null, "description": "small, powerful web server" }] }
```

`version` is only filled where the manager reports it (pacman, emerge, apk).

### Mutating operations (return `202` + a job)

| Endpoint | Body | Command (apt example) |
|---|---|---|
| `POST /api/packages/refresh` | – | `apt-get update` |
| `POST /api/packages/install` | `{"packages": ["git"]}` (non-empty) | `apt-get install -y git` |
| `POST /api/packages/remove` | `{"packages": ["git"]}` (non-empty) | `apt-get remove -y git` |
| `POST /api/packages/updates/install` | `{"packages": [], "security_only": false}` (both optional) | `apt-get upgrade -y` |

`updates/install` upgrades everything when `packages` is empty.

| Manager | Selected packages | `security_only` |
|---|---|---|
| apt | yes | yes (installs the flagged packages) |
| dnf | yes | yes (`--security`) |
| zypper | yes | yes (`patch --category security`, not combinable with `packages`) |
| pacman | unsupported (no partial upgrades on Arch) | unsupported |
| emerge | yes | unsupported |
| apk | yes | unsupported |

Unsupported combinations produce a `failed` job whose `result.error` explains why.

Response (`202 Accepted`, with a `Location` header pointing at `url`):

```json
{
  "job": { "id": "6af3c701-1", "operation": "install", "manager": "apt", "packages": ["git"],
           "status": "running", "started_at": 1794361089, "finished_at": null,
           "duration_ms": null, "line_count": 0, "truncated": false, "result": null },
  "url": "/api/packages/jobs/6af3c701-1",
  "stream_url": "/api/packages/jobs/6af3c701-1/stream"
}
```

Only one job runs at a time; starting another returns `409` with `running_job` so a client can
attach to it instead.

## Jobs and output streaming

A job keeps running if the client disconnects (a closed browser tab must not kill an
installation). Output is recorded, so you can attach at any time, replay from the start, or
resume after a dropped connection. The last 50 jobs are kept in memory (lost on restart);
each keeps at most 20 000 lines (`truncated: true` afterwards).

### `GET /api/packages/jobs`

Recent jobs, newest first (`{"count": n, "jobs": [ … ]}`), without output.

### `GET /api/packages/jobs/{id}`

The job fields above plus the full recorded `output`:

```json
{ "id": "6af3c701-1", "status": "succeeded", "…": "…",
  "result": { "success": true, "exit_code": 0, "timed_out": false, "error": null },
  "output": [
    { "seq": 0, "stream": "command", "text": "apt-get install -y git" },
    { "seq": 1, "stream": "stdout",  "text": "Reading package lists..." },
    { "seq": 2, "stream": "stderr",  "text": "…" }
  ] }
```

`status` is `running`, `succeeded` or `failed`. `stream` is `command` (the command being run;
an operation can run several), `stdout` or `stderr`. Carriage-return progress updates arrive as
separate lines.

### `GET /api/packages/jobs/{id}/stream`

[Server-Sent Events](https://developer.mozilla.org/docs/Web/API/Server-sent_events). Works with
the browser's `EventSource` (which is GET-only, hence the separate job resource).

| Event | `id:` | `data:` |
|---|---|---|
| `output` | line `seq` | `{"seq": 1, "stream": "stdout", "text": "…"}` |
| `done` | – | the final job object (`status`, `result`, `duration_ms`, …) |

- A running job streams live; a finished job replays everything. The stream always ends after
  `done`.
- **Resume:** pass `?after=<seq>` or the `Last-Event-ID` header (sent automatically by
  `EventSource` when it reconnects); only lines after that `seq` are sent.
- Keep-alive comments are sent periodically, and `X-Accel-Buffering: no` is set so nginx does
  not buffer the stream.
- **`EventSource` reconnects automatically when the stream ends**, so close it on `done`.

```js
const res = await fetch('/api/packages/install', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ packages: ['htop'] })
});
if (res.status === 409) { /* attach to (await res.json()).running_job instead */ }
const { stream_url } = await res.json();

const es = new EventSource(stream_url);
es.addEventListener('output', (e) => append(JSON.parse(e.data))); // {seq, stream, text}
es.addEventListener('done', (e) => {
  es.close();
  showResult(JSON.parse(e.data)); // {status, result, …}
});
```

## Example (curl)

```bash
curl -s localhost:8080/api/packages/info
curl -s 'localhost:8080/api/packages/updates?security=true'
curl -s -XPOST localhost:8080/api/packages/refresh          # -> {"stream_url": …}
curl -sN localhost:8080/api/packages/jobs/<id>/stream       # live output
curl -s -XPOST -H 'content-type: application/json' \
     -d '{"packages":["htop"]}' localhost:8080/api/packages/install
```

## Adding a manager

Implement `PackageManager` in `src/pkgmgr/<name>.rs` (pure `parse_*` functions + fixture tests,
commands built as `Cmd`), then register it in `ManagerKind` and `kind_from_distro_id` in
`src/pkgmgr/mod.rs`.

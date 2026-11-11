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

- Read operations (`updates`, `search`, `info`) run unprivileged.
- Mutating operations (`refresh`, installs) need root. If ControlDeck is not root, they run
  through `sudo -n` (never prompts), so a sudoers rule is required. The packaged systemd unit
  uses `DynamicUser`, which **cannot** use sudo: run the service as root, or override the unit
  and grant a narrow sudoers rule, for the commands you want to allow.
- `DECK_PKG_READONLY=1` disables every mutating endpoint (`403`).
- Package names are restricted to `[A-Za-z0-9+._:@/=~-]`, ≤128 chars, never starting with `-`
  (max 100 per request). Commands are executed directly, never through a shell.
- One mutating operation runs at a time (`409` otherwise).
- Operations time out (refresh 5 min, installs 30 min, Gentoo 2 h); returned output is capped at 1 MiB per stream.

## Conventions

- JSON in/out. Errors: `{"error": "message"}`.
- Status codes: `400` invalid input · `403` read-only mode · `409` operation in progress ·
  `500` command could not run / read command failed · `501` unsupported distro or operation.
- Mutating operations that *ran* return `200` even if the command failed: check `success`.
- Updates reflect the current cache. Call `refresh` first for fresh data.

### Command result (returned by every mutating endpoint)

```json
{
  "manager": "apt",
  "success": true,
  "exit_code": 0,
  "timed_out": false,
  "stdout": "…",
  "stderr": "",
  "duration_ms": 2310
}
```

## Endpoints

### `GET /api/packages/info`

```json
{ "distro": "ubuntu", "manager": "apt", "security_support": "full", "read_only": false }
```

`manager` and `security_support` are `null` on unsupported distros.

### `POST /api/packages/refresh`

Refreshes the package cache (`apt-get update`, `dnf makecache --refresh`, `zypper refresh`,
`pacman -Sy`, `emerge --sync`, `apk update`). No body. Returns a command result.

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

### `POST /api/packages/updates/install`

Body (all optional): `{"packages": ["curl"], "security_only": false}`

Empty `packages` upgrades everything. Returns a command result.

| Manager | Selected packages | `security_only` |
|---|---|---|
| apt | yes | yes (installs the flagged packages) |
| dnf | yes | yes (`--security`) |
| zypper | yes | yes (`patch --category security`, not combinable with `packages`) |
| pacman | `501` (partial upgrades unsupported on Arch) | `501` |
| emerge | yes | `501` |
| apk | yes | `501` |

### `GET /api/packages/search?q=<text>`

```json
{ "manager": "apt", "query": "nginx", "count": 1,
  "results": [{ "name": "nginx", "version": null, "description": "small, powerful web server" }] }
```

`version` is only filled where the manager reports it (pacman, emerge, apk).

### `POST /api/packages/install`

Body: `{"packages": ["git", "curl"]}` (non-empty). Returns a command result.

## Example

```bash
curl -s localhost:8080/api/packages/info
curl -s -XPOST localhost:8080/api/packages/refresh
curl -s 'localhost:8080/api/packages/updates?security=true'
curl -s -XPOST -H 'content-type: application/json' \
     -d '{"packages":["htop"]}' localhost:8080/api/packages/install
```

## Adding a manager

Implement `PackageManager` in `src/pkgmgr/<name>.rs` (pure `parse_*` functions + fixture tests,
commands built as `Cmd`), then register it in `ManagerKind` and `kind_from_distro_id` in
`src/pkgmgr/mod.rs`.

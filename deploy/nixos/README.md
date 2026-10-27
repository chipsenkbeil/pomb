# Running Pomb on the homelab (NixOS / green-box)

Pomb is a single self-contained binary (pure Rust, embedded static
assets in release builds). The recommended deployment is a NixOS module on
green-box, following the homelab repo's existing module conventions.

## What lives here

- `pomb.nix` — NixOS module draft. Copy it into the homelab repo as
  `modules/pomb.nix`, register it in `modules/default.nix`, and set
  `homelab.pomb.enable = true;` in `hosts/green-box/default.nix`.

## Deploy steps (run from a machine with SSH to green-box, e.g. your Mac)

1. Pin a commit: in `pomb.nix`, set `rev` to a pomb commit SHA and
   replace `hash` with the `sha256-...` value nix prints on the first build.
2. Add the Caddy route in `modules/proxy.nix`:
   `pomb = simple "127.0.0.1:3000";`
   → serves `https://pomb.chip.network` (wildcard cert already covers it).
3. Deploy: `scripts/deploy-green-box.sh <slot> --switch`
   (no reboot needed — `nixos-rebuild switch` just starts the service).
4. Visit `https://pomb.chip.network/setup` once to create the admin
   account (first run only).
5. In the app, go to **Settings → API tokens**, create a token for your
   agent (e.g. name `dale`, scope `work`), and store it as
   `POMB_API_TOKEN` in the agent's environment.

## Notes

- `homelab.pomb.publicUrl` (default null) sets `POMB_PUBLIC_URL`,
  the base of the device-pairing link and the OpenAPI `servers` entry.
  Without it the link follows the request's `Host` header.
- The service binds `127.0.0.1` (`HOST` env / `--host` flag); Caddy
  terminates TLS and proxies in. Nothing listens on the LAN directly.
  `homelab.pomb.auth` (default `"passkey"`) sets `POMB_AUTH`, and
  passkeys need `publicUrl` set. `homelab.pomb.trustProxy` sets
  `POMB_TRUST_PROXY`, by default `forwarded` under passkeys, so the
  per-address limits count the client Caddy names in `X-Forwarded-For`,
  and `none` under `auth = "none"`, since pomb refuses to start with
  `--auth none` behind a trusted proxy.
- Database: `/var/lib/pomb/pomb.redb` (single redb file, created on
  first start). Back it up like any other state dir.
- Why not `cargo install` on the box: an imperative install isn't in the
  Nix store, doesn't roll back with generations, and breaks the repo's
  declarative pattern. The `buildRustPackage` approach builds from the
  pinned GitHub rev reproducibly.
- Headless alternative: set `POMB_BOOTSTRAP_TOKEN` in the service
  environment to bootstrap API access without touching the UI (see
  `/docs/api.md`). Single mode only: `--mode multi` refuses to start with
  it set.

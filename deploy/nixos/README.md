# Running ChipFlow on the homelab (NixOS / green-box)

ChipFlow is a single self-contained binary (pure Rust, embedded static
assets in release builds). The recommended deployment is a NixOS module on
green-box, following the homelab repo's existing module conventions.

## What lives here

- `chipflow.nix` — NixOS module draft. Copy it into the homelab repo as
  `modules/chipflow.nix`, register it in `modules/default.nix`, and set
  `homelab.chipflow.enable = true;` in `hosts/green-box/default.nix`.

## Deploy steps (run from a machine with SSH to green-box, e.g. your Mac)

1. Pin a commit: in `chipflow.nix`, set `rev` to a chipflow commit SHA and
   replace `hash` with the `sha256-...` value nix prints on the first build.
2. Add the Caddy route in `modules/proxy.nix`:
   `chipflow = simple "127.0.0.1:3000";`
   → serves `https://chipflow.chip.network` (wildcard cert already covers it).
3. Deploy: `scripts/deploy-green-box.sh <slot> --switch`
   (no reboot needed — `nixos-rebuild switch` just starts the service).
4. Visit `https://chipflow.chip.network/setup` once to create the admin
   account (first run only).
5. In the app, go to **Settings → API tokens**, create a token for your
   agent (e.g. name `dale`, scope `write`), and store it as
   `CHIPFLOW_API_TOKEN` in the agent's environment.

## Notes

- The service binds `127.0.0.1` (`HOST` env / `--host` flag); Caddy
  terminates TLS and proxies in. Nothing listens on the LAN directly.
- Database: `/var/lib/chipflow/chipflow.redb` (single redb file, created on
  first start). Back it up like any other state dir.
- Why not `cargo install` on the box: an imperative install isn't in the
  Nix store, doesn't roll back with generations, and breaks the repo's
  declarative pattern. The `buildRustPackage` approach builds from the
  pinned GitHub rev reproducibly.
- Headless alternative: set `CHIPFLOW_API_TOKEN` in the service environment
  to bootstrap API access without touching the UI (see `/agents.md`).

# Genesis Rebrand — Rules for Merging Upstream Updates

This fork renames the product **Wayland → Genesis** (`wayland`→`genesis`,
`Wayland`→`Genesis`, `WAYLAND`→`GENESIS`) across code, docs, and CI. When
merging upstream (FerroxLabs/wayland-core) releases, upstream-touched files
arrive with the old names and must be re-renamed — **but a blind
find-and-replace will break the build and runtime.** Follow these rules.

## NEVER rename (protocol / external-contract tokens)

This codebase also touches **Wayland, the Linux display server** (the CUA
screen-control backend). These stay exactly as upstream writes them:

| Token / pattern | Why |
| --- | --- |
| `WAYLAND_DISPLAY` | Real Linux env var used for session detection |
| `linux_wayland`, `LinuxWayland*` (e.g. `crates/wcore-cua/src/backends/linux_wayland.rs`) | Display-protocol backend module/types |
| `WCORE_CUA_TEST_WAYLAND_*` / `TEST_WAYLAND_` | Compositor-probe test fixtures |
| `WaylandRestricted` | Compositor-restriction error variant |
| Cargo features `wayland` / `wayland-test` in `wcore-cua` | Protocol feature gates (paired with `x11` / `x11-test`) |
| `Xwayland` / `xwayland` | The X11 compat server (a blind rename produced `Xgenesis` once) |
| Prose about the protocol: "Wayland compositor/session/protocol/display", "Linux Wayland", "GNOME on Wayland", `wlrctl`/`grim`/`sway`/`mutter`/`Hyprland` context | Docs must stay technically true |
| OAuth `originator` value `"wayland"` (`oauth/chatgpt.rs`, `oauth/flow.rs`, `openai_chatgpt.rs` + their tests) | Server-validated; renaming can break ChatGPT login |
| `LICENSE`, `NOTICE` attribution to Ferrox Labs, `CHANGELOG.md` history | Legal/history — NOTICE already credits upstream as required (Apache-2.0) |
| Upstream references: `FerroxLabs/wayland#N`, `FerroxLabs/wayland-core#N` issue links, `getwayland.com` (their Desktop product) | Point at real upstream things |

## DO rename (product tokens)

Everything else: crate names `wayland-*`→`genesis-*` (browser/cua/honcho/
ijfw/ollama), the `wayland-core` binary→`genesis-core`, `WAYLAND_HOME`→
`GENESIS_HOME` and other product env vars, config dirs, `wayland-host.wit`,
scripts, docs, CI, and `Cargo.lock` entries for the local crates (no external
crate in the tree contains "wayland" — verified; re-verify after big bumps:
`grep 'name = ".*wayland' Cargo.lock` should list only this repo's crates).

Repo URLs `github.com/FerroxLabs/wayland-core` → `github.com/dmercer290-byte/wayland-core`
(except issue references, above). npm scope `@ferroxlabs/*` refs are left
as-is (the fork has no npm packages yet).

## Recommended merge procedure

1. `git fetch upstream --tags && git merge <release-tag>` (merge tags, not
   upstream main).
2. Conflicts in renamed files: prefer taking upstream's content, then re-apply
   the rename with the protect-list. The original conversion (commit
   `a402b88`, "refactor(branding): rebrand Wayland to Genesis") used
   placeholder-protect → 3-case replace → restore; its protect patterns match
   the table above.
3. Watch for symlinks (`GEMINI.md` → `AGENTS.md`): in-place text replacement
   destroys them; restore with `ln -s`.
4. Verify: `cargo check --workspace --all-targets`, run the touched crates'
   tests, then audit both directions:
   - leftover product "wayland": `grep -rIni wayland . | grep -v <protect-list>` → should be empty
   - wrongly renamed protocol text: `grep -rIn 'Genesis' | grep -iE 'compositor|wlrctl|grim|x11|sway|mutter|xdg'` → should be empty
5. Fork-added changes to preserve across merges: the
   `replace_single_server_tools` idempotent re-add path in
   `wcore-mcp/tool_proxy.rs` + `wcore-cli/main.rs` (#135) — check whether
   upstream shipped its own equivalents before keeping ours (upstream did
   exactly that for two other fixes at 0.12.20, and again for the #136
   tool-call-slot cap: upstream's `MAX_TOOL_CALLS` in
   `wcore-providers/src/openai.rs` superseded the fork's
   `MAX_TOOL_CALL_SLOTS` at the v0.12.22 merge).

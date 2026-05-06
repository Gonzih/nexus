# Plan: Rename amai-* crates to nexus-*

## Task
Rename all 5 `amai-*` crates to `nexus-*` — directories, Cargo.toml names, dependency references, source file internal references, and config file names. No `amai` or `Nexus` string should remain after.

## Approach
Single-pass sed-based rename across all relevant files after physically moving directories. This is the most reliable approach for a pure rename — no logic changes, just string substitution.

## Files to touch
- Root `Cargo.toml` — workspace members + workspace.dependencies + authors/repository strings
- `crates/nexus-tools/Cargo.toml` → `crates/nexus-tools/Cargo.toml`
- `crates/nexus-tools/src/*.rs` (7 files with amai references)
- `crates/nexus-server/Cargo.toml` → `crates/nexus-server/Cargo.toml`
- `crates/nexus-server/src/main.rs`
- `crates/nexus-agent/Cargo.toml` → `crates/nexus-agent/Cargo.toml`
- `crates/nexus-agent/amai-*.toml` → `crates/nexus-agent/nexus-*.toml` (5 files to rename)
- `crates/nexus-agent/src/*.rs` (10 files)
- `crates/nexus-wasm/Cargo.toml` → `crates/nexus-wasm/Cargo.toml`
- `crates/nexus-wasm/src/lib.rs`
- `crates/nexus-app/Cargo.toml` → `crates/nexus-app/Cargo.toml`
- `crates/nexus-app/src/*.rs` (4 files)

## Risks
- cargo check may fail if Rust `use` statements use underscored crate names (nexus_tools → nexus_tools) — sed handles this
- Config toml file renames in nexus-agent need manual mv for the `amai-*.toml` files

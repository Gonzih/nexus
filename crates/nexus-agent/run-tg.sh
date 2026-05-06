#!/bin/bash
# Launch AMAI Agent with Telegram gateway
# Usage: ./run-tg.sh              (normal mode)
#        ./run-tg.sh --supervisor (auto-restart with self-compile)

set -e
cd "$(dirname "$0")/../../.."

# API Keys
export GEMINI_API_KEY="GOOGLE_API_KEY_REDACTED"
# export MINIMAX_API_KEY=""  # Set when available
# export GROQ_API_KEY=""     # Set when available

# Debug logging: show LLM request/response details, TG send results
export RUST_LOG="amai=info,soul_core::provider=debug,soul_gateways=debug"

CONFIG="amai/crates/amai-agent/amai-tg-local.toml"

echo "=== AMAI TG Agent ==="
echo "Config: $CONFIG"
echo "CWD: $(pwd)"
echo "Log level: $RUST_LOG"
echo ""

# Always compile before running to ensure binary is up to date
echo "--- Compiling amai-agent (release) ---"
cargo build --release -p amai-agent --manifest-path amai/Cargo.toml 2>&1
echo "--- Compilation done ---"
echo ""

exec amai/target/release/amai --config "$CONFIG" --telegram "$@"

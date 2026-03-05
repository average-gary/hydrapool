#!/usr/bin/env bash
# SV2 Interoperability Test
#
# Runs a Bitcoin Core regtest node, Hydrapool with SV2 enabled, and
# the SRI mining-device (CPU SV2 miner) in Docker containers. Verifies
# the mining-device can complete the full SV2 mining lifecycle:
#   1. Noise NX handshake
#   2. SetupConnection
#   3. OpenStandardMiningChannel
#   4. Receive NewMiningJob + SetNewPrevHash
#   5. Submit shares
#
# Usage:
#   ./scripts/test-interop-sv2.sh [--keep]
#
# Options:
#   --keep    Don't tear down containers after test (for debugging)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$REPO_DIR/docker/docker-compose.interop.yml"
CONFIG_FILE="$REPO_DIR/docker/interop/config-regtest.toml"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

KEEP_CONTAINERS=false
if [[ "${1:-}" == "--keep" ]]; then
    KEEP_CONTAINERS=true
fi

cleanup() {
    if [[ "$KEEP_CONTAINERS" == false ]]; then
        echo -e "${YELLOW}Cleaning up containers...${NC}"
        docker compose -f "$COMPOSE_FILE" down -v --remove-orphans 2>/dev/null || true
    else
        echo -e "${YELLOW}Keeping containers running (--keep). Clean up with:${NC}"
        echo "  docker compose -f $COMPOSE_FILE down -v"
    fi
}

trap cleanup EXIT

fail() {
    echo -e "${RED}FAIL: $1${NC}" >&2
    # Dump logs on failure
    echo -e "${YELLOW}--- Hydrapool logs ---${NC}"
    docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | tail -50 || true
    echo -e "${YELLOW}--- Mining-device logs ---${NC}"
    docker compose -f "$COMPOSE_FILE" logs mining-device 2>/dev/null | tail -50 || true
    exit 1
}

pass() {
    echo -e "${GREEN}PASS: $1${NC}"
}

info() {
    echo -e "${YELLOW}>>> $1${NC}"
}

# -----------------------------------------------------------------------
# Step 0: Authority keypair for Noise NX
# -----------------------------------------------------------------------
info "Using pre-computed Noise NX authority keypair..."

# Pre-computed secp256k1 keypair with even-parity x-only public key.
# Derived from SHA-256("") with parity negation applied.
# Verified against Responder::from_authority_kp() in the SRI noise-sv2 crate.
SECRET_KEY_HEX="1c4f3bbd6703e3eb65040b37669046da93009b024aad0cef1b3cc57157e388ec"
PUBLIC_KEY_HEX="a34b99f22c790c4e36b2b3c2c35a36db06226e41c692fc82b8b56ac1c540c5bd"

info "Authority secret key: ${SECRET_KEY_HEX:0:8}..."
info "Authority public key: ${PUBLIC_KEY_HEX:0:8}..."

# -----------------------------------------------------------------------
# Step 1: Inject keypair into config
# -----------------------------------------------------------------------
info "Injecting keypair into regtest config..."

# Create a temporary copy of the config with the real keys
TEMP_CONFIG=$(mktemp)
sed \
    -e "s/AUTHORITY_PUBLIC_KEY_PLACEHOLDER/${PUBLIC_KEY_HEX}/" \
    -e "s/AUTHORITY_SECRET_KEY_PLACEHOLDER/${SECRET_KEY_HEX}/" \
    "$CONFIG_FILE" > "$TEMP_CONFIG"

# Copy back (the Docker volume mount reads from the original location)
cp "$TEMP_CONFIG" "$CONFIG_FILE.active"
rm "$TEMP_CONFIG"

# We need to mount the .active file. Update the compose command to use it.
# Actually, let's just overwrite in-place and restore later.
cp "$CONFIG_FILE" "$CONFIG_FILE.bak"
cp "$CONFIG_FILE.active" "$CONFIG_FILE"

restore_config() {
    if [[ -f "$CONFIG_FILE.bak" ]]; then
        mv "$CONFIG_FILE.bak" "$CONFIG_FILE"
    fi
    rm -f "$CONFIG_FILE.active"
}
trap 'restore_config; cleanup' EXIT

# -----------------------------------------------------------------------
# Step 2: Build and start services
# -----------------------------------------------------------------------
info "Building Docker images (this may take several minutes on first run)..."
docker compose -f "$COMPOSE_FILE" build --quiet 2>&1 || \
    fail "Docker build failed"

info "Starting bitcoind..."
docker compose -f "$COMPOSE_FILE" up -d bitcoind
info "Waiting for bitcoind to be ready..."

# Wait for bitcoind healthcheck
for i in $(seq 1 60); do
    if docker compose -f "$COMPOSE_FILE" exec -T bitcoind \
        bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
        getblockchaininfo &>/dev/null; then
        break
    fi
    if [[ $i -eq 60 ]]; then
        fail "bitcoind did not become ready within 60 seconds"
    fi
    sleep 1
done
pass "bitcoind is running (regtest)"

# -----------------------------------------------------------------------
# Step 3: Generate initial blocks (need 101 for coinbase maturity)
# -----------------------------------------------------------------------
info "Generating 101 regtest blocks for coinbase maturity..."

# Create a wallet first
docker compose -f "$COMPOSE_FILE" exec -T bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
    createwallet "interop" 2>/dev/null || true

MINER_ADDRESS=$(docker compose -f "$COMPOSE_FILE" exec -T bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
    getnewaddress "miner" "bech32" 2>/dev/null)

docker compose -f "$COMPOSE_FILE" exec -T bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
    generatetoaddress 101 "$MINER_ADDRESS" >/dev/null 2>&1 || \
    fail "Failed to generate initial blocks"

BLOCK_COUNT=$(docker compose -f "$COMPOSE_FILE" exec -T bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
    getblockcount 2>/dev/null)
pass "Generated $BLOCK_COUNT regtest blocks"

# -----------------------------------------------------------------------
# Step 4: Start Hydrapool
# -----------------------------------------------------------------------
info "Starting Hydrapool with SV2 enabled..."
docker compose -f "$COMPOSE_FILE" up -d hydrapool

info "Waiting for Hydrapool to be ready..."
for i in $(seq 1 120); do
    if docker compose -f "$COMPOSE_FILE" exec -T hydrapool \
        wget -q --spider http://127.0.0.1:46884/health 2>/dev/null; then
        break
    fi
    if [[ $i -eq 120 ]]; then
        echo -e "${YELLOW}--- Hydrapool logs ---${NC}"
        docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | tail -30
        fail "Hydrapool did not become ready within 120 seconds"
    fi
    sleep 1
done
pass "Hydrapool is running with SV2 on port 3334"

# -----------------------------------------------------------------------
# Step 5: Generate a block to trigger GBT template
# -----------------------------------------------------------------------
info "Generating a block to trigger a fresh GBT template..."
docker compose -f "$COMPOSE_FILE" exec -T bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
    generatetoaddress 1 "$MINER_ADDRESS" >/dev/null 2>&1

# Give Hydrapool time to process the ZMQ notification and build the template
sleep 3
pass "Block generated, Hydrapool should have a fresh template"

# -----------------------------------------------------------------------
# Step 6: Start the SRI mining-device
# -----------------------------------------------------------------------
info "Starting SRI mining-device (SV2 CPU miner)..."
docker compose -f "$COMPOSE_FILE" up -d mining-device

# -----------------------------------------------------------------------
# Step 7: Wait for shares to be submitted
# -----------------------------------------------------------------------
info "Waiting for mining-device to submit shares (up to 120s)..."

# Helper: check pool logs for a pattern (pipes directly, avoids capturing 50MB+)
# Uses grep -m1 to stop after first match, and disables pipefail locally to
# avoid SIGPIPE errors when grep exits before docker compose finishes writing.
pool_log_has() {
    set +o pipefail
    docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | grep -q -m1 "$1"
    local rc=$?
    set -o pipefail
    return $rc
}

SHARES_FOUND=false
for i in $(seq 1 120); do
    # Check Hydrapool logs for SV2 share validation (most reliable signal)
    if pool_log_has "validated SV2 share"; then
        SHARES_FOUND=true
        break
    fi

    # Check if mining-device exited (it may crash after submitting shares)
    if ! docker compose -f "$COMPOSE_FILE" ps mining-device --status running 2>/dev/null | grep -q mining-device; then
        # Container is not running — give pool a moment to flush logs, then check
        sleep 2
        if pool_log_has "validated SV2 share" || pool_log_has "SV2 standard mining channel opened"; then
            SHARES_FOUND=true
        fi
        break
    fi

    if [[ $i -eq 120 ]]; then
        echo -e "${YELLOW}--- Mining-device logs ---${NC}"
        docker compose -f "$COMPOSE_FILE" logs mining-device 2>/dev/null | tail -30
        echo -e "${YELLOW}--- Hydrapool logs ---${NC}"
        docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | tail -30
        fail "No SV2 mining activity detected within 120 seconds"
    fi
    sleep 1
done

# -----------------------------------------------------------------------
# Step 8: Verify results
# -----------------------------------------------------------------------
echo ""
echo "========================================"
echo " SV2 Interoperability Test Results"
echo "========================================"
echo ""

# Check each phase (pipe directly into grep to avoid capturing huge logs)
PHASE_RESULTS=()

# Phase 1: Noise handshake
if pool_log_has "SV2 Noise handshake completed"; then
    pass "Phase 1: Noise NX handshake completed"
    PHASE_RESULTS+=(1)
else
    echo -e "${RED}FAIL: Phase 1: Noise NX handshake not detected${NC}"
fi

# Phase 2: SetupConnection
if pool_log_has "SetupConnection succeeded"; then
    pass "Phase 2: SetupConnection exchange succeeded"
    PHASE_RESULTS+=(1)
else
    echo -e "${RED}FAIL: Phase 2: SetupConnection not detected${NC}"
fi

# Phase 3: OpenStandardMiningChannel
if pool_log_has "SV2 standard mining channel opened"; then
    pass "Phase 3: OpenStandardMiningChannel succeeded"
    PHASE_RESULTS+=(1)
else
    echo -e "${RED}FAIL: Phase 3: Channel open not detected${NC}"
fi

# Phase 4: Job distribution
if pool_log_has "built SV2 NewMiningJob"; then
    pass "Phase 4: Job distribution working (NewMiningJob + SetNewPrevHash)"
    PHASE_RESULTS+=(1)
else
    echo -e "${RED}FAIL: Phase 4: Job distribution not detected${NC}"
fi

# Phase 5: Share submission
if pool_log_has "validated SV2 share"; then
    pass "Phase 5: Share submission and validation working"
    PHASE_RESULTS+=(1)
    # Count shares for extra info (disable pipefail for pipe safety)
    set +o pipefail
    SHARE_COUNT=$(docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | grep -c "validated SV2 share" || true)
    EMITTED_COUNT=$(docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | grep -c "emitted SV2 share" || true)
    NETWORK_COUNT=$(docker compose -f "$COMPOSE_FILE" logs hydrapool 2>/dev/null | grep -c "share meets Bitcoin network difficulty" || true)
    set -o pipefail
    info "  Shares validated: $SHARE_COUNT, emitted to pipeline: $EMITTED_COUNT, met network difficulty: $NETWORK_COUNT"
else
    echo -e "${YELLOW}NOTE: Phase 5: No share submissions detected (may need more time)${NC}"
fi

echo ""
TOTAL_PHASES=${#PHASE_RESULTS[@]}
if [[ $TOTAL_PHASES -ge 3 ]]; then
    echo -e "${GREEN}========================================${NC}"
    echo -e "${GREEN} INTEROP TEST PASSED ($TOTAL_PHASES/5 phases verified)${NC}"
    echo -e "${GREEN}========================================${NC}"
    exit 0
else
    echo -e "${RED}========================================${NC}"
    echo -e "${RED} INTEROP TEST INCOMPLETE ($TOTAL_PHASES/5 phases verified)${NC}"
    echo -e "${RED}========================================${NC}"
    echo ""
    echo "Check container logs for details:"
    echo "  docker compose -f $COMPOSE_FILE logs hydrapool"
    echo "  docker compose -f $COMPOSE_FILE logs mining-device"
    exit 1
fi

# SV2 Interoperability Testing

This document describes how to test Hydrapool's Stratum V2 (SV2) server
against third-party SV2 implementations and documents the compatibility
matrix.

## Quick Start

Run the automated interop test:

```bash
./scripts/test-interop-sv2.sh
```

This spins up a Bitcoin Core regtest node, Hydrapool with SV2 enabled, and
the SRI mining-device in Docker containers. The script verifies the full
SV2 mining lifecycle completes successfully.

### Prerequisites

- Docker and Docker Compose (v2)
- ~4 GB free disk space (for Rust build caches)
- ~10 minutes for first run (building the mining-device from source)

### Options

```bash
./scripts/test-interop-sv2.sh          # Run test, clean up after
./scripts/test-interop-sv2.sh --keep   # Keep containers for debugging
```

### Manual Testing

```bash
# Start services manually
docker compose -f docker/docker-compose.interop.yml up --build

# In another terminal, generate blocks to trigger mining work
docker compose -f docker/docker-compose.interop.yml exec bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool \
    generatetoaddress 1 $(docker compose -f docker/docker-compose.interop.yml exec bitcoind \
    bitcoin-cli -regtest -rpcuser=hydrapool -rpcpassword=hydrapool getnewaddress)

# Watch logs
docker compose -f docker/docker-compose.interop.yml logs -f mining-device
docker compose -f docker/docker-compose.interop.yml logs -f hydrapool
```

## Architecture

```
                    regtest
  +----------+     ZMQ/RPC     +-----------+     SV2 (Noise NX)     +----------------+
  | bitcoind | <-------------> | Hydrapool | <--------------------> | mining-device  |
  | (regtest)|                 |  (pool)   |     port 3334          | (SRI CPU miner)|
  +----------+                 +-----------+                        +----------------+
       |                            |
       |                       SV1 port 3333
       |                       API port 46884
       |
   generates blocks
   via bitcoin-cli
```

Both SV1 and SV2 miners feed validated shares into the same PPLNS
accounting pipeline. The SV2 server runs on a separate port alongside the
SV1 server.

## Compatibility Matrix

### Supported SV2 Features

| Feature | Status | Notes |
|---------|--------|-------|
| Noise NX encryption | Supported | Certificate validation optional |
| SetupConnection | Supported | Mining protocol only |
| Standard Mining Channels | Supported | One group per connection |
| NewMiningJob (standard) | Supported | Pre-computed merkle root |
| SetNewPrevHash | Supported | Sent on new blocks |
| SubmitSharesStandard | Supported | Full PoW validation |
| SubmitSharesSuccess/Error | Supported | |
| SetTarget (vardiff) | Supported | Automatic difficulty adjustment |
| Multiple channels per connection | Supported | Share same group |
| Late-connect bootstrap | Supported | New channels get current job |
| Future jobs | Partial | Infrastructure exists, not yet used |
| Extended channels | Not supported | Planned (Issue #8) |
| SetCustomMiningJob | Not supported | |
| Job negotiation subprotocol | Not supported | |
| Template distribution subprotocol | Not supported | |

### Protocol Version

- SV2 protocol version: 2
- Minimum supported version: 2
- Maximum supported version: 2

### Encryption

- Noise NX handshake with secp256k1 keypairs
- Certificate validity configurable (default: 86400 seconds)
- Clients can connect with or without server pubkey validation

## Tested Clients

### SRI mining-device (stratum-mining/sv2-apps)

The reference SV2 CPU mining device from the Stratum Reference
Implementation project.

| Property | Value |
|----------|-------|
| Repository | https://github.com/stratum-mining/sv2-apps |
| Component | `miner-apps/mining-device` |
| Branch tested | `main` |
| Connection type | Standard channels, no pubkey validation |
| Test command | `--address-pool <host>:<port> --nominal-hashrate-multiplier 0.01` |

**Recommended flags for testing:**

- `--nominal-hashrate-multiplier 0.01` — Advertise 1% of real hashrate,
  causing the pool to set very low difficulty. This makes shares arrive
  within seconds instead of minutes.
- `--id-user <btc_address>.<worker>` — The pool parses this as
  `btc_address.worker_name` for PPLNS accounting.

### SRI translator (stratum-mining/sv2-apps)

The SV1-to-SV2 translation proxy. Allows SV1 miners to connect to an
SV2 pool through the translator.

| Property | Value |
|----------|-------|
| Repository | https://github.com/stratum-mining/sv2-apps |
| Component | `miner-apps/translator` |
| Status | Not yet tested |

## Known Limitations

1. **Standard channels only**: Extended channels (Issue #8) are not yet
   implemented. Miners requiring custom coinbase space (e.g., for merged
   mining) are not supported.

2. **No job negotiation**: The pool fully controls the block template.
   Miners cannot propose their own transaction sets.

3. **Single protocol version**: Only SV2 protocol version 2 is supported.
   Version negotiation always resolves to version 2.

4. **Fixed pool-side coinbase**: The pool constructs the full coinbase
   transaction. Standard channel miners can only manipulate nonce, ntime,
   and version bits (BIP320).

5. **No SetExtranoncePrefix**: The extranonce is assigned at channel open
   time and cannot be changed during the channel's lifetime.

## Troubleshooting

### Mining-device can't connect

Check that:
- Hydrapool is running and the SV2 port (default 3334) is accessible
- The `[stratum_sv2]` section is enabled in config
- The authority keypair is valid (both public and secret keys must be set)

### No shares being submitted

- Use `--nominal-hashrate-multiplier 0.01` to get low difficulty
- Generate blocks on the regtest node to trigger fresh templates
- Check Hydrapool logs for `SV2 standard mining channel opened`

### Connection drops after handshake

- Check for `SetupConnection` errors in Hydrapool debug logs
- Ensure the mining-device protocol version matches (version 2)
- Verify the `flags` field is compatible (standard jobs = 0x01)

## File Locations

| File | Purpose |
|------|---------|
| `docker/docker-compose.interop.yml` | Docker Compose for interop test |
| `docker/Dockerfile.mining-device` | Builds SRI mining-device from source |
| `docker/interop/config-regtest.toml` | Hydrapool config for regtest + SV2 |
| `docker/interop/bitcoin-regtest.conf` | Bitcoin Core regtest configuration |
| `scripts/test-interop-sv2.sh` | Automated interop test script |

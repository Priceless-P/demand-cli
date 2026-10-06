# Mining integration tests

Run the CLI configuration checks and self-contained pool protocol/validator tests with the
repository's pinned compiler:

```sh
cargo +1.88.0 test --locked --test cli_args --test pool_simulator
```

The external suite is selected explicitly. It requires a versioned DEMAND `tp-simulator`
binary and pooler's `minerd` (SHA256d support). Missing or invalid binary paths fail the
selected tests; no scenarios are silently skipped.

```sh
TP_SIMULATOR_BIN=/absolute/path/to/tp-simulator \
CPUMINER_BIN=/absolute/path/to/minerd \
TP_SIMULATOR_VERSION=your-artifact-version \
cargo +1.88.0 test --locked --features mining-e2e --test mining_e2e -- --nocapture
```

`TP_SIMULATOR_VERSION` is an optional artifact release/run identifier; the harness always
records the binary's SHA256 and exact invocation. The TP artifact must support
`--rpc-listen-addr`, `--sv2-listen-addr`, `--seed`, `--time-multiplier`, and
`--max-stored-templates`, `--network-hashpower`, the locked SV2 template protocol, and authenticated `getblockcount`/`submitblock`
RPC with its default `username`/`password` credentials. Both single and batch RPC responses
are understood. The suite consumes the TP binary without depending on the private Cargo
workspace. Build changes to TP in its own repository using its pinned 1.95.0 compiler and
toolchain-parity check.

The `Mining end-to-end tests` workflow provides the CI artifact contract: configure
`TP_ARTIFACT_REPOSITORY` to the private repository publishing an Actions artifact named
`tp-simulator-linux-x86_64` containing `tp-simulator` at its root. Configure the
`TP_ARTIFACT_READ_TOKEN` secret with Actions read access to that repository. Dispatch with
the successful producer run ID and the binary's expected SHA256. The producer must build
TP using its pinned compiler and run its own parity/tests checks before uploading. The
consumer checks the checksum, builds cpuminer at a fixed revision, runs the explicitly
selected suite on Rust 1.88.0, and uploads logs even when tests fail. Ordinary CI tests run
without `mining-e2e`; Clippy still compiles all features and targets.

Each scenario launches a fresh proxy with controlled environment variables, auto-update,
browser opening, and monitoring disabled, isolated working directories, and dynamic
loopback ports. Scenarios serialize their external processes within the test executable;
pool peers and miners run concurrently. Normal/error exits terminate and reap children,
shut down the pool actor and its tasks, and preserve artifacts. Drop also terminates
children and aborts tasks if a scenario panics. Port allocation has a short bind-to-launch
race because the external binaries accept addresses rather than prebound sockets.

Artifacts are kept under `target/mining-e2e/<scenario>-<random>/`, or
`MINING_E2E_ARTIFACTS` when set. They contain `events.jsonl`, per-pool event journals,
`summary.json`, scenario configuration/seed, process arguments/environment/working
directories/binary checksums, and separate TP, proxy, and miner logs. Per-pool journals
are written as events occur, so interrupted scenarios retain their evidence. Sequence
numbers are scoped to each pool. A successful mining test requires independently
validated pool submissions; miner acknowledgement logs are an additional check.

To see service logs from startup, run this in a second Bash terminal from the repository
root while the suite is running:

```sh
RUN=$(ls -td -- target/mining-e2e/*/ | head -n 1)
tail -n +1 -F "$RUN"/{tp.log,proxy.log,miner-0.log,pool-0-events.jsonl}
```

This follows one scenario. Rerun it to select the next scenario's directory; services
stop after each scenario finishes. Disconnect/rejection scenarios deliberately generate
errors, so check the test result and `summary.json` for the outcome. Invalid-token tests
do not start a miner.

The actor supports mining and JD connections on one listener, including router probe
connections, target changes, peer disconnects, response pause/resume, forced share and
declaration rejection, snapshots, and deterministic synthetic templates/tips for protocol
tests. Job keys contain peer, channel, and job IDs. Its verifier uses Bitcoin transaction
and header primitives, enforces extranonce length/time/version/tip/target constraints, and
keeps a bounded duplicate set per retained job. JD checks token ownership, transaction
hashes, declaration coinbase, custom-job coinbase/merkle/payout, and current chain tip.

The new-tip scenario mines a valid empty block at an easy network difficulty and submits
it through TP's existing `submitblock` RPC, then checks the announced hash and fresh work.
It records the block and does not rely on the next random background block arriving.
Scripted SV1 negative cases assert rejection at the proxy boundary,
while pool protocol tests assert stale/duplicate/unknown work at the pool boundary.

  Repository layout

  demand-cli/
    tests/
      mining_e2e.rs
      cli_args.rs
      support/
        mod.rs
        processes.rs
        artifacts.rs
        sv1_client.rs
        pool/
          mod.rs
          transport.rs
          templates.rs
          mining.rs
          jobs.rs
          jd.rs
          shares.rs
          control.rs

  mining_e2e.rs contains named scenarios. cli_args.rs checks argument parsing and configuration behavior. The support modules contain the simulator and process management.

  Pool modules and types

   Module          Main types                                        Responsibility
  ━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
   pool/mod.rs     PoolConfig, RunningPool, PoolHandle               Start listeners, own tasks, expose controls, shut down.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   transport.rs    PeerId, PeerConnection, PeerProtocol              Noise handshake, frame decoding/encoding, setup, connection lifetime.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   templates.rs    TemplateClient, TemplateStore                     Connect to TP, receive templates/prevhash, request transaction data.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   mining.rs       MiningSession, ChannelState                       Open/update channels, manage targets and extranonces, dispatch submissions.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   jobs.rs         JobEngine, JobRecord, JobKey                      Generate work and track exactly which job each channel received.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   jd.rs           TokenRegistry, DeclarationState                   Allocate tokens, process declarations, request missing transactions, register custom jobs.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   shares.rs       ShareValidator, ValidatedShare, ShareRejection    Independently verify submitted work and decide acceptance.
  ──────────────  ────────────────────────────────────────────────  ────────────────────────────────────────────────────────────────────────────────────────────
   control.rs      PoolCommand, PoolEvent, PoolSnapshot              Drive scenarios and expose structured observations.

  JobKey should include peer, channel, and job ID. IDs alone must not accidentally identify work belonging to another connection.

  Use a Tokio task that owns mutable pool state, receiving commands through mpsc. This keeps state transitions ordered and avoids putting locks around every operation.

  The proposed API would look like:

  let pool = RunningPool::start(pool_config).await?;
  let handle = pool.handle();

  handle
      .wait_for(EventMatcher::ChannelOpened, deadline)
      .await?;

  handle
      .command(PoolCommand::SetTarget {
          channel,
          target,
      })
      .await?;

  let share = handle.wait_for_validated_share(deadline).await?;

  pool.shutdown().await?;

  Commands should initially include:

  • SetTarget
  • DisconnectPeer
  • PauseResponses / ResumeResponses
  • RejectNextSubmit
  • RejectNextDeclaration
  • Snapshot

  Events should include connection/setup, channel opening, job publication, token allocation, declaration/custom-job registration, accepted/rejected shares, and disconnects.

  Store an event history with sequence numbers. wait_for(...) should check existing events before subscribing, so fast events cannot be missed.

  Libraries to use

  Reuse the CLI’s existing dependencies and locked revisions:

   Library                                          Use
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
   tokio                                            TCP listeners, subprocesses, channels, deadlines, tasks.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   demand-sv2-connection                            Existing Noise transport through Connection::new.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   noise_sv2, codec_sv2, framing_sv2, binary_sv2    Handshake roles, frames, encoding, SV2 field types.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   roles_logic_sv2                                  Mining/JD/template messages, channel and job construction helpers.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   demand-share-accounting-ext                      PoolExtMessages, including DEMAND’s ShareOk acknowledgement.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   bitcoin                                          Transaction decoding, coinbase txid, merkle root, block header, hash and targets.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   serde, serde_json, tracing                       Configurations, event artifacts, diagnostics.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   reqwest                                          TP RPC readiness and chain inspection; proxy API checks.
  ───────────────────────────────────────────────  ───────────────────────────────────────────────────────────────────────────────────
   tempfile — new dev dependency                    Separate working/config/artifact directories per scenario.

  The pool transport should use PoolExtMessages, which includes mining, JD, template, and accounting messages. That matches the proxy’s pool connection implementation (/home/user/src/demand-cli/src/minin_pool_connection/mod.rs).

  For job generation, wrap these existing PoolChannelFactory methods behind JobEngine:

  new_extended_channel
  on_new_template
  on_new_prev_hash_from_tp
  update_target_for_channel
  on_new_set_custom_mining_job

  Use the CLI’s locked demand-open-source/stratum revision. Avoid introducing another SV2 dependency revision through stratum-apps.

  Protocol behavior

  There are two modes, and the suite should cover both.

  1. Non-JD: the simulator connects to TP and generates pool jobs. The proxy connects only to the pool.
  2. JD: both simulator and proxy connect to TP. The simulator additionally handles token allocation, declarations, missing-transaction requests, and SetCustomMiningJob.

  The proxy selects JD when a TP address is configured; this branch is in initialize_proxy (/home/user/src/demand-cli/src/lib.rs).

  One pool listener can accept both mining and JD connections, dispatching them according to SetupConnection.protocol.

  It must support multiple connections from the beginning. Router::get_latency and PoolLatency (/home/user/src/demand-cli/src/router/mod.rs) open preliminary connections and request jobs before the proxy establishes its actual mining session.

  For setup, use the existing published test authority keypair with --local, explicit pool addresses, and a dummy token. No external token service is necessary. Token acceptance/rejection can be an in-memory policy.

  Share verification must be real

  ShareValidator should reconstruct the work from the stored job and incoming SubmitSharesExtended:

  1. Find the peer/channel/job.
  2. Check extranonce length, timestamp, permitted version bits, and job validity.
  3. Build the complete coinbase using the advertised prefix, channel extranonce, submitted extranonce, and suffix.
  4. Compute the coinbase txid and merkle root.
  5. Construct the Bitcoin header and calculate its hash.
  6. Compare against the channel share target and the network target.
  7. Reject duplicate submissions using a bounded per-job set.

  Return something like:

  enum ShareOutcome {
      Accepted(ValidatedShare),
      BlockCandidate(ValidatedShare),
      Rejected(ShareRejection),
  }

  ValidatedShare records the reconstructed header, hash, target, channel/job IDs, and sequence number.

  This verifier should use bitcoin primitives rather than calling the proxy’s share validator. Fixed known-answer vectors should cover byte order, merkle reconstruction, and target comparison.

  For acknowledgements, support:

  enum AckMode {
      StandardSubmitSharesSuccess,
      DemandShareOk,
  }

  The proxy handles both paths in share_accounter::relay_down (/home/user/src/demand-cli/src/share_accounter/mod.rs). Test both.

  For JD, validate token/declaration/custom-job consistency explicitly. The locked factory’s custom-job check currently returns true, so calling that helper alone would not verify a declaration.

  Process harness

  processes.rs should own:

  • TpProcess: start the supplied simulator binary and await RPC/SV2 readiness.
  • ProxyProcess: launch Cargo’s CARGO_BIN_EXE_dmnd-client.
  • CpuMinerProcess: launch a supplied minerd binary.
  • TestRig: assemble the scenario and terminate/reap every child afterward.

  Run a fresh proxy process per scenario. Its Configuration::init (/home/user/src/demand-cli/src/config.rs) uses OnceLock, so repeatedly starting it inside one test process is unsuitable for testing different configurations.

  Use explicit loopback endpoints, separate working directories, controlled environment variables, and disabled auto-update/browser/monitoring by default.

  Reuse TP as a versioned binary, avoiding a Cargo path dependency into the private DEMAND workspace. Record its checksum/version and seed with every run. Existing TP flags provide seed and timing controls; exact externally triggered tip changes would require an additional control interface if we need them.

  Initial scenario matrix

   Scenario                       Required observation
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
   Non-JD mining                  Pool independently validates shares from the real CPU miner.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   JD mining                      Declaration and custom-job registration complete, followed by validated shares.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Both acknowledgement modes     Mining succeeds with standard success and DEMAND ShareOk.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Target change                  Subsequent submissions are checked against the announced target.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   New chain tip                  Previous-tip work is rejected; fresh work succeeds.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Upstream disconnect            Proxy reconnects and mining resumes.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Two pool endpoints             Mining resumes through the remaining available endpoint.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Multiple miners                Work and submissions remain correctly associated with channels.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Invalid token                  Expected setup rejection; no accepted shares.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   CLI/env/TOML configurations    Each selected configuration produces the expected behavior and precedence.
  ─────────────────────────────  ─────────────────────────────────────────────────────────────────────────────────
   Invalid arguments              Nonzero exit and the expected diagnostic.

  Use the real CPU miner for positive mining/recovery tests. sv1_client.rs should provide a small scripted SV1 client for deliberate duplicate, stale, malformed, or unknown-job submissions.

  Negative cases need assertions at the correct boundary: some invalid submissions may be rejected by the proxy before reaching the simulator.

  Implementation order and completion gates

  1. Transport and lifecycle: simultaneous probe/mining/JD connections work; shutdown leaves no tasks or children.
  2. TP and non-JD jobs: proxy receives usable work; channel/extranonce/job consistency tests pass.
  3. Independent validator: known valid/invalid vectors pass, then real minerd produces independently validated shares.
  4. JD: token allocation, declarations, missing transactions, custom jobs, and actual mining pass.
  5. Controls and failure scenarios: target changes, tip changes, rejection, reconnect, and failover pass.
  6. Argument matrix and CI: configuration variants pass; failed runs retain useful artifacts.

  Each run should produce events.jsonl, a summary JSON, exact process arguments/configuration, binary identities, and separate TP/proxy/miner logs. Success should depend on validated pool submissions, not merely a miner’s “accepted” log line.

  Make the external-binary suite an explicitly selected test target, with a proposed invocation:

  TP_SIMULATOR_BIN=/path/to/tp-simulator \
  CPUMINER_BIN=/path/to/minerd \
  cargo +1.88.0 test --locked --features mining-e2e \
    --test mining_e2e -- --nocapture

  Once selected, missing binaries should fail clearly. CI needs a defined way to obtain the private TP artifact. CLI validation should use its pinned 1.88.0; changes to this repo’s TP should use its separately pinned 1.95.0 and pass the toolchain-parity check.

  This keeps the simulator focused on protocol and mining correctness, while making every scenario concrete and independently observable.

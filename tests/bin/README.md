# Bundled mining test binaries

`linux-amd64/` contains executable Linux x86-64 binaries, stripped of debug symbols:

- `tp-simulator`: DEMAND's test template provider, version 0.1.0, built with Rust 1.95.0
  from DEMAND source revision `8ff9e80fdd1d741df65f9158c311e8d868d7a61d`.
  Source paths are remapped to `/build` before stripping.
  It serves SV2 templates and simulated Bitcoin JSON-RPC, so the tests need no Bitcoin
  node. This is the template provider used by the integration suite.
- `minerd`: pooler's cpuminer 2.5.1 at commit
  `5f02105940edb61144c09a7eb960bba04a10d5b7`, built with `./autogen.sh`,
  `./configure`, and `make` (default `-g -O2`, followed by stripping).

The binaries require glibc 2.34 or newer; `minerd` also requires libcurl. The Docker
image supplies those libraries. Use `--platform linux/amd64` when building and running
the image; Docker can emulate this architecture on ARM hosts.

`SHA256SUMS` pins the binaries and the cpuminer source archive. Verify it with:

```sh
(cd tests/bin/linux-amd64 && sha256sum --check SHA256SUMS)
```

Cpuminer's GPLv2 license is in `COPYING.cpuminer`; the bundled Jansson license is in
`LICENSE.jansson`. `cpuminer-source.tar.gz` contains the corresponding upstream source
at that commit, including its build scripts and licenses. Its upstream repository is
https://github.com/pooler/cpuminer.

To update TP, build the replacement in its source repository with its pinned Rust 1.95.0
compiler and toolchain-parity check, then replace the executable here, strip it, update
its version information and checksums, and rerun the Docker suite. Update cpuminer's
source archive and licenses alongside its executable. Docker builds use these checked-in
files directly and do not download either binary from an external artifact repository.

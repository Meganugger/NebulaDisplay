# SPAKE2 cross-stack interop test

Compiles the Android app's **real** `Spake2.kt` + `NdspCrypto.kt` (both pure
JVM) and runs a full SPAKE2 exchange — many rounds plus negative cases —
against the Rust reference (`shared/protocol/examples/spake2_interop.rs`),
asserting confirmation MACs, session keys and token keys agree
byte-for-byte, wrong PINs fail, and token unsealing works end-to-end.

Run from the repo root:

```sh
cargo build -p ndsp-protocol --example spake2_interop
gradle -p viewer/android/interop run --args="$PWD/target/debug/examples/spake2_interop"
```

CI runs this on every push (`android-spake2-interop` job).

// SPAKE2 over P-256 for the Android viewer (NDSP pairing v2) — replaces the
// legacy PIN-bound-HKDF pairing so a recorded pairing transcript can no
// longer be ground offline against the PIN (ROADMAP P1.4).
//
// Byte-compatible with shared/protocol/src/spake2.rs (RFC 9382 construction
// with the NDSP transcript/key schedule); verified against the Rust
// implementation by the JVM interop test in viewer/android/interop/.
//
// Pure JVM (BouncyCastle EC arithmetic) so it is unit-testable off-device.
package dev.nebuladisplay.viewer

import org.bouncycastle.jce.ECNamedCurveTable
import org.bouncycastle.math.ec.ECPoint
import java.math.BigInteger
import java.nio.ByteBuffer
import java.security.MessageDigest
import java.security.SecureRandom

object Spake2 {
    private const val CONTEXT = "ndsp-spake2-v1"
    private const val M_SEED = "ndsp-spake2-M-v1"
    private const val N_SEED = "ndsp-spake2-N-v1"
    private const val W_INFO = "ndsp-spake2-w-v1"
    private const val ID_CLIENT = "client"
    private const val ID_SERVER = "server"

    private val spec = ECNamedCurveTable.getParameterSpec("secp256r1")
    private val curve = spec.curve
    private val order: BigInteger = spec.n

    /** M/N: deterministic "nothing-up-my-sleeve" points — rejection-sample
     *  compressed x-coordinates from SHA-256(seed ‖ counter) until one lies
     *  on the curve (identical loop to the Rust side). */
    private fun derivePoint(seed: String): ECPoint {
        for (counter in 0..255) {
            val x = sha256(seed.toByteArray() + byteArrayOf(counter.toByte()))
            val candidate = ByteArray(33)
            candidate[0] = 0x02 // even-y compressed form
            x.copyInto(candidate, 1)
            try {
                val p = curve.decodePoint(candidate)
                if (!p.isInfinity) return p
            } catch (_: IllegalArgumentException) {
                // x not on the curve — next counter.
            }
        }
        throw IllegalStateException("no curve point within 256 tries")
    }

    private val mPoint: ECPoint by lazy { derivePoint(M_SEED) }
    private val nPoint: ECPoint by lazy { derivePoint(N_SEED) }

    /** PIN → non-zero canonical scalar, bound to the connection nonce. */
    private fun wScalar(pin: String, nonce: ByteArray): BigInteger {
        for (counter in 0..255) {
            val digest = sha256(
                W_INFO.toByteArray() + nonce + pin.toByteArray() + byteArrayOf(counter.toByte())
            )
            val s = BigInteger(1, digest)
            // Canonical (matches Rust Scalar::from_repr): reject >= n or 0.
            if (s.signum() != 0 && s < order) return s
        }
        throw IllegalStateException("no canonical scalar within 256 tries")
    }

    private fun randomScalar(): BigInteger {
        val rng = SecureRandom()
        while (true) {
            val bytes = ByteArray(32).also { rng.nextBytes(it) }
            val s = BigInteger(1, bytes)
            if (s.signum() != 0 && s < order) return s
        }
    }

    private fun sha256(data: ByteArray): ByteArray =
        MessageDigest.getInstance("SHA-256").digest(data)

    private fun encode(p: ECPoint): ByteArray = p.normalize().getEncoded(false) // uncompressed

    private fun decode(bytes: ByteArray): ECPoint {
        val p = try {
            curve.decodePoint(bytes) // validates curve membership
        } catch (e: IllegalArgumentException) {
            throw IllegalArgumentException("invalid SPAKE2 share encoding: ${e.message}")
        }
        require(!p.isInfinity) { "SPAKE2 share is the identity" }
        return p
    }

    private fun BigInteger.toFixed32(): ByteArray {
        val raw = toByteArray()
        val out = ByteArray(32)
        val src = if (raw.size > 32) raw.copyOfRange(raw.size - 32, raw.size) else raw
        src.copyInto(out, 32 - src.size)
        return out
    }

    /** Length-prefixed transcript: context‖idA‖idB‖nonce‖pA‖pB‖K‖w, each
     *  part with a u32-BE byte-length prefix (identical to the Rust side). */
    private fun transcript(
        nonce: ByteArray,
        pa: ByteArray,
        pb: ByteArray,
        k: ByteArray,
        w: BigInteger,
    ): ByteArray {
        val parts = listOf(
            CONTEXT.toByteArray(), ID_CLIENT.toByteArray(), ID_SERVER.toByteArray(),
            nonce, pa, pb, k, w.toFixed32(),
        )
        val buf = ByteBuffer.allocate(parts.sumOf { 4 + it.size })
        for (part in parts) {
            buf.putInt(part.size)
            buf.put(part)
        }
        return buf.array()
    }

    private fun hkdf32(ikm: ByteArray, info: String): ByteArray =
        NdspCrypto.hkdf(ikm, ByteArray(32), info.toByteArray()) // zero salt = RFC default

    class Keys(
        val confirmClient: ByteArray,
        val confirmServer: ByteArray,
        val sessionKey: ByteArray,
        val tokenKey: ByteArray,
    )

    private fun deriveKeys(tt: ByteArray): Keys {
        val kMain = sha256(tt)
        val ka = hkdf32(kMain, "ndsp-spake2-ka-v1")
        val ke = hkdf32(kMain, "ndsp-spake2-ke-v1")
        fun tag(info: String): ByteArray = NdspCrypto.hmacSha256(hkdf32(ka, info), tt)
        return Keys(
            confirmClient = tag("ndsp-spake2-confirm-client-v1"),
            confirmServer = tag("ndsp-spake2-confirm-server-v1"),
            sessionKey = hkdf32(ke, "ndsp-spake2-session-v1"),
            tokenKey = hkdf32(ke, "ndsp-spake2-token-v1"),
        )
    }

    /** Constant-time MAC comparison. */
    fun macEqual(a: ByteArray, b: ByteArray): Boolean {
        if (a.size != b.size) return false
        var diff = 0
        for (i in a.indices) diff = diff or (a[i].toInt() xor b[i].toInt())
        return diff == 0
    }

    /** Client (party A) state: `pA = x·G + w·M`. */
    class Client(pin: String, private val nonce: ByteArray) {
        private val w = wScalar(pin, nonce)
        private val x = randomScalar()

        /** Our masked share `pA` (uncompressed SEC1, 65 bytes). */
        val share: ByteArray = encode(spec.g.multiply(x).add(mPoint.multiply(w)))

        /** Complete with the server share `pB`, producing the key schedule.
         *  @throws IllegalArgumentException on malformed/degenerate shares. */
        fun finish(serverShare: ByteArray): Keys {
            val pb = decode(serverShare)
            val k = pb.subtract(nPoint.multiply(w)).multiply(x).normalize()
            require(!k.isInfinity) { "SPAKE2 derived identity" }
            return deriveKeys(transcript(nonce, share, serverShare, encode(k), w))
        }
    }
}

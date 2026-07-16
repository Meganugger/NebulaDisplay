// NDSP handshake + envelope crypto for Android (JCA).
// Byte-compatible with shared/protocol/src/crypto.rs — see docs/PROTOCOL.md.
package dev.nebuladisplay.viewer

import java.math.BigInteger
import java.nio.ByteBuffer
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec
import java.security.spec.ECPoint
import java.security.spec.ECPublicKeySpec
import javax.crypto.Cipher
import javax.crypto.KeyAgreement
import javax.crypto.Mac
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

object NdspCrypto {
    const val CONFIRM_CONTEXT = "ndsp-confirm-v1"
    private const val PAIR_INFO = "ndsp-pair-v1"
    private const val SESSION_INFO = "ndsp-session-v1"

    class Handshake {
        private val keyPair = KeyPairGenerator.getInstance("EC").apply {
            initialize(ECGenParameterSpec("secp256r1"))
        }.generateKeyPair()

        /** Uncompressed SEC1 point (65 bytes, 0x04-prefixed). */
        val publicRaw: ByteArray = (keyPair.public as ECPublicKey).w.let { w ->
            ByteArray(65).also { out ->
                out[0] = 0x04
                w.affineX.toFixed32().copyInto(out, 1)
                w.affineY.toFixed32().copyInto(out, 33)
            }
        }

        /** ECDH with the server's uncompressed SEC1 point → 32-byte secret. */
        fun agree(peerRaw: ByteArray): ByteArray {
            require(peerRaw.size == 65 && peerRaw[0] == 0x04.toByte()) { "expected uncompressed point" }
            val x = BigInteger(1, peerRaw.copyOfRange(1, 33))
            val y = BigInteger(1, peerRaw.copyOfRange(33, 65))
            val params = (keyPair.public as ECPublicKey).params
            val peerKey = KeyFactory.getInstance("EC")
                .generatePublic(ECPublicKeySpec(ECPoint(x, y), params))
            val ka = KeyAgreement.getInstance("ECDH")
            ka.init(keyPair.private)
            ka.doPhase(peerKey, true)
            return ka.generateSecret()
        }
    }

    private fun BigInteger.toFixed32(): ByteArray {
        val raw = toByteArray()
        val out = ByteArray(32)
        val src = if (raw.size > 32) raw.copyOfRange(raw.size - 32, raw.size) else raw
        src.copyInto(out, 32 - src.size)
        return out
    }

    // ---- HMAC / HKDF-SHA256 (RFC 5869) ----------------------------------------
    fun hmacSha256(key: ByteArray, data: ByteArray): ByteArray =
        Mac.getInstance("HmacSHA256").apply { init(SecretKeySpec(key, "HmacSHA256")) }.doFinal(data)

    fun hkdf(ikm: ByteArray, salt: ByteArray, info: ByteArray, length: Int = 32): ByteArray {
        val prk = hmacSha256(salt, ikm)
        var t = ByteArray(0)
        val out = ByteBuffer.allocate(length)
        var counter = 1
        while (out.position() < length) {
            t = hmacSha256(prk, t + info + byteArrayOf(counter.toByte()))
            out.put(t, 0, minOf(t.size, length - out.position()))
            counter++
        }
        return out.array()
    }

    fun pairingKey(shared: ByteArray, salt: ByteArray, pin: String, nonce: ByteArray): ByteArray =
        hkdf(shared, salt, PAIR_INFO.toByteArray() + pin.toByteArray() + nonce)

    fun sessionKey(shared: ByteArray, salt: ByteArray, nonce: ByteArray): ByteArray =
        hkdf(shared, salt, SESSION_INFO.toByteArray() + nonce)

    // ---- AES-256-GCM one-shot seal/open (random nonce prefix) ----------------
    fun seal(key: ByteArray, plaintext: ByteArray, aad: ByteArray): ByteArray {
        val nonce = ByteArray(12).also { SecureRandom().nextBytes(it) }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        if (aad.isNotEmpty()) c.updateAAD(aad)
        return nonce + c.doFinal(plaintext)
    }

    fun open(key: ByteArray, sealed: ByteArray, aad: ByteArray): ByteArray {
        require(sealed.size >= 28) { "sealed blob too short" }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(128, sealed, 0, 12))
        if (aad.isNotEmpty()) c.updateAAD(aad)
        return c.doFinal(sealed, 12, sealed.size - 12)
    }

    /** Reconnect proof: SHA-256(token || nonce || clientPub || serverPub). */
    fun tokenProof(token: ByteArray, nonce: ByteArray, clientPub: ByteArray, serverPub: ByteArray): ByteArray =
        MessageDigest.getInstance("SHA-256").digest(token + nonce + clientPub + serverPub)
}

/** Post-auth envelope framing: [chan u8][counter u64 BE][GCM ct+tag]. */
class Envelope(sessionKey: ByteArray) {
    companion object {
        const val CHAN_CONTROL = 1
        const val CHAN_VIDEO = 2
        private const val DIR_SERVER = 0
        private const val DIR_CLIENT = 1
    }

    private val key = SecretKeySpec(sessionKey, "AES")
    private val sendCounters = HashMap<Int, Long>()
    private val recvExpected = HashMap<Int, Long>()

    private fun nonce(dir: Int, chan: Int, counter: Long): ByteArray =
        ByteBuffer.allocate(12).put(dir.toByte()).put(chan.toByte()).putShort(0).putLong(counter).array()

    fun seal(chan: Int, plaintext: ByteArray): ByteArray {
        val counter = sendCounters.getOrDefault(chan, 0L)
        sendCounters[chan] = counter + 1
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key, GCMParameterSpec(128, nonce(DIR_CLIENT, chan, counter)))
        c.updateAAD(byteArrayOf(chan.toByte()))
        val ct = c.doFinal(plaintext)
        return ByteBuffer.allocate(9 + ct.size).put(chan.toByte()).putLong(counter).put(ct).array()
    }

    /** @return channel to plaintext, or throws on tamper/replay. */
    fun open(envelope: ByteArray): Pair<Int, ByteArray> {
        require(envelope.size >= 25) { "envelope too short" }
        val chan = envelope[0].toInt()
        val counter = ByteBuffer.wrap(envelope, 1, 8).long
        val expected = recvExpected.getOrDefault(chan, 0L)
        require(counter >= expected) { "replayed envelope" }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, nonce(DIR_SERVER, chan, counter)))
        c.updateAAD(byteArrayOf(chan.toByte()))
        val pt = c.doFinal(envelope, 9, envelope.size - 9)
        recvExpected[chan] = counter + 1
        return chan to pt
    }
}

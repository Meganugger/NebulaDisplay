// NDSP session for Android: WebSocket transport (OkHttp), pairing/token auth,
// encrypted control + video channels. Mirrors shared/client/src/lib.rs.
package dev.nebuladisplay.viewer

import android.util.Base64
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

data class VideoFrame(
    val codec: Int,          // 0=jpeg 1=h264
    val keyframe: Boolean,
    val seq: Long,
    val timestampUs: Long,
    val width: Int,
    val height: Int,
    val payload: ByteArray,
)

interface SessionListener {
    fun onVideo(frame: VideoFrame)
    fun onControl(msg: JSONObject)
    fun onClosed(reason: String)
}

/**
 * Blocking connect (call off the main thread). On success returns a live
 * session already switched to the encrypted phase.
 */
class NdspSession private constructor(
    private val ws: WebSocket,
    private val envelope: Envelope,
    val codec: String,
    val width: Int,
    val height: Int,
    val inputAllowed: Boolean,
    val newlyPairedToken: ByteArray?,   // persist via CredStore when non-null
    val serverFingerprint: String,
) {
    companion object {
        private fun b64(b: ByteArray): String = Base64.encodeToString(b, Base64.NO_WRAP)
        private fun unb64(s: String): ByteArray = Base64.decode(s, Base64.NO_WRAP)

        @Throws(Exception::class)
        fun connect(
            host: String,
            port: Int,
            pin: String?,                 // null → token auth
            creds: CredStore.Credentials?, // null → pairing
            deviceId: String,
            deviceName: String,
            listener: SessionListener,
        ): NdspSession {
            require(pin != null || creds != null) { "need a PIN or stored credentials" }
            val client = OkHttpClient.Builder()
                .readTimeout(0, TimeUnit.MILLISECONDS) // streaming socket
                .build()
            val request = Request.Builder().url("ws://$host:$port/ndsp").build()

            // Handshake runs synchronously over a message queue; after auth we
            // switch the listener to the streaming path.
            val handshakeQueue = LinkedBlockingQueue<String>()
            var streaming: ((ByteString) -> Unit)? = null
            var closedCb: ((String) -> Unit)? = null

            val ws = client.newWebSocket(request, object : WebSocketListener() {
                override fun onMessage(webSocket: WebSocket, text: String) {
                    handshakeQueue.put(text)
                }
                override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                    streaming?.invoke(bytes)
                }
                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    closedCb?.invoke(reason.ifEmpty { "closed ($code)" })
                }
                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    handshakeQueue.put("""{"type":"_transport_error","error":${JSONObject.quote(t.message ?: "io error")}}""")
                    closedCb?.invoke(t.message ?: "transport failure")
                }
            })

            fun send(msg: JSONObject) { ws.send(msg.toString()) }
            fun recv(): JSONObject {
                val text = handshakeQueue.poll(15, TimeUnit.SECONDS)
                    ?: throw Exception("handshake timeout")
                val obj = JSONObject(text)
                if (obj.optString("type") == "_transport_error") throw Exception(obj.optString("error"))
                if (obj.optString("type") == "auth_err") throw Exception(obj.optString("error"))
                return obj
            }

            // 1. hello
            send(JSONObject().apply {
                put("type", "hello")
                put("protocol", 1)
                put("client", JSONObject().apply {
                    put("device_id", deviceId)
                    put("name", deviceName)
                    put("platform", "android")
                    put("app_version", BuildConfig.VERSION_NAME)
                })
                put("auth", JSONObject().apply {
                    if (creds != null) { put("method", "token"); put("device_id", deviceId) }
                    else put("method", "pair")
                })
                put("codecs", JSONArray(listOf("h264", "jpeg")))
            })
            val ack = recv()
            require(ack.getString("type") == "hello_ack") { "expected hello_ack" }
            val nonce = unb64(ack.getString("connection_nonce"))
            val server = ack.getJSONObject("server")
            val fingerprint = server.getString("fingerprint")
            if (creds != null && creds.fingerprint != fingerprint) {
                ws.close(1000, null)
                throw Exception("Host identity changed since pairing — possible impostor. Re-pair with a PIN after verifying the host.")
            }

            // 2. ephemeral ECDH
            val hs = NdspCrypto.Handshake()
            send(JSONObject().put("type", "pair_start").put("client_pubkey", b64(hs.publicRaw)))
            val challenge = recv()
            require(challenge.getString("type") == "pair_challenge") { "expected pair_challenge" }
            val serverPub = unb64(challenge.getString("server_pubkey"))
            val salt = unb64(challenge.getString("salt"))
            val shared = hs.agree(serverPub)
            val sessionKey = NdspCrypto.sessionKey(shared, salt, nonce)

            // 3. prove PIN or token
            var newToken: ByteArray? = null
            if (creds != null) {
                val proof = NdspCrypto.tokenProof(creds.token, nonce, hs.publicRaw, serverPub)
                send(JSONObject().put("type", "token_proof").put("proof", b64(proof)))
            } else {
                val pairKey = NdspCrypto.pairingKey(shared, salt, pin!!, nonce)
                val confirm = NdspCrypto.CONFIRM_CONTEXT.toByteArray() + nonce
                send(JSONObject().put("type", "pair_confirm")
                    .put("sealed", b64(NdspCrypto.seal(pairKey, confirm, ByteArray(0)))))
                val result = recv()
                if (result.getString("type") != "pair_result" || !result.optBoolean("ok"))
                    throw Exception("pairing failed: ${result.optString("error", "wrong PIN?")}")
                newToken = NdspCrypto.open(pairKey, unb64(result.getString("sealed_token")), "token".toByteArray())
            }

            // 4. auth_ok → encrypted phase
            val authOk = recv()
            require(authOk.getString("type") == "auth_ok") { "expected auth_ok" }
            val mode = authOk.getJSONObject("mode")
            val envelope = Envelope(sessionKey)
            val session = NdspSession(
                ws, envelope,
                codec = authOk.getString("codec"),
                width = mode.getInt("width"),
                height = mode.getInt("height"),
                inputAllowed = authOk.optBoolean("input_allowed"),
                newlyPairedToken = newToken,
                serverFingerprint = fingerprint,
            )

            closedCb = { reason -> listener.onClosed(reason) }
            streaming = { bytes ->
                try {
                    val (chan, pt) = envelope.open(bytes.toByteArray())
                    when (chan) {
                        Envelope.CHAN_VIDEO -> listener.onVideo(parseVideoFrame(pt))
                        Envelope.CHAN_CONTROL -> listener.onControl(JSONObject(String(pt)))
                    }
                } catch (e: Exception) {
                    listener.onClosed("protocol error: ${e.message}")
                    ws.close(1002, null)
                }
            }
            return session
        }

        private fun parseVideoFrame(buf: ByteArray): VideoFrame {
            require(buf.size >= 18) { "frame header truncated" }
            val bb = ByteBuffer.wrap(buf)
            val codec = bb.get().toInt()
            val flags = bb.get().toInt()
            val seq = bb.int.toLong() and 0xFFFFFFFFL
            val ts = bb.long
            val w = bb.short.toInt() and 0xFFFF
            val h = bb.short.toInt() and 0xFFFF
            return VideoFrame(codec, flags and 1 != 0, seq, ts, w, h, buf.copyOfRange(18, buf.size))
        }
    }

    // Synchronized: envelope counters must be allocated AND enqueued in the
    // same order (the host closes the session on counter regression), and
    // sendControl is reachable from both the UI thread and OkHttp callbacks.
    @Synchronized
    fun sendControl(msg: JSONObject) {
        ws.send(envelope.seal(Envelope.CHAN_CONTROL, msg.toString().toByteArray()).toByteString())
    }

    fun close() {
        ws.close(1000, "bye")
    }
}

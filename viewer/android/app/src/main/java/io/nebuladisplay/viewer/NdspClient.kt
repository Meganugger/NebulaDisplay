package io.nebuladisplay.viewer

import android.util.Log
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.SecureRandom
import java.security.cert.X509Certificate
import java.util.concurrent.TimeUnit
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

/**
 * NDSP client for Android (wire-compatible with crates/nebula-proto v1).
 *
 * Control messages are JSON over WebSocket text frames; video arrives as
 * binary packets with a 28-byte header (see docs/PROTOCOL.md).
 */
class NdspClient(
    private val listener: Listener,
    private val tokenStore: TokenStore,
) {
    interface Listener {
        fun onStateChanged(state: State, detail: String? = null)
        fun onNeedPin()
        fun onVideoPacket(packet: VideoPacket)
        fun onInputPermission(allowed: Boolean)
        fun onStats(json: JSONObject)
        fun onError(code: String, message: String)
    }

    /** Persist device tokens per host ("host:port" -> token). */
    interface TokenStore {
        fun get(hostKey: String): String?
        fun put(hostKey: String, token: String)
        fun remove(hostKey: String)
    }

    enum class State { DISCONNECTED, CONNECTING, PAIRING, READY, STREAMING }

    data class VideoPacket(
        val fullFrame: Boolean,
        val frameId: Long,
        val x: Int, val y: Int, val w: Int, val h: Int,
        val streamW: Int, val streamH: Int,
        val jpeg: ByteArray, val jpegOffset: Int,
    )

    @Volatile var inputAllowed = false; private set
    @Volatile private var ws: WebSocket? = null
    private var hostKey = ""
    private var profile = "balanced"

    private val deviceId: String by lazy {
        tokenStore.get("__device_id__") ?: run {
            val bytes = ByteArray(16).also { SecureRandom().nextBytes(it) }
            val id = bytes.joinToString("") { "%02x".format(it) }
            tokenStore.put("__device_id__", id)
            id
        }
    }

    /**
     * Trust-on-first-use TLS for the host's self-signed certificate. The QR
     * payload carries the SHA-256 fingerprint; verify it here when provided
     * so a spoofed host is rejected before pairing.
     */
    private fun httpClient(expectedFingerprint: String?): OkHttpClient {
        val builder = OkHttpClient.Builder()
            .pingInterval(15, TimeUnit.SECONDS)
            .connectTimeout(6, TimeUnit.SECONDS)
        val trust = object : X509TrustManager {
            override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {}
            override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
                val fp = expectedFingerprint ?: return
                val digest = java.security.MessageDigest.getInstance("SHA-256")
                    .digest(chain[0].encoded)
                    .joinToString(":") { "%02X".format(it) }
                if (digest != fp) throw java.security.cert.CertificateException(
                    "host certificate fingerprint mismatch"
                )
            }
            override fun getAcceptedIssuers(): Array<X509Certificate> = arrayOf()
        }
        val ssl = SSLContext.getInstance("TLS").apply { init(null, arrayOf(trust), SecureRandom()) }
        builder.sslSocketFactory(ssl.socketFactory, trust)
        builder.hostnameVerifier { _, _ -> true } // identity == pinned fingerprint
        return builder.build()
    }

    fun connect(host: String, port: Int, tls: Boolean, fingerprint: String?, profile: String) {
        this.profile = profile
        hostKey = "$host:$port"
        listener.onStateChanged(State.CONNECTING)
        val scheme = if (tls) "wss" else "ws"
        val request = Request.Builder().url("$scheme://$host:$port/ws").build()
        ws = httpClient(fingerprint).newWebSocket(request, socketListener)
    }

    fun disconnect() {
        sendJson(JSONObject().put("type", "bye").put("resume_token", JSONObject.NULL))
        ws?.close(1000, "bye")
        ws = null
        listener.onStateChanged(State.DISCONNECTED)
    }

    fun pair(pin: String) {
        sendJson(
            JSONObject().put("type", "pair_request").put("pin", pin)
                .put("device_name", "Android (${android.os.Build.MODEL})")
        )
    }

    fun sendInputEvents(events: JSONArray) {
        if (!inputAllowed) return
        sendJson(JSONObject().put("type", "input").put("events", events))
    }

    fun sendFeedback(lastFrame: Long, dropped: Int, decodeMs: Float, queueDepth: Int) {
        sendJson(
            JSONObject().put("type", "feedback")
                .put("last_presented_frame", lastFrame)
                .put("dropped_frames", dropped)
                .put("decode_ms", decodeMs)
                .put("queue_depth", queueDepth)
        )
    }

    private fun sendJson(obj: JSONObject) {
        ws?.send(obj.toString())
    }

    private fun startSession(viewportW: Int, viewportH: Int) {
        listener.onStateChanged(State.READY)
        sendJson(
            JSONObject()
                .put("type", "session_start")
                .put("mode", "mirror")
                .put("profile", profile)
                .put("preferred", JSONObject.NULL)
                .put("viewport_width", viewportW)
                .put("viewport_height", viewportH)
                .put("codecs", JSONArray().put("video/mjpeg"))
                .put("want_audio", false)
        )
    }

    private val socketListener = object : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            sendJson(
                JSONObject()
                    .put("type", "hello")
                    .put("min_version", 1)
                    .put("max_version", 1)
                    .put("client_name", "Android viewer")
                    .put("device_id", deviceId)
                    .put("capabilities", JSONArray().put("video/mjpeg").put("input"))
            )
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            val msg = JSONObject(text)
            when (msg.getString("type")) {
                "hello_ack" -> {
                    val token = tokenStore.get(hostKey)
                    if (msg.getBoolean("known_device") && token != null) {
                        sendJson(JSONObject().put("type", "auth").put("token", token))
                    } else {
                        listener.onStateChanged(State.PAIRING)
                        listener.onNeedPin()
                    }
                }
                "pair_ok" -> {
                    tokenStore.put(hostKey, msg.getString("token"))
                    startSession(1920, 1080)
                }
                "auth_ok" -> {
                    inputAllowed = msg.getBoolean("input_allowed")
                    listener.onInputPermission(inputAllowed)
                    startSession(1920, 1080)
                }
                "session_started" -> listener.onStateChanged(State.STREAMING)
                "session_stop" -> listener.onStateChanged(State.READY, msg.optString("reason"))
                "input_permission" -> {
                    inputAllowed = msg.getBoolean("allowed")
                    listener.onInputPermission(inputAllowed)
                }
                "ping" -> sendJson(JSONObject().put("type", "pong").put("t_micros", msg.getLong("t_micros")))
                "stats" -> listener.onStats(msg)
                "error" -> {
                    val code = msg.getString("code")
                    if (code == "bad_token") {
                        tokenStore.remove(hostKey)
                        listener.onNeedPin()
                    }
                    listener.onError(code, msg.optString("message"))
                }
            }
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            val buf = ByteBuffer.wrap(bytes.toByteArray()).order(ByteOrder.LITTLE_ENDIAN)
            if (buf.remaining() < 28 || buf.get(0) != 0x01.toByte()) return
            if (buf.get(1) != 1.toByte()) return
            val flags = buf.get(3).toInt()
            listener.onVideoPacket(
                VideoPacket(
                    fullFrame = flags and 1 != 0,
                    frameId = buf.getInt(4).toLong() and 0xFFFFFFFFL,
                    x = buf.getShort(16).toInt() and 0xFFFF,
                    y = buf.getShort(18).toInt() and 0xFFFF,
                    w = buf.getShort(20).toInt() and 0xFFFF,
                    h = buf.getShort(22).toInt() and 0xFFFF,
                    streamW = buf.getShort(24).toInt() and 0xFFFF,
                    streamH = buf.getShort(26).toInt() and 0xFFFF,
                    jpeg = bytes.toByteArray(),
                    jpegOffset = 28,
                )
            )
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            Log.w("NdspClient", "socket failure", t)
            listener.onStateChanged(State.DISCONNECTED, t.message)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            listener.onStateChanged(State.DISCONNECTED, reason)
        }
    }
}

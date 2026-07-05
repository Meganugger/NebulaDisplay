package io.nebuladisplay.viewer

import android.content.Context
import org.json.JSONObject
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress

/**
 * LAN discovery: broadcast `NDSP-DISCOVER-1` on UDP 38471 and collect JSON
 * replies. Discovery results are untrusted — pairing (PIN) is always
 * required before streaming, so a rogue responder gains nothing.
 */
object Discovery {
    data class HostInfo(
        val name: String,
        val address: String,
        val port: Int,
        val tls: Boolean,
        val tlsFingerprint: String?,
    )

    fun discover(timeoutMs: Int = 1500): List<HostInfo> {
        val results = mutableListOf<HostInfo>()
        DatagramSocket().use { socket ->
            socket.broadcast = true
            socket.soTimeout = 400
            val probe = "NDSP-DISCOVER-1".toByteArray()
            socket.send(
                DatagramPacket(probe, probe.size, InetAddress.getByName("255.255.255.255"), 38471)
            )
            val deadline = System.currentTimeMillis() + timeoutMs
            val buf = ByteArray(1024)
            while (System.currentTimeMillis() < deadline) {
                try {
                    val packet = DatagramPacket(buf, buf.size)
                    socket.receive(packet)
                    val json = JSONObject(String(packet.data, 0, packet.length))
                    if (json.optString("service") != "nebuladisplay") continue
                    results.add(
                        HostInfo(
                            name = json.optString("name", "NebulaDisplay host"),
                            address = packet.address.hostAddress ?: continue,
                            port = json.optInt("port", 38470),
                            tls = json.optBoolean("tls", true),
                            tlsFingerprint = json.optString("tls_fingerprint").ifEmpty { null },
                        )
                    )
                } catch (_: java.net.SocketTimeoutException) {
                    // keep collecting until the deadline
                }
            }
        }
        return results.distinctBy { "${it.address}:${it.port}" }
    }
}

/** SharedPreferences-backed token store. */
class PrefsTokenStore(context: Context) : NdspClient.TokenStore {
    private val prefs = context.getSharedPreferences("ndsp_tokens", Context.MODE_PRIVATE)
    override fun get(hostKey: String): String? = prefs.getString(hostKey, null)
    override fun put(hostKey: String, token: String) {
        prefs.edit().putString(hostKey, token).apply()
    }
    override fun remove(hostKey: String) {
        prefs.edit().remove(hostKey).apply()
    }
}

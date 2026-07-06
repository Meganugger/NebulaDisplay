// Per-host trust token storage (SharedPreferences; tokens gate access to the
// host's *screen*, the phone itself is the trusted element here).
package dev.nebuladisplay.viewer

import android.content.Context
import java.util.UUID

class CredStore(context: Context) {
    data class Credentials(val token: ByteArray, val fingerprint: String)

    private val prefs = context.getSharedPreferences("ndsp", Context.MODE_PRIVATE)

    val deviceId: String
        get() = prefs.getString("device_id", null) ?: UUID.randomUUID().toString()
            .also { prefs.edit().putString("device_id", it).apply() }

    var lastHost: String?
        get() = prefs.getString("last_host", null)
        set(v) = prefs.edit().putString("last_host", v).apply()

    fun load(host: String): Credentials? {
        val token = prefs.getString("token.$host", null) ?: return null
        val fp = prefs.getString("fp.$host", null) ?: return null
        return Credentials(token.hexToBytes() ?: return null, fp)
    }

    fun save(host: String, token: ByteArray, fingerprint: String) {
        prefs.edit()
            .putString("token.$host", token.joinToString("") { "%02x".format(it) })
            .putString("fp.$host", fingerprint)
            .apply()
    }

    private fun String.hexToBytes(): ByteArray? {
        if (length % 2 != 0) return null
        return ByteArray(length / 2) { substring(it * 2, it * 2 + 2).toInt(16).toByte() }
    }
}

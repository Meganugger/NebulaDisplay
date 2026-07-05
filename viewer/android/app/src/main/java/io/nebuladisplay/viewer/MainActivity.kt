package io.nebuladisplay.viewer

import android.app.AlertDialog
import android.os.Bundle
import android.text.InputType
import android.view.WindowManager
import android.widget.EditText
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import org.json.JSONObject
import java.util.Timer
import java.util.TimerTask
import kotlin.concurrent.thread

/**
 * Single-activity viewer:
 * 1. discovers hosts (or asks for a manual address),
 * 2. handles PIN pairing,
 * 3. shows the fullscreen stream with touch/stylus input.
 */
class MainActivity : AppCompatActivity(), NdspClient.Listener {

    private lateinit var streamView: StreamView
    private lateinit var client: NdspClient
    private var feedbackTimer: Timer? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        streamView = StreamView(this)
        setContentView(streamView)

        client = NdspClient(this, PrefsTokenStore(this))
        streamView.client = client

        pickHostAndConnect()
    }

    private fun pickHostAndConnect() {
        thread {
            val hosts = Discovery.discover()
            runOnUiThread {
                if (hosts.isEmpty()) {
                    promptManualAddress()
                } else if (hosts.size == 1) {
                    connectTo(hosts[0])
                } else {
                    AlertDialog.Builder(this)
                        .setTitle("Choose a NebulaDisplay host")
                        .setItems(hosts.map { "${it.name} (${it.address})" }.toTypedArray()) { _, i ->
                            connectTo(hosts[i])
                        }
                        .setNegativeButton("Enter address…") { _, _ -> promptManualAddress() }
                        .show()
                }
            }
        }
    }

    private fun promptManualAddress() {
        val input = EditText(this).apply {
            hint = "192.168.1.20:38470"
            inputType = InputType.TYPE_TEXT_VARIATION_URI
        }
        AlertDialog.Builder(this)
            .setTitle("Host address")
            .setView(input)
            .setPositiveButton("Connect") { _, _ ->
                val parts = input.text.toString().trim().split(":")
                val host = parts.getOrNull(0) ?: return@setPositiveButton
                val port = parts.getOrNull(1)?.toIntOrNull() ?: 38470
                connectTo(Discovery.HostInfo(host, host, port, tls = true, tlsFingerprint = null))
            }
            .setNegativeButton("Retry discovery") { _, _ -> pickHostAndConnect() }
            .show()
    }

    private fun connectTo(host: Discovery.HostInfo) {
        client.connect(host.address, host.port, host.tls, host.tlsFingerprint, profile = "balanced")
    }

    // ------------------------------------------------------------------
    // NdspClient.Listener
    // ------------------------------------------------------------------

    override fun onStateChanged(state: NdspClient.State, detail: String?) {
        runOnUiThread {
            when (state) {
                NdspClient.State.STREAMING -> startFeedback()
                NdspClient.State.DISCONNECTED -> {
                    stopFeedback()
                    Toast.makeText(this, detail ?: "Disconnected", Toast.LENGTH_SHORT).show()
                }
                else -> {}
            }
        }
    }

    override fun onNeedPin() {
        runOnUiThread {
            val input = EditText(this).apply {
                hint = "6-digit PIN"
                inputType = InputType.TYPE_CLASS_NUMBER
            }
            AlertDialog.Builder(this)
                .setTitle("Pair with host")
                .setMessage("Click “Pair a device” on the host's control panel and enter the PIN.")
                .setView(input)
                .setCancelable(false)
                .setPositiveButton("Pair") { _, _ -> client.pair(input.text.toString().trim()) }
                .setNegativeButton("Cancel") { _, _ -> client.disconnect() }
                .show()
        }
    }

    override fun onVideoPacket(packet: NdspClient.VideoPacket) {
        streamView.submit(packet)
    }

    override fun onInputPermission(allowed: Boolean) {
        runOnUiThread {
            if (!allowed) {
                Toast.makeText(
                    this,
                    "Touch input is disabled — allow it on the host control panel",
                    Toast.LENGTH_LONG
                ).show()
            }
        }
    }

    override fun onStats(json: JSONObject) { /* surfaced in a debug overlay later */ }

    override fun onError(code: String, message: String) {
        runOnUiThread {
            if (code == "bad_pin") {
                Toast.makeText(this, "Wrong PIN", Toast.LENGTH_SHORT).show()
                onNeedPin()
            } else {
                Toast.makeText(this, message, Toast.LENGTH_LONG).show()
            }
        }
    }

    private fun startFeedback() {
        stopFeedback()
        feedbackTimer = Timer().apply {
            scheduleAtFixedRate(object : TimerTask() {
                override fun run() {
                    val (lastFrame, dropped, decodeMs) = streamView.feedbackAndReset()
                    client.sendFeedback(lastFrame, dropped, decodeMs, 0)
                }
            }, 1000, 1000)
        }
    }

    private fun stopFeedback() {
        feedbackTimer?.cancel()
        feedbackTimer = null
    }

    override fun onDestroy() {
        stopFeedback()
        client.disconnect()
        super.onDestroy()
    }
}

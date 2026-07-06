// Main activity: connect form → fullscreen SurfaceView viewer with touch
// forwarding and clock-synced stats.
package dev.nebuladisplay.viewer

import android.annotation.SuppressLint
import android.app.Activity
import android.os.Bundle
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID
import kotlin.concurrent.thread

class MainActivity : Activity(), SessionListener {
    private lateinit var surface: SurfaceView
    private lateinit var connectPane: View
    private lateinit var status: TextView
    private var renderer: StreamRenderer? = null
    private var session: NdspSession? = null
    private var inputAllowed = false
    @Volatile private var pinging = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        surface = findViewById(R.id.stream_surface)
        connectPane = findViewById(R.id.connect_pane)
        status = findViewById(R.id.status)
        val hostField = findViewById<EditText>(R.id.host)
        val pinField = findViewById<EditText>(R.id.pin)
        val creds = CredStore(this)

        hostField.setText(creds.lastHost ?: "")
        findViewById<Button>(R.id.connect).setOnClickListener {
            val host = hostField.text.toString().trim()
            val pin = pinField.text.toString().trim().ifEmpty { null }
            if (host.isEmpty()) { status.text = getString(R.string.err_no_host); return@setOnClickListener }
            connect(host, pin, creds)
        }
        setupTouchForwarding()
    }

    private fun connect(hostPort: String, pin: String?, creds: CredStore) {
        status.text = getString(R.string.connecting)
        val host = hostPort.substringBeforeLast(':')
        val port = hostPort.substringAfterLast(':', "41800").toIntOrNull() ?: 41800
        val stored = creds.load(hostPort)
        if (stored == null && pin == null) {
            status.text = getString(R.string.err_need_pin)
            return
        }
        thread {
            try {
                val s = NdspSession.connect(
                    host, port,
                    pin = if (stored == null) pin else null,
                    creds = stored,
                    deviceId = creds.deviceId,
                    deviceName = android.os.Build.MODEL,
                    listener = this,
                )
                s.newlyPairedToken?.let { creds.save(hostPort, it, s.serverFingerprint) }
                creds.lastHost = hostPort
                session = s
                inputAllowed = s.inputAllowed
                runOnUiThread {
                    connectPane.visibility = View.GONE
                    surface.visibility = View.VISIBLE
                    renderer = StreamRenderer(surface.holder)
                }
                startPingLoop(s)
                s.sendControl(JSONObject().put("type", "set_input_mode").put("mode", "direct_touch"))
            } catch (e: Exception) {
                runOnUiThread { status.text = e.message ?: "connection failed" }
            }
        }
    }

    private fun startPingLoop(s: NdspSession) {
        pinging = true
        thread {
            while (pinging) {
                try {
                    s.sendControl(JSONObject().put("type", "ping")
                        .put("t0_us", System.currentTimeMillis() * 1000))
                } catch (_: Exception) { return@thread }
                Thread.sleep(1000)
            }
        }
    }

    @SuppressLint("ClickableViewAccessibility")
    private fun setupTouchForwarding() {
        surface.setOnTouchListener { v, ev ->
            val s = session ?: return@setOnTouchListener false
            if (!inputAllowed) return@setOnTouchListener true
            val events = JSONArray()
            val phase = when (ev.actionMasked) {
                MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> "start"
                MotionEvent.ACTION_MOVE -> "move"
                MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> "end"
                MotionEvent.ACTION_CANCEL -> "cancel"
                else -> return@setOnTouchListener true
            }
            for (i in 0 until ev.pointerCount) {
                events.put(JSONObject().apply {
                    put("kind", "touch")
                    put("id", ev.getPointerId(i))
                    put("phase", phase)
                    put("x", (ev.getX(i) / v.width).coerceIn(0f, 1f))
                    put("y", (ev.getY(i) / v.height).coerceIn(0f, 1f))
                    put("pressure", ev.getPressure(i))
                })
            }
            s.sendControl(JSONObject().put("type", "input").put("events", events))
            true
        }
    }

    // ---- SessionListener (called from OkHttp threads) -----------------------
    override fun onVideo(frame: VideoFrame) {
        renderer?.onFrame(frame)
    }

    override fun onControl(msg: JSONObject) {
        when (msg.optString("type")) {
            "input_grant" -> {
                inputAllowed = msg.optBoolean("allowed")
                runOnUiThread {
                    Toast.makeText(
                        this,
                        if (inputAllowed) R.string.input_granted else R.string.input_revoked,
                        Toast.LENGTH_SHORT,
                    ).show()
                }
            }
            "bye" -> onClosed(msg.optString("reason", "host ended session"))
        }
    }

    override fun onClosed(reason: String) {
        pinging = false
        runOnUiThread {
            renderer?.release(); renderer = null
            surface.visibility = View.GONE
            connectPane.visibility = View.VISIBLE
            status.text = reason
        }
    }

    override fun onDestroy() {
        pinging = false
        session?.close()
        renderer?.release()
        super.onDestroy()
    }
}

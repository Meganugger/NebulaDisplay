package io.nebuladisplay.viewer

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.Rect
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.LinkedBlockingQueue

/**
 * Renders the remote desktop stream and forwards touch input.
 *
 * A dedicated decode thread drains a latest-wins queue (depth 2) so a slow
 * decode never builds a latency balloon; dirty rects are composited into a
 * persistent stream-sized bitmap that is scaled onto the surface.
 */
class StreamView(context: Context) : SurfaceView(context), SurfaceHolder.Callback {

    var client: NdspClient? = null
    var touchMode: String = "direct" // direct | touchpad | view

    private var streamBitmap: Bitmap? = null
    private val queue = LinkedBlockingQueue<NdspClient.VideoPacket>(2)
    private var decodeThread: Thread? = null
    @Volatile private var running = false

    // Stats for feedback.
    @Volatile private var lastFrameId = 0L
    @Volatile private var droppedFrames = 0
    @Volatile private var decodeMsEma = 0f

    init {
        holder.addCallback(this)
    }

    fun submit(packet: NdspClient.VideoPacket) {
        if (!queue.offer(packet)) {
            queue.poll() // drop oldest (latest wins)
            droppedFrames++
            queue.offer(packet)
        }
    }

    fun feedbackAndReset(): Triple<Long, Int, Float> {
        val out = Triple(lastFrameId, droppedFrames, decodeMsEma)
        droppedFrames = 0
        return out
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        running = true
        decodeThread = Thread({ decodeLoop() }, "ndsp-decode").also { it.start() }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        running = false
        decodeThread?.interrupt()
        decodeThread = null
    }

    private fun decodeLoop() {
        val opts = BitmapFactory.Options().apply { inPreferredConfig = Bitmap.Config.ARGB_8888 }
        while (running) {
            val pkt = try {
                queue.take()
            } catch (_: InterruptedException) {
                break
            }
            val t0 = System.nanoTime()
            val region = BitmapFactory.decodeByteArray(
                pkt.jpeg, pkt.jpegOffset, pkt.jpeg.size - pkt.jpegOffset, opts
            ) ?: continue

            var canvasBitmap = streamBitmap
            if (canvasBitmap == null ||
                canvasBitmap.width != pkt.streamW || canvasBitmap.height != pkt.streamH
            ) {
                canvasBitmap = Bitmap.createBitmap(pkt.streamW, pkt.streamH, Bitmap.Config.ARGB_8888)
                streamBitmap = canvasBitmap
            }
            Canvas(canvasBitmap).drawBitmap(region, pkt.x.toFloat(), pkt.y.toFloat(), null)
            region.recycle()

            lastFrameId = pkt.frameId
            val dt = (System.nanoTime() - t0) / 1e6f
            decodeMsEma = if (decodeMsEma == 0f) dt else decodeMsEma * 0.9f + dt * 0.1f

            present(canvasBitmap)
        }
    }

    private fun present(bitmap: Bitmap) {
        val canvas = holder.lockCanvas() ?: return
        try {
            canvas.drawColor(android.graphics.Color.BLACK)
            // Letterboxed fit.
            val scale = minOf(
                canvas.width.toFloat() / bitmap.width,
                canvas.height.toFloat() / bitmap.height
            )
            val w = (bitmap.width * scale).toInt()
            val h = (bitmap.height * scale).toInt()
            val left = (canvas.width - w) / 2
            val top = (canvas.height - h) / 2
            canvas.drawBitmap(bitmap, null, Rect(left, top, left + w, top + h), null)
        } finally {
            holder.unlockCanvasAndPost(canvas)
        }
    }

    // ------------------------------------------------------------------
    // Touch input → NDSP events (normalized stream coordinates)
    // ------------------------------------------------------------------

    private fun norm(ev: MotionEvent, index: Int): Pair<Float, Float>? {
        val bmp = streamBitmap ?: return null
        val scale = minOf(width.toFloat() / bmp.width, height.toFloat() / bmp.height)
        val w = bmp.width * scale
        val h = bmp.height * scale
        val left = (width - w) / 2
        val top = (height - h) / 2
        val x = (ev.getX(index) - left) / w
        val y = (ev.getY(index) - top) / h
        return if (x in 0f..1f && y in 0f..1f) Pair(x, y) else null
    }

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(ev: MotionEvent): Boolean {
        if (touchMode == "view") return true
        val client = client ?: return true
        val events = JSONArray()
        val phase = when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> "down"
            MotionEvent.ACTION_MOVE -> "move"
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> "up"
            MotionEvent.ACTION_CANCEL -> "cancel"
            else -> return true
        }
        val actionIndex = ev.actionIndex
        for (i in 0 until ev.pointerCount) {
            // For down/up only the action pointer transitions; others move.
            val p = when {
                phase == "move" -> "move"
                i == actionIndex -> phase
                else -> "move"
            }
            val pos = norm(ev, i) ?: continue
            val isStylus = ev.getToolType(i) == MotionEvent.TOOL_TYPE_STYLUS
            if (isStylus) {
                events.put(
                    JSONObject().put("kind", "stylus")
                        .put("x", pos.first).put("y", pos.second)
                        .put("pressure", ev.getPressure(i))
                        .put("tilt_x", JSONObject.NULL).put("tilt_y", JSONObject.NULL)
                        .put("down", p == "down" || p == "move")
                        .put("eraser", ev.getToolType(i) == MotionEvent.TOOL_TYPE_ERASER)
                )
            } else {
                events.put(
                    JSONObject().put("kind", "touch")
                        .put("id", ev.getPointerId(i))
                        .put("phase", p)
                        .put("x", pos.first).put("y", pos.second)
                        .put("pressure", ev.getPressure(i))
                )
            }
        }
        if (events.length() > 0) client.sendInputEvents(events)
        return true
    }
}

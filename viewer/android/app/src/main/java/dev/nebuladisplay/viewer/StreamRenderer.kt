// H.264 (MediaCodec, Annex-B) + JPEG decoding onto a SurfaceView.
package dev.nebuladisplay.viewer

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Rect
import android.media.MediaCodec
import android.media.MediaFormat
import android.view.Surface
import android.view.SurfaceHolder

class StreamRenderer(private val holder: SurfaceHolder) {
    private var codec: MediaCodec? = null
    private var configuredSize: Pair<Int, Int>? = null
    private var sawKeyframe = false

    fun onFrame(frame: VideoFrame) {
        when (frame.codec) {
            1 -> renderH264(frame)
            0 -> renderJpeg(frame)
        }
    }

    private fun ensureCodec(width: Int, height: Int, surface: Surface): MediaCodec {
        val existing = codec
        if (existing != null && configuredSize == width to height) return existing
        existing?.release()
        // Low-latency Annex-B decode straight to the surface.
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
        }
        val c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        c.configure(format, surface, null, 0)
        c.start()
        codec = c
        configuredSize = width to height
        sawKeyframe = false
        return c
    }

    private fun renderH264(frame: VideoFrame) {
        val surface = holder.surface ?: return
        if (!surface.isValid) return
        val c = try {
            ensureCodec(frame.width, frame.height, surface)
        } catch (e: Exception) {
            android.util.Log.e("NDSP", "decoder init failed", e)
            return
        }
        if (!sawKeyframe && !frame.keyframe) return
        sawKeyframe = true
        try {
            val inIdx = c.dequeueInputBuffer(10_000)
            if (inIdx >= 0) {
                c.getInputBuffer(inIdx)!!.apply { clear(); put(frame.payload) }
                c.queueInputBuffer(inIdx, 0, frame.payload.size, frame.timestampUs, 0)
            }
            // Drain everything ready; render newest to the surface.
            val info = MediaCodec.BufferInfo()
            var outIdx = c.dequeueOutputBuffer(info, 0)
            while (outIdx >= 0) {
                c.releaseOutputBuffer(outIdx, true)
                outIdx = c.dequeueOutputBuffer(info, 0)
            }
        } catch (e: IllegalStateException) {
            android.util.Log.w("NDSP", "decoder reset after error", e)
            codec?.release(); codec = null; configuredSize = null
        }
    }

    private fun renderJpeg(frame: VideoFrame) {
        val bmp: Bitmap = BitmapFactory.decodeByteArray(frame.payload, 0, frame.payload.size) ?: return
        val canvas = holder.lockCanvas() ?: return
        try {
            val dst = fitRect(bmp.width, bmp.height, canvas.width, canvas.height)
            canvas.drawColor(android.graphics.Color.BLACK)
            canvas.drawBitmap(bmp, null, dst, null)
        } finally {
            holder.unlockCanvasAndPost(canvas)
            bmp.recycle()
        }
    }

    private fun fitRect(srcW: Int, srcH: Int, dstW: Int, dstH: Int): Rect {
        val scale = minOf(dstW.toFloat() / srcW, dstH.toFloat() / srcH)
        val w = (srcW * scale).toInt()
        val h = (srcH * scale).toInt()
        val x = (dstW - w) / 2
        val y = (dstH - h) / 2
        return Rect(x, y, x + w, y + h)
    }

    fun release() {
        codec?.release()
        codec = null
    }
}

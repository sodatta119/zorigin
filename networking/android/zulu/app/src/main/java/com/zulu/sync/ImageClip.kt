package com.zulu.sync

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.util.Base64
import java.io.ByteArrayOutputStream

/**
 * Turns a shared image (a content:// URI from the share sheet) into the same
 * `data:image/...;base64,...` string the desktop uses, so an image rides the
 * plain `/clip` endpoint exactly like text. Downscaled + JPEG-compressed so a
 * phone photo stays small enough for the LAN clip.
 */
object ImageClip {
    private const val MAX_DIM = 2560
    private const val JPEG_QUALITY = 85

    fun toDataUrl(ctx: Context, uri: Uri): String? {
        return try {
            // First pass: read only the bounds so a huge photo can be sampled
            // down during decode instead of blowing up memory.
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) }
            val longest = maxOf(bounds.outWidth, bounds.outHeight)
            if (longest <= 0) return null
            var sample = 1
            while (longest / (sample * 2) >= MAX_DIM) sample *= 2

            val opts = BitmapFactory.Options().apply { inSampleSize = sample }
            var bmp = ctx.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, opts) }
                ?: return null

            // Exact clamp to MAX_DIM on the longest side.
            val m = maxOf(bmp.width, bmp.height)
            if (m > MAX_DIM) {
                val scale = MAX_DIM.toFloat() / m
                bmp = Bitmap.createScaledBitmap(bmp, (bmp.width * scale).toInt(), (bmp.height * scale).toInt(), true)
            }

            val baos = ByteArrayOutputStream()
            bmp.compress(Bitmap.CompressFormat.JPEG, JPEG_QUALITY, baos)
            "data:image/jpeg;base64," + Base64.encodeToString(baos.toByteArray(), Base64.NO_WRAP)
        } catch (e: Exception) {
            null
        }
    }
}

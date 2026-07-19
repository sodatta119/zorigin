package com.zulu.sync

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.IntentCompat
import kotlin.concurrent.thread

/**
 * The share-sheet handler. Share **text/a link** or an **image** and pick
 * "Zulu": it pushes the item to the paired host's clipboard and finishes - no UI
 * of its own. This is the Android "send" path (the OS blocks background
 * clipboard reads, so an explicit share is how you send from a phone).
 */
class ShareActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val type = intent?.type
        if (intent?.action != Intent.ACTION_SEND || type == null) {
            toastAndFinish(getString(R.string.nothing_to_send))
            return
        }

        val host = Host.get(this)
        if (host.isEmpty()) {
            toastAndFinish(getString(R.string.set_host_first))
            startActivity(Intent(this, MainActivity::class.java))
            return
        }

        when {
            type == "text/plain" -> {
                val text = intent.getStringExtra(Intent.EXTRA_TEXT)
                if (text.isNullOrEmpty()) {
                    toastAndFinish(getString(R.string.nothing_to_send))
                } else {
                    send { Clip.send(host, text) }
                }
            }
            type.startsWith("image/") -> {
                val uri = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
                if (uri == null) {
                    toastAndFinish(getString(R.string.nothing_to_send))
                } else {
                    // Encoding + upload both off the main thread.
                    send {
                        val dataUrl = ImageClip.toDataUrl(this, uri)
                        dataUrl != null && Clip.send(host, dataUrl)
                    }
                }
            }
            else -> toastAndFinish(getString(R.string.nothing_to_send))
        }
    }

    private fun send(work: () -> Boolean) {
        thread {
            val ok = work()
            runOnUiThread { toastAndFinish(if (ok) getString(R.string.sent) else getString(R.string.send_failed)) }
        }
    }

    private fun toastAndFinish(msg: String) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
        finish()
    }
}

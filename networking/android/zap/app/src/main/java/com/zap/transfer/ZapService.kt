package com.zap.transfer

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Environment
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Foreground service that hosts the zap web server on the phone.
 *
 * Android would kill a plain background thread that holds an open socket, so the
 * server must run inside a foreground service with an ongoing notification. The
 * actual server is the Rust core, reached through [NativeBridge].
 */
class ZapService : Service() {

    private var handle: Long = 0L

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (handle == 0L) {
            val dir = intent?.getStringExtra(EXTRA_DIR)
                ?: Environment.getExternalStorageDirectory().absolutePath
            val user = intent?.getStringExtra(EXTRA_USER)
            val pass = intent?.getStringExtra(EXTRA_PASS)
            startServer(dir, user, pass)
        }
        return START_STICKY
    }

    private fun startServer(dir: String, user: String?, pass: String?) {
        // `dir` is chosen in the app (default: the whole shared-storage volume).
        // Reading it requires "All files access" (MANAGE_EXTERNAL_STORAGE),
        // which the activity ensures before starting the service.
        // `user`/`pass` are null unless the user enabled "Secure".
        // Transfer history persists in the app's private files dir, so the
        // Transfers tab survives a server stop/start or app restart.
        val history = java.io.File(filesDir, "transfers.tsv").absolutePath
        handle = NativeBridge.nativeStart(dir, PORT, user, pass, history)
        val url = if (handle != 0L) NativeBridge.nativeUrl(handle) else null

        createChannel()
        startAsForeground(buildNotification(url))
        ZapState.update(url, handle != 0L, handle)
    }

    private fun startAsForeground(notification: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIF_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIF_ID, notification)
        }
    }

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "zap server",
            NotificationManager.IMPORTANCE_LOW,
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun buildNotification(url: String?): Notification {
        val text = url ?: "Starting…"
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("zap is sharing files")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .build()
    }

    override fun onDestroy() {
        if (handle != 0L) {
            NativeBridge.nativeStop(handle)
            handle = 0L
        }
        ZapState.update(null, false, 0L)
        super.onDestroy()
    }

    companion object {
        const val PORT = 8080
        const val EXTRA_DIR = "dir"
        const val EXTRA_USER = "user"
        const val EXTRA_PASS = "pass"
        private const val NOTIF_ID = 1
        private const val CHANNEL_ID = "zap_server"
    }
}

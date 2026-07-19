package com.zap.transfer

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Environment
import android.os.IBinder
import android.os.PowerManager
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
    private var wifiLock: WifiManager.WifiLock? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var netCallback: ConnectivityManager.NetworkCallback? = null

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

        if (handle != 0L) {
            acquireLocks()
            keepWifiUp()
        }
        createChannel()
        startAsForeground(buildNotification(url))
        ZapState.update(url, handle != 0L, handle)
    }

    /**
     * Keep the Wi-Fi radio at full power and the CPU awake while serving.
     *
     * A foreground service prevents the process from being killed, but it does
     * NOT stop Android from putting Wi-Fi into power-save (or throttling the CPU)
     * once the screen turns off - which makes throughput on a phone-hosted server
     * collapse periodically to ~0. A high-perf WifiLock + a partial WakeLock keep
     * transfers smooth. Released in [onDestroy].
     */
    private fun acquireLocks() {
        if (wifiLock == null) {
            val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            @Suppress("DEPRECATION") // HIGH_PERF still works and is right for a throughput server
            wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "zap:wifi").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        if (wakeLock == null) {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "zap:cpu").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
    }

    private fun releaseLocks() {
        wifiLock?.let { if (it.isHeld) it.release() }
        wifiLock = null
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
    }

    /**
     * Hold an active request for a Wi-Fi network for as long as we're serving.
     *
     * On a phone-hosted server the real killer isn't the app being killed - it's
     * the *radio*: with the screen off and a weak-ish signal, Android/MIUI tears
     * Wi-Fi down and switches to mobile data ("Wi-Fi disconnected, isMobileData
     * =true"), which yanks the LAN IP out from under an in-flight download. An
     * outstanding requestNetwork(TRANSPORT_WIFI) is the framework's vote to keep
     * Wi-Fi alive, so the system won't drop it while a transfer is running.
     * We don't bind traffic to it - just express the need. Released in onDestroy.
     */
    private fun keepWifiUp() {
        if (netCallback != null) return
        val cm = getSystemService(ConnectivityManager::class.java) ?: return
        val req = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .build()
        val cb = object : ConnectivityManager.NetworkCallback() {}
        try {
            cm.requestNetwork(req, cb)
            netCallback = cb
        } catch (_: Exception) {
            netCallback = null
        }
    }

    private fun releaseWifi() {
        val cb = netCallback ?: return
        try {
            getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(cb)
        } catch (_: Exception) {
        }
        netCallback = null
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
        releaseLocks()
        releaseWifi()
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

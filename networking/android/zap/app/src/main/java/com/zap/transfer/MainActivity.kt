package com.zap.transfer

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.DocumentsContract
import android.provider.Settings
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.ProgressBar
import android.widget.TextView
import android.widget.Toast
import org.json.JSONArray
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.widget.SwitchCompat
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : AppCompatActivity() {

    private lateinit var status: TextView
    private lateinit var urlView: TextView
    private lateinit var copiedLabel: TextView
    private lateinit var urlActions: View
    private lateinit var hint: TextView
    private lateinit var isolationWarn: View
    private lateinit var folderPath: TextView
    private lateinit var startButton: Button
    private lateinit var stopButton: Button
    private lateinit var changeFolder: Button
    private lateinit var secureSwitch: SwitchCompat
    private lateinit var credBox: View
    private lateinit var userInput: EditText
    private lateinit var passInput: EditText
    private lateinit var tabShare: Button
    private lateinit var tabTransfers: Button
    private lateinit var shareView: View
    private lateinit var transfersView: ViewGroup

    private var transfersTab = false
    private val handler = Handler(Looper.getMainLooper())
    private var polling = false
    private data class Sample(var t: Long, var done: Long, var speed: Double)
    private val speeds = HashMap<Long, Sample>()
    private data class Xfer(val id: Long, val name: String, val path: String, val dir: String, val done: Long, val total: Long?, val finished: Boolean, val ok: Boolean, val verified: Boolean)

    private val pollRunnable = object : Runnable {
        override fun run() {
            refreshTransfers()
            if (polling) handler.postDelayed(this, 500)
        }
    }

    // Watchdog: if the server is reachable but no device connects within the
    // grace period, the likely cause is AP/client isolation or a wrong network.
    private var serverStartedAt = 0L
    private var isoWatching = false
    private val isolationRunnable = object : Runnable {
        override fun run() {
            updateIsolationHint()
            if (isoWatching) handler.postDelayed(this, 2000)
        }
    }

    private val prefs by lazy { getSharedPreferences("zap", Context.MODE_PRIVATE) }
    private val storageRoot: String get() = Environment.getExternalStorageDirectory().absolutePath

    private val notificationPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    private val folderPicker =
        registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
            if (uri == null) return@registerForActivityResult
            val path = treeUriToPath(uri)
            if (path == null) {
                Toast.makeText(this, "Only internal storage folders are supported", Toast.LENGTH_LONG).show()
            } else {
                prefs.edit().putString(KEY_FOLDER, path).apply()
                render()
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContentView(R.layout.activity_main)
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.main)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            insets
        }

        status = findViewById(R.id.status)
        urlView = findViewById(R.id.url)
        copiedLabel = findViewById(R.id.copiedLabel)
        urlActions = findViewById(R.id.urlActions)
        hint = findViewById(R.id.hint)
        isolationWarn = findViewById(R.id.isolationWarn)
        folderPath = findViewById(R.id.folderPath)
        startButton = findViewById(R.id.startButton)
        stopButton = findViewById(R.id.stopButton)
        changeFolder = findViewById(R.id.changeFolder)
        secureSwitch = findViewById(R.id.secureSwitch)
        credBox = findViewById(R.id.credBox)
        userInput = findViewById(R.id.userInput)
        passInput = findViewById(R.id.passInput)
        tabShare = findViewById(R.id.tabShare)
        tabTransfers = findViewById(R.id.tabTransfers)
        shareView = findViewById(R.id.shareView)
        transfersView = findViewById(R.id.transfersView)
        tabShare.setOnClickListener { transfersTab = false; render() }
        tabTransfers.setOnClickListener { transfersTab = true; render() }
        if (intent?.getStringExtra("zap_tab") == "transfers") transfersTab = true

        // Restore saved security settings.
        secureSwitch.isChecked = prefs.getBoolean(KEY_SECURE, false)
        userInput.setText(prefs.getString(KEY_USER, DEFAULT_CRED))
        passInput.setText(prefs.getString(KEY_PASS, DEFAULT_CRED))
        credBox.visibility = if (secureSwitch.isChecked) View.VISIBLE else View.GONE

        secureSwitch.setOnCheckedChangeListener { _, checked ->
            credBox.visibility = if (checked) View.VISIBLE else View.GONE
            if (checked && userInput.text.isNullOrBlank()) userInput.setText(DEFAULT_CRED)
            if (checked && passInput.text.isNullOrBlank()) passInput.setText(DEFAULT_CRED)
        }

        startButton.setOnClickListener { onStartClicked() }
        stopButton.setOnClickListener { stopService(Intent(this, ZapService::class.java)) }
        changeFolder.setOnClickListener { folderPicker.launch(null) }
        findViewById<View>(R.id.copyBtn).setOnClickListener { copyLink() }
        findViewById<View>(R.id.shareBtn).setOnClickListener { shareLink() }

        requestPermissionsUpfront()
        ZapState.onChange = { runOnUiThread { render() } }
        render()
        animateIn()
    }

    override fun onResume() {
        super.onResume()
        render()
    }

    override fun onPause() {
        persistCredentials()
        stopPolling()
        stopIsolationWatch()
        super.onPause()
    }

    override fun onDestroy() {
        ZapState.onChange = null
        super.onDestroy()
    }

    // ---- UI state ----

    private fun render() {
        val running = ZapState.running
        val url = ZapState.url
        // A localhost URL means no Wi-Fi/LAN IP was found - warn instead.
        val noWifi = running && (url == null || url.contains("localhost") || url.contains("127.0.0.1"))
        val reachable = running && !noWifi
        status.text = when {
            !running -> "Stopped"
            reachable -> "✓ Reachable at"
            else -> "Server running"
        }
        status.setTextColor(if (reachable) 0xFF2E9E57.toInt() else 0xFFF2F2F2.toInt())
        status.textSize = if (reachable) 14f else 18f
        urlView.text = when {
            !running -> "Start to share over Wi-Fi"
            noWifi -> "No Wi-Fi detected - connect to Wi-Fi, then Stop and Start again"
            else -> url ?: "…"
        }
        urlView.setTextColor(if (noWifi) 0xFFE0554B.toInt() else if (reachable) 0xFFF2F2F2.toInt() else 0xFF8A8A90.toInt())
        urlView.textSize = if (reachable) 17f else 15f
        urlView.setTypeface(null, if (reachable) Typeface.BOLD else Typeface.NORMAL)
        urlActions.visibility = if (running && !noWifi) View.VISIBLE else View.GONE
        hint.visibility = if (running && !noWifi) View.VISIBLE else View.GONE

        // Watchdog lifecycle: run only while reachable, reset the clock on start.
        if (reachable) {
            if (serverStartedAt == 0L) serverStartedAt = SystemClock.elapsedRealtime()
            startIsolationWatch()
        } else {
            serverStartedAt = 0L
            stopIsolationWatch()
            isolationWarn.visibility = View.GONE
        }

        startButton.isEnabled = !running
        stopButton.isEnabled = running
        startButton.alpha = if (running) 0.5f else 1f
        stopButton.alpha = if (running) 1f else 0.5f

        // Config can only change while stopped.
        setConfigEnabled(!running)
        folderPath.text = folderLabel(currentFolder())

        // Tabs
        shareView.visibility = if (transfersTab) View.GONE else View.VISIBLE
        transfersView.visibility = if (transfersTab) View.VISIBLE else View.GONE
        val accent = 0xFFF5A623.toInt(); val muted = 0xFF8A8A90.toInt()
        tabShare.setTextColor(if (transfersTab) muted else accent)
        tabShare.setTypeface(null, if (transfersTab) Typeface.NORMAL else Typeface.BOLD)
        tabTransfers.setTextColor(if (transfersTab) accent else muted)
        tabTransfers.setTypeface(null, if (transfersTab) Typeface.BOLD else Typeface.NORMAL)
        if (transfersTab) startPolling() else stopPolling()
    }

    // ---- AP/client-isolation watchdog ----

    private fun startIsolationWatch() {
        if (!isoWatching) {
            isoWatching = true
            handler.post(isolationRunnable)
        }
    }

    private fun stopIsolationWatch() {
        isoWatching = false
        handler.removeCallbacks(isolationRunnable)
    }

    /** Show the isolation hint once the grace period passes with zero requests. */
    private fun updateIsolationHint() {
        val running = ZapState.running
        val requests = if (running) NativeBridge.nativeRequests(ZapState.handle) else 0L
        val waited = if (serverStartedAt == 0L) 0L else SystemClock.elapsedRealtime() - serverStartedAt
        val show = running && serverStartedAt != 0L && requests == 0L && waited >= NO_CLIENT_WARN_MS
        isolationWarn.visibility = if (show) View.VISIBLE else View.GONE
    }

    // ---- Transfers tab ----

    private fun startPolling() {
        if (!polling) {
            polling = true
            handler.post(pollRunnable)
        }
    }

    private fun stopPolling() {
        polling = false
        handler.removeCallbacks(pollRunnable)
    }

    private fun refreshTransfers() {
        transfersView.removeAllViews()
        val running = ZapState.running
        val list = if (running) parseTransfers(NativeBridge.nativeTransfers(ZapState.handle)) else emptyList()

        if (list.isEmpty()) {
            val tv = TextView(this).apply {
                text = if (running) "No transfers yet - send or grab a file from another device."
                else "Start the server to see transfers here."
                setTextColor(0xFF8A8A90.toInt())
                textSize = 13f
                setPadding(6, 12, 6, 12)
            }
            transfersView.addView(tv)
            return
        }

        val now = SystemClock.elapsedRealtime()
        for (t in list) {
            val s = speeds.getOrPut(t.id) { Sample(now, t.done, 0.0) }
            val dt = (now - s.t) / 1000.0
            if (dt >= 0.35) {
                val inst = (t.done - s.done) / dt
                s.speed = s.speed * 0.4 + inst * 0.6
                s.t = now; s.done = t.done
            }
            val speed = if (t.finished) 0.0 else s.speed

            val row = layoutInflater.inflate(R.layout.transfer_row, transfersView, false)
            val incoming = t.dir == "up"
            row.findViewById<TextView>(R.id.arrow).text = if (incoming) "⬇" else "⬆"
            row.findViewById<TextView>(R.id.name).text = t.name
            val dirTxt = if (incoming) "Incoming" else "Outgoing"
            row.findViewById<TextView>(R.id.sub).text =
                if (t.total != null && t.total > 0) {
                    val pct = (t.done.toDouble() / t.total * 100).coerceAtMost(100.0).toInt()
                    "$dirTxt · ${humanBytes(t.done)} / ${humanBytes(t.total)} ($pct%)"
                } else {
                    "$dirTxt · ${humanBytes(t.done)}"
                }
            val rate = row.findViewById<TextView>(R.id.rate)
            if (t.finished) {
                rate.text = if (t.ok) (if (t.verified) "✓ verified" else "done") else "failed"
                rate.setTextColor(if (t.ok) 0xFF2E9E57.toInt() else 0xFFE0554B.toInt())
            } else {
                rate.text = humanBytes(speed.toLong()) + "/s"
                rate.setTextColor(0xFFF5A623.toInt())
            }
            val pb = row.findViewById<ProgressBar>(R.id.progress)
            if (t.total != null && t.total > 0 && !t.finished) {
                pb.visibility = View.VISIBLE
                pb.progress = ((t.done.toDouble() / t.total) * 1000).toInt()
            } else {
                pb.visibility = View.GONE
            }
            // "Open" reveals/opens a completed file that still exists on disk.
            val openBtn = row.findViewById<Button>(R.id.openBtn)
            val target = revealTarget(t)
            if (t.finished && t.ok && target != null) {
                openBtn.visibility = View.VISIBLE
                openBtn.setOnClickListener { openFile(target.absolutePath) }
            } else {
                openBtn.visibility = View.GONE
            }
            transfersView.addView(row)
        }
        speeds.keys.retainAll(list.map { it.id }.toSet())
    }

    private fun parseTransfers(json: String?): List<Xfer> {
        if (json.isNullOrBlank()) return emptyList()
        return try {
            val arr = JSONArray(json)
            val out = ArrayList<Xfer>(arr.length())
            for (i in 0 until arr.length()) {
                val o = arr.getJSONObject(i)
                out.add(
                    Xfer(
                        id = o.getLong("id"),
                        name = o.getString("name"),
                        path = o.optString("path", ""),
                        dir = o.getString("dir"),
                        done = o.getLong("done"),
                        total = if (o.isNull("total")) null else o.getLong("total"),
                        finished = o.getBoolean("finished"),
                        ok = o.getBoolean("ok"),
                        verified = o.optBoolean("verified", false),
                    )
                )
            }
            out.reversed() // newest first
        } catch (_: Exception) {
            emptyList()
        }
    }

    private fun humanBytes(n: Long): String {
        val u = arrayOf("B", "KB", "MB", "GB", "TB")
        var v = n.toDouble(); var i = 0
        while (v >= 1024 && i < u.size - 1) { v /= 1024; i++ }
        return if (i == 0) "$n B" else String.format("%.1f %s", v, u[i])
    }

    private fun setConfigEnabled(enabled: Boolean) {
        val a = if (enabled) 1f else 0.5f
        for (v in listOf(secureSwitch, userInput, passInput, changeFolder)) {
            v.isEnabled = enabled
            v.alpha = a
        }
    }

    private fun animateIn() {
        val views = listOf(
            R.id.headerRow, R.id.statusCard, R.id.secureCard, R.id.folderCard, R.id.startButton
        )
        views.forEachIndexed { i, id ->
            val v = findViewById<View>(id)
            v.alpha = 0f
            v.translationY = 24f
            v.animate().alpha(1f).translationY(0f)
                .setStartDelay(60L + i * 70L).setDuration(420L).start()
        }
    }

    // ---- Actions ----

    private fun onStartClicked() {
        if (!hasAllFilesAccess()) {
            status.text = "Grant “All files access”, then press Start"
            openAllFilesSettings()
            return
        }
        persistCredentials()
        val intent = Intent(this, ZapService::class.java)
            .putExtra(ZapService.EXTRA_DIR, currentFolder())
        if (secureSwitch.isChecked) {
            intent.putExtra(ZapService.EXTRA_USER, credUser())
            intent.putExtra(ZapService.EXTRA_PASS, credPass())
        }
        ContextCompat.startForegroundService(this, intent)
    }

    private fun copyLink() {
        val url = ZapState.url ?: return
        val clip = getSystemService(ClipboardManager::class.java)
        clip.setPrimaryClip(ClipData.newPlainText("zap link", url))
        // Flash a brief "Copied" that fades on its own.
        copiedLabel.animate().cancel()
        copiedLabel.alpha = 0f
        copiedLabel.animate().alpha(1f).setDuration(150).withEndAction {
            copiedLabel.animate().alpha(0f).setStartDelay(900).setDuration(350).start()
        }.start()
    }

    private fun shareLink() {
        val base = ZapState.url ?: return
        // When secured, share the keyed URL so the recipient is signed in on open
        // (no password to relay). Falls back to the plain URL otherwise.
        val handle = ZapState.handle
        val url = if (secureSwitch.isChecked && handle != 0L) {
            NativeBridge.nativeShareUrl(handle) ?: base
        } else {
            base
        }
        val text = "Open this on the same Wi-Fi to grab my files:\n$url"
        val send = Intent(Intent.ACTION_SEND).apply {
            type = "text/plain"
            putExtra(Intent.EXTRA_TEXT, text)
        }
        startActivity(Intent.createChooser(send, "Share zap link"))
    }

    /// File to reveal for a transfer: the stored path if it exists, else a
    /// fallback of <shared folder>/<name> (covers older history rows saved before
    /// uploads recorded their path, and files that moved). Null if neither exists.
    private fun revealTarget(t: Xfer): java.io.File? {
        if (t.path.isNotEmpty()) {
            val f = java.io.File(t.path)
            if (f.exists()) return f
        }
        val f = java.io.File(currentFolder(), t.name)
        return if (f.exists()) f else null
    }

    /// Open a received/sent file with the system chooser (via a FileProvider URI).
    private fun openFile(path: String) {
        try {
            val uri = androidx.core.content.FileProvider.getUriForFile(
                this, "$packageName.fileprovider", java.io.File(path)
            )
            val mime = contentResolver.getType(uri) ?: "*/*"
            val view = Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, mime)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            startActivity(Intent.createChooser(view, "Open with"))
        } catch (e: Exception) {
            Toast.makeText(this, "Can't open this file", Toast.LENGTH_SHORT).show()
        }
    }

    // ---- Credentials / folder helpers ----

    private fun credUser(): String = userInput.text.toString().ifBlank { DEFAULT_CRED }
    private fun credPass(): String = passInput.text.toString().ifBlank { DEFAULT_CRED }

    private fun persistCredentials() {
        prefs.edit()
            .putBoolean(KEY_SECURE, secureSwitch.isChecked)
            .putString(KEY_USER, credUser())
            .putString(KEY_PASS, credPass())
            .apply()
    }

    private fun currentFolder(): String = prefs.getString(KEY_FOLDER, null) ?: storageRoot

    private fun folderLabel(path: String): String = when {
        path == storageRoot -> "Whole phone storage"
        path.startsWith("$storageRoot/") -> "/" + path.removePrefix("$storageRoot/")
        else -> path
    }

    private fun treeUriToPath(uri: Uri): String? {
        val docId = DocumentsContract.getTreeDocumentId(uri)
        val parts = docId.split(":", limit = 2)
        if (parts.getOrNull(0) != "primary") return null
        val sub = parts.getOrNull(1).orEmpty()
        return if (sub.isEmpty()) storageRoot else "$storageRoot/$sub"
    }

    // ---- Permissions ----

    private fun requestPermissionsUpfront() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (!hasAllFilesAccess() && !prefs.getBoolean(KEY_ASKED_FILES, false)) {
            prefs.edit().putBoolean(KEY_ASKED_FILES, true).apply()
            openAllFilesSettings()
        }
    }

    private fun hasAllFilesAccess(): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.R || Environment.isExternalStorageManager()

    private fun openAllFilesSettings() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        val intent = Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION).apply {
            data = Uri.parse("package:$packageName")
        }
        startActivity(intent)
    }

    companion object {
        /** Grace period before warning that no device has connected (ms). */
        private const val NO_CLIENT_WARN_MS = 20_000L
        private const val DEFAULT_CRED = "zap"
        private const val KEY_FOLDER = "folder"
        private const val KEY_ASKED_FILES = "asked_files"
        private const val KEY_SECURE = "secure"
        private const val KEY_USER = "user"
        private const val KEY_PASS = "pass"
    }
}

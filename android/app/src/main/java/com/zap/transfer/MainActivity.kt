package com.zap.transfer

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.Settings
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
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
    private lateinit var folderPath: TextView
    private lateinit var startButton: Button
    private lateinit var stopButton: Button
    private lateinit var changeFolder: Button
    private lateinit var secureSwitch: SwitchCompat
    private lateinit var credBox: View
    private lateinit var userInput: EditText
    private lateinit var passInput: EditText

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
        folderPath = findViewById(R.id.folderPath)
        startButton = findViewById(R.id.startButton)
        stopButton = findViewById(R.id.stopButton)
        changeFolder = findViewById(R.id.changeFolder)
        secureSwitch = findViewById(R.id.secureSwitch)
        credBox = findViewById(R.id.credBox)
        userInput = findViewById(R.id.userInput)
        passInput = findViewById(R.id.passInput)

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
        // A localhost URL means no Wi-Fi/LAN IP was found — warn instead.
        val noWifi = running && (url == null || url.contains("localhost") || url.contains("127.0.0.1"))
        status.text = if (running) "Server running" else "Stopped"
        urlView.text = when {
            !running -> "Start to share over Wi-Fi"
            noWifi -> "No Wi-Fi detected — connect to Wi-Fi, then Stop and Start again"
            else -> url ?: "…"
        }
        urlView.setTextColor(if (noWifi) 0xFFE0554B.toInt() else 0xFF8A8A90.toInt())
        urlActions.visibility = if (running && !noWifi) View.VISIBLE else View.GONE
        hint.visibility = if (running && !noWifi) View.VISIBLE else View.GONE

        startButton.isEnabled = !running
        stopButton.isEnabled = running
        startButton.alpha = if (running) 0.5f else 1f
        stopButton.alpha = if (running) 1f else 0.5f

        // Config can only change while stopped.
        setConfigEnabled(!running)
        folderPath.text = folderLabel(currentFolder())
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
        val url = ZapState.url ?: return
        val text = buildString {
            append("Open this on the same Wi-Fi to grab my files:\n")
            append(url)
            if (secureSwitch.isChecked) append("\n\nLogin — ${credUser()} / ${credPass()}")
        }
        val send = Intent(Intent.ACTION_SEND).apply {
            type = "text/plain"
            putExtra(Intent.EXTRA_TEXT, text)
        }
        startActivity(Intent.createChooser(send, "Share zap link"))
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
        private const val DEFAULT_CRED = "zap"
        private const val KEY_FOLDER = "folder"
        private const val KEY_ASKED_FILES = "asked_files"
        private const val KEY_SECURE = "secure"
        private const val KEY_USER = "user"
        private const val KEY_PASS = "pass"
    }
}

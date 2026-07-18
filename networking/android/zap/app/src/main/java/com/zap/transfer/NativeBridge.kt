package com.zap.transfer

/**
 * Kotlin side of the JNI bridge into the Rust `zap-android` library.
 *
 * The three `external` functions map to the exported symbols in
 * `crates/zap-android/src/lib.rs`. The library name "zap_android" matches the
 * `.so` files bundled under `src/main/jniLibs/<abi>/libzap_android.so`.
 */
object NativeBridge {
    init {
        System.loadLibrary("zap_android")
    }

    /**
     * Start the web server sharing [dir] on [port], bound to all interfaces.
     * Pass [user] and [pass] to require a login (HTTP Basic auth), or null for
     * none. Returns an opaque handle, or 0 if the server could not be started.
     */
    external fun nativeStart(dir: String, port: Int, user: String?, pass: String?, history: String?): Long

    /** The URL another device on the same Wi-Fi should open, or null. */
    external fun nativeUrl(handle: Long): String?

    /** Share URL - includes the pairing key when secured (recipient auto-signs-in). */
    external fun nativeShareUrl(handle: Long): String?

    /** Recent transfers as a JSON array string (see Rust doc). "[]" if none. */
    external fun nativeTransfers(handle: Long): String?

    /**
     * Number of HTTP requests any client has made since start. While this stays
     * 0, no device has reached the phone - the UI uses it to warn about wrong
     * Wi-Fi / AP-client isolation instead of showing a dead link.
     */
    external fun nativeRequests(handle: Long): Long

    /** Stop the server and release the handle. Safe to call with 0. */
    external fun nativeStop(handle: Long)
}

/**
 * Tiny in-process holder so the [MainActivity] UI can reflect what
 * [ZapService] is doing. Both run in the same process, so a shared object is
 * enough - no IPC needed.
 */
object ZapState {
    @Volatile
    var url: String? = null
        private set

    @Volatile
    var running: Boolean = false
        private set

    /** Native handle of the running server (0 when stopped) - for polling transfers. */
    @Volatile
    var handle: Long = 0
        private set

    /** Set by the activity to be notified when [update] is called. */
    var onChange: (() -> Unit)? = null

    fun update(url: String?, running: Boolean, handle: Long) {
        this.url = url
        this.running = running
        this.handle = handle
        onChange?.invoke()
    }
}

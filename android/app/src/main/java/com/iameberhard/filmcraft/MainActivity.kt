package com.iameberhard.filmcraft

import android.content.ContentValues
import android.net.Uri
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity
import java.io.File
import java.io.FileInputStream

/**
 * The Android shell around the Rust app (`apps/filmcraft-android`, loaded as
 * `libfilmcraft_android.so` by GameActivity's glue). The Rust side calls [pickOpen],
 * [pickOpenMany] and [exportFile] through JNI; picked files are copied into `files/Media/` and
 * their paths go back through [nativeDeliverFile].
 */
class MainActivity : GameActivity() {

    companion object {
        private const val TAG = "filmcraft"
        private const val FOLDER = "FilmCraft"

        init {
            System.loadLibrary("filmcraft_android")
        }
    }

    /** Implemented in Rust: hands a picked file (display name, path of the copy) to the app. */
    private external fun nativeDeliverFile(name: String, path: String)

    /** Files this session created in Downloads, so saving the same name again overwrites. */
    private val savedUris = HashMap<String, Uri>()

    private val openDocument = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri: Uri? ->
        if (uri != null) deliver(listOf(uri))
    }

    private val openDocuments = registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris: List<Uri> ->
        if (uris.isNotEmpty()) deliver(uris)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        hideSystemBars()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    /** Full screen for the canvas; a swipe from an edge shows the bars briefly. */
    private fun hideSystemBars() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    // ---- Called from Rust (any thread) --------------------------------------------------------

    /** File › Open (one file): the system picker. All files: projects have no MIME type. */
    fun pickOpen() {
        runOnUiThread {
            try {
                openDocument.launch(arrayOf("*/*"))
            } catch (e: Exception) {
                Log.e(TAG, "picker failed", e)
                toast("Couldn't open the file picker: ${e.message}")
            }
        }
    }

    /** File › Import (several files). */
    fun pickOpenMany() {
        runOnUiThread {
            try {
                openDocuments.launch(arrayOf("*/*"))
            } catch (e: Exception) {
                Log.e(TAG, "picker failed", e)
                toast("Couldn't open the file picker: ${e.message}")
            }
        }
    }

    /**
     * Save or Export: copy the file at [path] to `Downloads/FilmCraft/[name]` through MediaStore.
     * The same name saved again in this session overwrites the file. Returns null on success,
     * else a message.
     */
    fun exportFile(name: String, path: String): String? {
        return try {
            val src = File(path)
            if (!src.isFile) return "$path is not a file"
            val resolver = contentResolver
            val existing = savedUris[name]
            val uri: Uri = existing ?: run {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, name)
                    put(MediaStore.Downloads.MIME_TYPE, mimeFor(name))
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/" + FOLDER)
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return "couldn't create $name in Downloads"
            }
            val out = resolver.openOutputStream(uri, "wt") ?: return "couldn't open $name for writing"
            out.use { o -> FileInputStream(src).use { i -> i.copyTo(o, 1 shl 20) } }
            if (existing == null) {
                resolver.update(uri, ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }, null, null)
                savedUris[name] = uri
            }
            toast("Saved to Downloads/$FOLDER/$name")
            null
        } catch (e: Exception) {
            Log.e(TAG, "export failed", e)
            e.message ?: e.toString()
        }
    }

    // ---- Helpers ------------------------------------------------------------------------------

    /**
     * Copy each picked document into `files/Media/<name>` (media are read in place by range, so
     * the app needs a real file) and hand the paths to Rust. Off the UI thread: a video can be
     * gigabytes.
     */
    private fun deliver(uris: List<Uri>) {
        Thread {
            val dir = File(filesDir, "Media")
            dir.mkdirs()
            for (uri in uris) {
                try {
                    val name = (displayName(uri) ?: "file").replace('/', '_').replace('\\', '_')
                    val dest = File(dir, name)
                    val input = contentResolver.openInputStream(uri)
                    if (input == null) {
                        toast("Couldn't read $name")
                        continue
                    }
                    input.use { i -> dest.outputStream().use { o -> i.copyTo(o, 1 shl 20) } }
                    nativeDeliverFile(name, dest.absolutePath)
                } catch (e: Exception) {
                    Log.e(TAG, "open failed", e)
                    toast("Couldn't open the file: ${e.message}")
                }
            }
        }.start()
    }

    private fun displayName(uri: Uri): String? {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (i >= 0) return c.getString(i)
            }
        }
        return uri.lastPathSegment
    }

    private fun mimeFor(name: String): String = when (name.substringAfterLast('.', "").lowercase()) {
        "mp4", "m4v" -> "video/mp4"
        "mov" -> "video/quicktime"
        "mkv" -> "video/x-matroska"
        "webm" -> "video/webm"
        "mxf" -> "application/mxf"
        "ts" -> "video/mp2t"
        "wav" -> "audio/wav"
        "aac" -> "audio/aac"
        "m4a" -> "audio/mp4"
        "mp3" -> "audio/mpeg"
        "ogg", "oga" -> "audio/ogg"
        "opus" -> "audio/opus"
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "gif" -> "image/gif"
        "srt" -> "application/x-subrip"
        "vtt" -> "text/vtt"
        "json" -> "application/json"
        "xml", "fcpxml" -> "application/xml"
        else -> "application/octet-stream"
    }

    private fun toast(text: String) {
        runOnUiThread { Toast.makeText(this, text, Toast.LENGTH_SHORT).show() }
    }
}

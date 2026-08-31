package com.orgsync.p2p

// The Android half of the bridge.
//
// Mirrors the iOS module exactly: strings in, strings out, node events pushed
// onto React Native's device event emitter. All work happens on a background
// dispatcher because it touches SQLite and the network.

import android.util.Log
import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.facebook.react.bridge.ReadableMap
import com.facebook.react.modules.core.DeviceEventManagerModule
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import uniffi.p2p_mobile.P2pClient
import uniffi.p2p_mobile.P2pConfig
import uniffi.p2p_mobile.P2pListener
import java.io.File

class P2pNativeModule(private val reactContext: ReactApplicationContext) :
    ReactContextBaseJavaModule(reactContext) {

    private var client: P2pClient? = null
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    override fun getName() = "P2pNative"

    // React Native requires these two to exist for NativeEventEmitter; the
    // subscription bookkeeping itself is handled on the JS side.
    @ReactMethod fun addListener(eventName: String) = Unit
    @ReactMethod fun removeListeners(count: Int) = Unit

    private fun emit(json: String) {
        if (!reactContext.hasActiveReactInstance()) return
        reactContext
            .getJSModule(DeviceEventManagerModule.RCTDeviceEventEmitter::class.java)
            .emit("p2p", json)
    }

    @ReactMethod
    fun initialize(options: ReadableMap, promise: Promise) {
        scope.launch {
            try {
                client?.let { promise.resolve(describe(it)); return@launch }

                val dataDir = if (options.hasKey("dataDir")) options.getString("dataDir")!!
                              else File(reactContext.filesDir, "orgsync").absolutePath
                File(dataDir).mkdirs()

                val config = P2pConfig(
                    dataDir = dataDir,
                    enableMdns = if (options.hasKey("enableMdns")) options.getBoolean("enableMdns") else true,
                    enableRelay = if (options.hasKey("enableRelay")) options.getBoolean("enableRelay") else true,
                )
                val created = P2pClient(config)
                created.setListener(object : P2pListener {
                    override fun onEvent(eventJson: String) = emit(eventJson)
                })
                client = created
                promise.resolve(describe(created))
            } catch (e: Throwable) {
                Log.e(TAG, "initialize failed", e)
                promise.reject("p2p_error", e.message, e)
            }
            Unit
        }
    }

    private fun describe(c: P2pClient) = Arguments.createMap().apply {
        putString("peerId", c.peerId())
        putBoolean("enrolled", c.isEnrolled())
        putBoolean("running", c.isRunning())
        putString("dbPath", c.dbPath())
    }

    @ReactMethod
    fun beginEnrollment(inviteCode: String, deviceName: String, promise: Promise) =
        run(promise) { it.beginEnrollment(inviteCode, deviceName, "android") }

    @ReactMethod
    fun completeEnrollment(responseJson: String, promise: Promise) =
        run(promise) { it.completeEnrollment(responseJson) }

    @ReactMethod
    fun start(promise: Promise) = run(promise) { it.start(); true }

    @ReactMethod
    fun stop(promise: Promise) = run(promise) { it.stop(); true }

    @ReactMethod
    fun status(promise: Promise) = run(promise) { it.status() }

    @ReactMethod
    fun syncNow(promise: Promise) = run(promise) { it.syncNow(); true }

    @ReactMethod
    fun dial(multiaddr: String, promise: Promise) = run(promise) { it.dial(multiaddr); true }

    @ReactMethod
    fun query(sql: String, params: String, promise: Promise) = run(promise) { it.query(sql, params) }

    @ReactMethod
    fun execute(sql: String, params: String, promise: Promise) =
        run(promise) { it.execute(sql, params).toDouble() }

    @ReactMethod
    fun sendMessage(room: String, body: String, promise: Promise) =
        run(promise) { it.sendMessage(room, body) }

    @ReactMethod
    fun registerTable(table: String, pkColumn: String, promise: Promise) =
        run(promise) { it.registerTable(table, pkColumn); true }

    /// One place that moves off the main thread, checks initialisation, and
    /// turns a Rust error into a rejected promise.
    private fun run(promise: Promise, body: (P2pClient) -> Any) {
        scope.launch {
            val c = client
            if (c == null) {
                promise.reject("p2p_not_initialized", "call initialize() before anything else")
                return@launch
            }
            try {
                promise.resolve(body(c))
            } catch (e: Throwable) {
                Log.e(TAG, "native call failed", e)
                promise.reject("p2p_error", e.message, e)
            }
        }
    }

    private companion object {
        const val TAG = "OrgSyncNative"
    }
}

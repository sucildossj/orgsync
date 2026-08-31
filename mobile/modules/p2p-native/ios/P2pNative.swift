//  The iOS half of the bridge.
//
//  Everything of substance lives in Rust; this file only moves strings across
//  and pushes node events onto React Native's event emitter. Calls are
//  dispatched off the main thread because they touch SQLite.

import Foundation
import React

@objc(P2pNative)
final class P2pNative: RCTEventEmitter {

    private var client: P2pClient?
    private let queue = DispatchQueue(label: "org.orgsync.p2p", qos: .userInitiated)

    override static func requiresMainQueueSetup() -> Bool { false }

    override func supportedEvents() -> [String]! { ["p2p"] }

    /// Default location: Application Support, which is backed up but not
    /// visible to the user, and excluded from iCloud below.
    private static func defaultDataDir() -> String {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let dir = base.appendingPathComponent("orgsync", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        // The device key and replica are local state; restoring them onto a
        // second device would give two phones the same identity.
        var url = dir
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try? url.setResourceValues(values)
        return dir.path
    }

    // MARK: - lifecycle

    @objc(initialize:resolver:rejecter:)
    func initialize(_ options: NSDictionary,
                    resolver resolve: @escaping RCTPromiseResolveBlock,
                    rejecter reject: @escaping RCTPromiseRejectBlock) {
        queue.async { [weak self] in
            guard let self else { return }
            do {
                if let existing = self.client {
                    resolve(self.describe(existing))
                    return
                }
                let config = P2pConfig(
                    dataDir: (options["dataDir"] as? String) ?? Self.defaultDataDir(),
                    enableMdns: (options["enableMdns"] as? Bool) ?? true,
                    enableRelay: (options["enableRelay"] as? Bool) ?? true
                )
                let client = try P2pClient(config: config)
                client.setListener(listener: EventForwarder { [weak self] json in
                    self?.sendEvent(withName: "p2p", body: json)
                })
                self.client = client
                resolve(self.describe(client))
            } catch {
                reject("p2p_error", error.localizedDescription, error)
            }
        }
    }

    private func describe(_ c: P2pClient) -> [String: Any] {
        ["peerId": c.peerId(), "enrolled": c.isEnrolled(), "running": c.isRunning(), "dbPath": c.dbPath()]
    }

    // MARK: - enrolment

    @objc(beginEnrollment:deviceName:resolver:rejecter:)
    func beginEnrollment(_ inviteCode: String, deviceName: String,
                         resolver resolve: @escaping RCTPromiseResolveBlock,
                         rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.beginEnrollment(inviteCode: inviteCode, deviceName: deviceName, platform: "ios") }
    }

    @objc(completeEnrollment:resolver:rejecter:)
    func completeEnrollment(_ responseJson: String,
                            resolver resolve: @escaping RCTPromiseResolveBlock,
                            rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.completeEnrollment(responseJson: responseJson) }
    }

    // MARK: - node

    @objc(start:rejecter:)
    func start(_ resolve: @escaping RCTPromiseResolveBlock, rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.start(); return true }
    }

    @objc(stop:rejecter:)
    func stop(_ resolve: @escaping RCTPromiseResolveBlock, rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.stop(); return true }
    }

    @objc(status:rejecter:)
    func status(_ resolve: @escaping RCTPromiseResolveBlock, rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.status() }
    }

    @objc(syncNow:rejecter:)
    func syncNow(_ resolve: @escaping RCTPromiseResolveBlock, rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.syncNow(); return true }
    }

    @objc(dial:resolver:rejecter:)
    func dial(_ multiaddr: String,
              resolver resolve: @escaping RCTPromiseResolveBlock,
              rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.dial(multiaddr: multiaddr); return true }
    }

    // MARK: - data

    @objc(query:params:resolver:rejecter:)
    func query(_ sql: String, params: String,
               resolver resolve: @escaping RCTPromiseResolveBlock,
               rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.query(sql: sql, paramsJson: params) }
    }

    @objc(execute:params:resolver:rejecter:)
    func execute(_ sql: String, params: String,
                 resolver resolve: @escaping RCTPromiseResolveBlock,
                 rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { Int(try $0.execute(sql: sql, paramsJson: params)) }
    }

    @objc(sendMessage:body:resolver:rejecter:)
    func sendMessage(_ room: String, body: String,
                     resolver resolve: @escaping RCTPromiseResolveBlock,
                     rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.sendMessage(room: room, body: body) }
    }

    @objc(registerTable:pkColumn:resolver:rejecter:)
    func registerTable(_ table: String, pkColumn: String,
                       resolver resolve: @escaping RCTPromiseResolveBlock,
                       rejecter reject: @escaping RCTPromiseRejectBlock) {
        run(resolve, reject) { try $0.registerTable(table: table, pkColumn: pkColumn); return true }
    }

    // MARK: - plumbing

    /// One place that hops off the main thread, checks initialisation and
    /// turns a thrown Rust error into a rejected promise.
    private func run(_ resolve: @escaping RCTPromiseResolveBlock,
                     _ reject: @escaping RCTPromiseRejectBlock,
                     _ body: @escaping (P2pClient) throws -> Any) {
        queue.async { [weak self] in
            guard let client = self?.client else {
                reject("p2p_not_initialized", "call initialize() before anything else", nil)
                return
            }
            do { resolve(try body(client)) }
            catch { reject("p2p_error", error.localizedDescription, error) }
        }
    }
}

/// Adapts the Rust callback interface to a Swift closure.
private final class EventForwarder: P2pListener, @unchecked Sendable {
    private let sink: (String) -> Void
    init(_ sink: @escaping (String) -> Void) { self.sink = sink }
    func onEvent(eventJson: String) { sink(eventJson) }
}

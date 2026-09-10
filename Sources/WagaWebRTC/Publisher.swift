import Foundation

public protocol WagaPublisherDelegate: AnyObject {
    func wagaPublisherConnected()
    func wagaPublisherDisconnected()
    func wagaPublisherNeedsKeyframe()
    func wagaPublisherBitrateEstimate(_ bitrate: UInt64)
    func wagaPublisherFailed(_ message: String)
    func wagaPublisherDiagnostic(_ message: String)
}

public extension WagaPublisherDelegate {
    func wagaPublisherBitrateEstimate(_: UInt64) {}
    func wagaPublisherDiagnostic(_: String) {}
}

public final class WagaPublisher: @unchecked Sendable {
    public weak var delegate: (any WagaPublisherDelegate)?

    private let queue = DispatchQueue(label: "com.wagastrim.webrtc")
    private let core: WagaCore
    private let network: WagaNetwork
    private var timeout: DispatchSourceTimer?
    private var offerCompletion: (@Sendable (Result<String, Error>) -> Void)?
    private var offered = false
    private var stopped = false
    private var offerScheduled = false
    private let gatheringDelayMilliseconds: Int
    private let diagnostics: Bool
    private var lastDiagnostic: UInt64 = 0

    public init(
        audio: WagaCodec = .opus,
        video: WagaCodec = .h264,
        mode: WagaTransportMode = .standard,
        iceServers: [String] = [],
        connectionPriorities: WagaConnectionPriorities = .init(),
        targetBitrate: UInt64? = nil,
        diagnostics: Bool = false,
        delegate: (any WagaPublisherDelegate)? = nil
    ) throws {
        self.delegate = delegate
        self.diagnostics = diagnostics
        gatheringDelayMilliseconds = iceServers.isEmpty ? 150 : 1_000
        core = try WagaCore(audio: audio, video: video, targetBitrate: targetBitrate)
        network = WagaNetwork(
            queue: queue,
            bonding: mode == .bonded,
            iceServers: iceServers,
            connectionPriorities: connectionPriorities
        )
        network.onCandidate = { [weak self] candidate in
            self?.candidate(candidate)
        }
        network.onCandidateRemoved = { [weak self] candidate in
            guard let self, !stopped else { return }
            do {
                try core.removeLocalCandidate(candidate)
                drain()
            } catch {
                delegate?.wagaPublisherFailed(String(describing: error))
            }
        }
        network.onReceive = { [weak self] source, destination, data in
            self?.receive(source: source, destination: destination, data: data)
        }
        network.onPathCapacityChanged = { [weak self] in
            guard let self, !stopped else { return }
            do {
                try core.requestPathProbe()
                drain()
            } catch {
                delegate?.wagaPublisherFailed(String(describing: error))
            }
        }
        network.onTransmitPath = { [weak self] sequence, path in
            self?.core.setEgressPath(sequence: sequence, path: path)
        }
        network.onServerReflexiveCandidate = { [weak self] address, base in
            self?.serverReflexiveCandidate(address, base: base)
        }
    }

    public func createOffer(
        completion: @escaping @Sendable (Result<String, Error>) -> Void
    ) {
        queue.async {
            self.offerCompletion = completion
            self.network.start()
            self.queue.asyncAfter(deadline: .now() + 5) {
                guard let completion = self.offerCompletion else { return }
                self.offerCompletion = nil
                completion(.failure(WagaCoreError(description: "No usable cellular, Wi-Fi, or Ethernet interface")))
            }
        }
    }

    public func acceptAnswer(_ sdp: String) {
        queue.async {
            do {
                try self.core.acceptAnswer(sdp)
                self.network.setRemoteDescription(sdp)
                self.drain()
            } catch {
                self.delegate?.wagaPublisherFailed(String(describing: error))
            }
        }
    }

    public func send(codec: WagaCodec, mediaTime: UInt64, data: Data) {
        queue.async {
            guard !self.stopped else { return }
            do {
                try self.core.send(codec: codec, mediaTime: mediaTime, data: data)
                self.drain()
            } catch {
                self.delegate?.wagaPublisherFailed(String(describing: error))
            }
        }
    }

    public func setTargetBitrate(_ bitrate: UInt64) {
        queue.async {
            do {
                try self.core.setDesiredBitrate(bitrate)
                self.drain()
            } catch {
                self.delegate?.wagaPublisherFailed(String(describing: error))
            }
        }
    }

    public func stop() {
        queue.async {
            self.stopped = true
            self.timeout?.cancel()
            self.timeout = nil
            self.offerCompletion = nil
            self.network.stop()
        }
    }

    private func candidate(_ candidate: String) {
        guard !stopped else { return }
        do {
            try core.addLocalCandidate(candidate)
            if !offered, !offerScheduled, offerCompletion != nil {
                offerScheduled = true
                queue.asyncAfter(deadline: .now() + .milliseconds(gatheringDelayMilliseconds)) {
                    self.finishOffer()
                }
            }
            drain()
        } catch {
            offerCompletion?(.failure(error))
            offerCompletion = nil
            delegate?.wagaPublisherFailed(String(describing: error))
        }
    }

    private func serverReflexiveCandidate(_ candidate: String, base: String) {
        guard !stopped else { return }
        do {
            try core.addServerReflexiveCandidate(candidate, base: base)
            drain()
        } catch {
            delegate?.wagaPublisherFailed(String(describing: error))
        }
    }

    private func finishOffer() {
        guard !offered, let completion = offerCompletion else { return }
        do {
            offered = true
            offerCompletion = nil
            completion(.success(try core.createOffer()))
            drain()
        } catch {
            offerCompletion = nil
            completion(.failure(error))
            delegate?.wagaPublisherFailed(String(describing: error))
        }
    }

    private func receive(source: String, destination: String, data: Data) {
        guard !stopped else { return }
        do {
            try core.receive(source: source, destination: destination, data: data)
            drain()
        } catch {
            delegate?.wagaPublisherFailed(String(describing: error))
        }
    }

    private func drain() {
        guard !stopped else { return }
        while let datagram = core.pollTransmit() {
            network.send(datagram)
        }
        while let event = core.pollEvent() {
            switch event {
            case .connected:
                network.setConnected(true)
                delegate?.wagaPublisherConnected()
            case .disconnected, .closed:
                if diagnostics {
                    delegate?.wagaPublisherDiagnostic("transport_event=\(event)")
                }
                network.setConnected(false)
                delegate?.wagaPublisherDisconnected()
            case .keyframeRequest:
                delegate?.wagaPublisherNeedsKeyframe()
            }
        }
        while let estimate = core.pollBitrateEstimate() {
            delegate?.wagaPublisherBitrateEstimate(estimate)
        }
        if diagnostics {
            let now = DispatchTime.now().uptimeNanoseconds
            if now - lastDiagnostic >= 1_000_000_000 {
                lastDiagnostic = now
                if var snapshot = core.bweDiagnosticSnapshot() {
                    if let paths = network.diagnosticSnapshot() { snapshot += " \(paths)" }
                    delegate?.wagaPublisherDiagnostic(snapshot)
                }
            }
        }
        scheduleTimeout()
    }

    private func scheduleTimeout() {
        timeout?.cancel()
        let milliseconds = min(core.timeoutMilliseconds(), 60_000)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(Int(milliseconds)))
        timer.setEventHandler { [weak self] in
            guard let self, !stopped else { return }
            do {
                try core.handleTimeout()
                drain()
            } catch {
                delegate?.wagaPublisherFailed(String(describing: error))
            }
        }
        timeout = timer
        timer.resume()
    }
}

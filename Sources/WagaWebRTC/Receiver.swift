import Foundation

public protocol WagaReceiverDelegate: AnyObject {
    func wagaReceiverConnected()
    func wagaReceiverDisconnected()
    func wagaReceiverReceived(_ frame: WagaMediaFrame)
    func wagaReceiverFailed(_ message: String)
}

public final class WagaReceiver: @unchecked Sendable {
    public weak var delegate: (any WagaReceiverDelegate)?

    private enum Negotiation {
        case offer(@Sendable (Result<String, Error>) -> Void)
        case answer(String, @Sendable (Result<String, Error>) -> Void)
    }

    private let queue: DispatchQueue
    private let core: WagaCore
    private let network: WagaNetwork
    private var timer: DispatchSourceTimer?
    private var negotiation: Negotiation?
    private var negotiationScheduled = false
    private let gatheringDelayMilliseconds: Int

    public init(
        queue: DispatchQueue = DispatchQueue(label: "com.wagastrim.webrtc.receiver"),
        iceServers: [String] = [],
        delegate: (any WagaReceiverDelegate)? = nil
    ) throws {
        self.queue = queue
        self.delegate = delegate
        gatheringDelayMilliseconds = iceServers.isEmpty ? 150 : 1_000
        core = try WagaCore(receiver: ())
        network = WagaNetwork(queue: queue, bonding: false, iceServers: iceServers)
        network.onCandidate = { [weak self] candidate in self?.candidate(candidate) }
        network.onReceive = { [weak self] source, destination, data in
            self?.receive(source: source, destination: destination, data: data)
        }
        network.onServerReflexiveCandidate = { [weak self] address, base in
            self?.serverReflexiveCandidate(address, base: base)
        }
        network.onError = { [weak self] message in self?.delegate?.wagaReceiverFailed(message) }
    }

    public func createOffer(completion: @escaping @Sendable (Result<String, Error>) -> Void) {
        start(.offer(completion))
    }

    public func acceptOffer(
        _ sdp: String,
        completion: @escaping @Sendable (Result<String, Error>) -> Void
    ) {
        start(.answer(sdp, completion))
    }

    public func acceptAnswer(_ sdp: String) {
        queue.async {
            do {
                try self.core.acceptAnswer(sdp)
                self.drain()
            } catch {
                self.fail(error)
            }
        }
    }

    public func stop() {
        queue.async {
            self.timer?.cancel()
            self.timer = nil
            self.negotiation = nil
            self.network.stop()
        }
    }

    private func start(_ negotiation: Negotiation) {
        queue.async {
            self.negotiation = negotiation
            self.network.start()
            self.queue.asyncAfter(deadline: .now() + 5) {
                guard let negotiation = self.negotiation else { return }
                self.negotiation = nil
                let error = WagaCoreError(description: "No usable cellular, Wi-Fi, or Ethernet interface")
                switch negotiation {
                case let .offer(completion), let .answer(_, completion):
                    completion(.failure(error))
                }
            }
        }
    }

    private func candidate(_ candidate: String) {
        do {
            try core.addLocalCandidate(candidate)
            if !negotiationScheduled, negotiation != nil {
                negotiationScheduled = true
                queue.asyncAfter(deadline: .now() + .milliseconds(gatheringDelayMilliseconds)) {
                    self.finishNegotiation()
                }
            }
            drain()
        } catch {
            fail(error)
        }
    }

    private func serverReflexiveCandidate(_ candidate: String, base: String) {
        do {
            try core.addServerReflexiveCandidate(candidate, base: base)
            drain()
        } catch {
            fail(error)
        }
    }

    private func finishNegotiation() {
        guard let negotiation else { return }
        self.negotiation = nil
        do {
            switch negotiation {
            case let .offer(completion):
                completion(.success(try core.createReceiveOffer()))
            case let .answer(sdp, completion):
                completion(.success(try core.acceptOffer(sdp)))
            }
            drain()
        } catch {
            switch negotiation {
            case let .offer(completion), let .answer(_, completion):
                completion(.failure(error))
            }
            fail(error)
        }
    }

    private func receive(source: String, destination: String, data: Data) {
        do {
            try core.receive(source: source, destination: destination, data: data)
            drain()
        } catch {
            fail(error)
        }
    }

    private func drain() {
        while let datagram = core.pollTransmit() { network.send(datagram) }
        while let frame = core.pollMedia() { delegate?.wagaReceiverReceived(frame) }
        while let event = core.pollEvent() {
            switch event {
            case .connected:
                network.setConnected(true)
                delegate?.wagaReceiverConnected()
            case .disconnected, .closed:
                network.setConnected(false)
                delegate?.wagaReceiverDisconnected()
            case .keyframeRequest:
                break
            }
        }
        scheduleTimeout()
    }

    private func scheduleTimeout() {
        timer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(Int(min(core.timeoutMilliseconds(), 60_000))))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            do {
                try core.handleTimeout()
                drain()
            } catch {
                fail(error)
            }
        }
        self.timer = timer
        timer.resume()
    }

    private func fail(_ error: Error) {
        delegate?.wagaReceiverFailed(String(describing: error))
    }
}

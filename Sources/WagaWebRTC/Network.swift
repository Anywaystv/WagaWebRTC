import Darwin
import Foundation
import Network

private let supportedInterfaces: Set<NWInterface.InterfaceType> = [.cellular, .wifi, .wiredEthernet]

final class WagaNetwork: @unchecked Sendable {
    var onCandidate: ((String) -> Void)?
    var onServerReflexiveCandidate: ((String, String) -> Void)?
    var onReceive: ((String, String, Data) -> Void)?
    var onError: ((String) -> Void)?

    private let queue: DispatchQueue
    private let monitor = NWPathMonitor()
    private var paths: [String: WagaPath] = [:]
    private var scheduler = WagaPathScheduler()
    private var recovery = WagaRecovery()
    private var connected = false
    private var remoteIcePassword: String?
    private let bonding: Bool
    private let stunServers: [String]
    private let connectionPriorities: WagaConnectionPriorities

    init(
        queue: DispatchQueue,
        bonding: Bool,
        iceServers: [String],
        connectionPriorities: WagaConnectionPriorities = .init()
    ) {
        self.queue = queue
        self.bonding = bonding
        stunServers = iceServers.compactMap(stunEndpoint)
        self.connectionPriorities = connectionPriorities
    }

    func start() {
        monitor.pathUpdateHandler = { [weak self] path in
            self?.update(path)
        }
        monitor.start(queue: queue)
    }

    func stop() {
        monitor.cancel()
        paths.values.forEach { $0.stop() }
        paths.removeAll()
        scheduler.replace([])
        recovery.removeAll()
    }

    func setConnected(_ connected: Bool) {
        self.connected = connected
    }

    func setRemoteDescription(_ sdp: String) {
        remoteIcePassword = sdp.split(whereSeparator: \.isNewline)
            .first { $0.hasPrefix("a=ice-pwd:") }.map { String($0.dropFirst(10)) }
        for path in paths.values {
            path.remoteIcePassword = remoteIcePassword
        }
    }

    func send(_ datagram: WagaDatagram) {
        if bonding, connected, isRtp(datagram.data) {
            let parity = recovery.record(datagram.data)
            sendBest(datagram.data, to: datagram.destination, fallback: datagram.source)
            if let parity {
                sendBest(parity, to: datagram.destination, fallback: datagram.source)
            }
        } else {
            paths[datagram.source]?.send(datagram.data, to: datagram.destination)
        }
    }

    private func update(_ path: NWPath) {
        let interfaces = path.availableInterfaces.filter { supportedInterfaces.contains($0.type) }
        var active = Set<String>()
        for interface in interfaces {
            for address in addresses(interface.name) {
                let id = "\(interface.name)|\(address)"
                active.insert(id)
                guard !paths.values.contains(where: { $0.id == id }) else {
                    continue
                }
                let path = WagaPath(
                    interface: interface,
                    address: address,
                    queue: queue,
                    stunServers: stunServers,
                    bonding: bonding
                )
                path.remoteIcePassword = remoteIcePassword
                path.onReady = { [weak self, weak path] candidate in
                    guard let self, let path else { return }
                    self.paths.removeValue(forKey: id)
                    self.paths[candidate] = path
                    self.onCandidate?(candidate)
                }
                path.onReceive = { [weak self] source, destination, data in
                    self?.receive(source: source, destination: destination, data: data)
                }
                path.onServerReflexiveCandidate = { [weak self] address, base in
                    self?.onServerReflexiveCandidate?(address, base)
                }
                path.onError = { [weak self] message in
                    self?.onError?(message)
                }
                paths[id] = path
                path.start()
            }
        }
        let removed = paths.filter { !active.contains($0.value.id) }.map(\.key)
        for id in removed {
            paths.removeValue(forKey: id)?.stop()
        }
    }

    private func receive(source: String, destination: String, data: Data) {
        let trustedSource = paths.values.contains { $0.isValidated(source) }
        if bonding, trustedSource, let repairs = recovery.repairs(for: data) {
            for packet in repairs {
                sendBest(packet, to: source, fallback: destination)
            }
            return
        }
        onReceive?(source, destination, data)
    }

    private func sendBest(_ data: Data, to destination: String, fallback: String) {
        if refreshScheduler(destination: destination),
           let id = scheduler.select(byteCount: data.count),
           let path = paths[id] {
            path.send(data, to: destination)
        } else {
            if let path = paths[fallback], path.isValidated(destination) {
                path.send(data, to: destination)
            }
        }
    }

    @discardableResult
    private func refreshScheduler(destination: String) -> Bool {
        let bestByInterface = Dictionary(grouping: paths.filter {
            $0.value.isValidated(destination)
        }) {
            $0.value.interface.name
        }.compactMap { _, entries in
            entries.min {
                ($0.value.smoothedRttMilliseconds ?? .greatestFiniteMagnitude)
                    < ($1.value.smoothedRttMilliseconds ?? .greatestFiniteMagnitude)
            }
        }
        scheduler.replace(bestByInterface.map { id, path in
            return WagaPathScore(
                id: id,
                priority: priority(for: path.interface.type),
                smoothedRttMilliseconds: path.smoothedRttMilliseconds,
                pendingBytes: path.pendingBytes
            )
        })
        return !bestByInterface.isEmpty
    }

    private func priority(for type: NWInterface.InterfaceType) -> Double {
        switch type {
        case .cellular:
            connectionPriorities.cellular
        case .wiredEthernet:
            connectionPriorities.wiredEthernet
        default:
            connectionPriorities.wifi
        }
    }
}

private final class WagaPath: @unchecked Sendable {
    var remoteIcePassword: String?
    var onReady: ((String) -> Void)?
    var onReceive: ((String, String, Data) -> Void)?
    var onServerReflexiveCandidate: ((String, String) -> Void)?
    var onError: ((String) -> Void)?

    let interface: NWInterface
    let id: String
    private let address: String
    private let queue: DispatchQueue
    private var listener: NWListener?
    private var connections: [String: NWConnection] = [:]
    private var pending: [String: [Data]] = [:]
    private var stunSent: [Data: UInt64] = [:]
    private var gatheringTransactions = Set<Data>()
    private(set) var port: NWEndpoint.Port?
    private var health = WagaPathHealth()
    private var probes: [String: Data] = [:]
    private var probeTimer: DispatchSourceTimer?
    private var probeRequests: [Data: (destination: String, sent: UInt64)] = [:]
    private let bonding: Bool
    private(set) var smoothedRttMilliseconds: Double?
    private(set) var pendingBytes = 0
    private let stunServers: [String]

    init(interface: NWInterface, address: String, queue: DispatchQueue, stunServers: [String] = [], bonding: Bool = false) {
        self.interface = interface
        id = "\(interface.name)|\(address)"
        self.address = address
        self.queue = queue
        self.stunServers = stunServers
        self.bonding = bonding
    }

    func start() {
        do {
            let parameters = NWParameters.udp
            parameters.requiredInterface = interface
            parameters.requiredLocalEndpoint = .hostPort(host: .init(address), port: .any)
            parameters.allowLocalEndpointReuse = true
            let listener = try NWListener(using: parameters)
            listener.stateUpdateHandler = { [weak self] state in
                guard let self else { return }
                switch state {
                case .ready:
                    port = listener.port
                    if let port {
                        onReady?(makeSocketAddress(address, port.rawValue))
                        gatherServerReflexiveCandidates()
                    }
                case let .failed(error):
                    onError?("\(interface.name): \(error)")
                default:
                    break
                }
            }
            listener.newConnectionHandler = { [weak self] connection in
                self?.startIncoming(connection)
            }
            self.listener = listener
            listener.start(queue: queue)
        } catch {
            onError?("\(interface.name): \(error)")
        }
    }

    func stop() {
        probeTimer?.cancel()
        probeTimer = nil
        probes.removeAll()
        probeRequests.removeAll()
        health = WagaPathHealth()
        listener?.cancel()
        listener = nil
        connections.values.forEach { $0.forceCancel() }
        connections.removeAll()
        pending.removeAll()
    }

    func send(_ data: Data, to destination: String) {
        guard let endpoint = endpoint(destination), let port else {
            return
        }
        if let transaction = stunTransaction(data) {
            if data[0] == 0 && data[1] == 1 {
                stunSent[transaction] = DispatchTime.now().uptimeNanoseconds
                if bonding, !gatheringTransactions.contains(transaction) {
                    probes[destination] = data
                    startProbes()
                }
            }
        }
        if let connection = connections[destination], connection.state == .ready {
            send(data, on: connection)
            return
        }
        pending[destination, default: []].append(data)
        guard connections[destination] == nil else {
            return
        }
        let parameters = NWParameters.udp
        parameters.requiredInterface = interface
        parameters.requiredLocalEndpoint = .hostPort(host: .init(address), port: port)
        parameters.allowLocalEndpointReuse = true
        parameters.prohibitExpensivePaths = false
        let connection = NWConnection(to: endpoint, using: parameters)
        connections[destination] = connection
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let self, let connection else { return }
            switch state {
            case .ready:
                connection.batch {
                    for data in pending.removeValue(forKey: destination) ?? [] {
                        self.send(data, on: connection)
                    }
                }
                receive(connection)
            case let .failed(error):
                health.invalidate(destination)
                connections.removeValue(forKey: destination)
                pending.removeValue(forKey: destination)
                onError?("\(interface.name): \(error)")
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    private func send(_ data: Data, on connection: NWConnection) {
        pendingBytes += data.count
        connection.send(content: data, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            pendingBytes = max(0, pendingBytes - data.count)
            if let error {
                health.invalidate(connection.endpoint.socketAddress)
                onError?("\(interface.name): \(error)")
            }
        })
    }

    private func startIncoming(_ connection: NWConnection) {
        let key = connection.endpoint.socketAddress
        connections[key] = connection
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            if case .failed = state {
                health.invalidate(key)
                connections.removeValue(forKey: key)
            } else if case .cancelled = state {
                health.invalidate(key)
                connections.removeValue(forKey: key)
            }
        }
        connection.start(queue: queue)
        receive(connection)
    }

    private func receive(_ connection: NWConnection) {
        connection.receiveMessage { [weak self, weak connection] data, _, _, error in
            guard let self, let connection else { return }
            if let data, !data.isEmpty, let port {
                let source = connection.endpoint.socketAddress
                if let candidate = consumeGatheringResponse(data) {
                    onServerReflexiveCandidate?(
                        candidate,
                        makeSocketAddress(address, port.rawValue)
                    )
                } else if !consumeProbeResponse(data, source: source) {
                    updateRtt(data, source: source)
                    onReceive?(source, makeSocketAddress(address, port.rawValue), data)
                }
            }
            if let error {
                health.invalidate(connection.endpoint.socketAddress)
                onError?("\(interface.name): \(error)")
            } else {
                receive(connection)
            }
        }
    }

    func isValidated(_ destination: String) -> Bool {
        health.isLive(destination, now: DispatchTime.now().uptimeNanoseconds)
    }

    private func updateRtt(_ data: Data, source: String) {
        guard isStunSuccess(data), let transaction = stunTransaction(data),
              let sent = stunSent.removeValue(forKey: transaction)
        else {
            return
        }
        health.succeeded(source, now: DispatchTime.now().uptimeNanoseconds)
        let sample = Double(DispatchTime.now().uptimeNanoseconds - sent) / 1_000_000
        smoothedRttMilliseconds = smoothedRttMilliseconds.map { $0 * 0.8 + sample * 0.2 } ?? sample
    }

    private func startProbes() {
        guard probeTimer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(500), repeating: .milliseconds(500))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let now = DispatchTime.now().uptimeNanoseconds
            stunSent = stunSent.filter { now - $0.value < WagaPathHealth.lifetime }
            probeRequests = probeRequests.filter { now - $0.value.sent < WagaPathHealth.lifetime }
            guard let remoteIcePassword else { return }
            // Fresh signed transactions keep delayed replies from refreshing a
            // dead path or making its measured RTT artificially low.
            for (destination, template) in probes {
                guard let connection = connections[destination], connection.state == .ready,
                      let request = makeIceProbe(template, password: remoteIcePassword),
                      let transaction = stunTransaction(request) else { continue }
                probeRequests[transaction] = (destination, now)
                send(request, on: connection)
            }
        }
        probeTimer = timer
        timer.resume()
    }

    private func consumeProbeResponse(_ data: Data, source: String) -> Bool {
        guard let transaction = stunTransaction(data), let probe = probeRequests[transaction] else { return false }
        guard probe.destination == source, let remoteIcePassword,
              isStunSuccess(data), validIceIntegrity(data, password: remoteIcePassword) else { return true }
        probeRequests.removeValue(forKey: transaction)
        let now = DispatchTime.now().uptimeNanoseconds
        guard now - probe.sent < WagaPathHealth.lifetime else { return true }
        health.succeeded(source, now: now)
        let sample = Double(now - probe.sent) / 1_000_000
        smoothedRttMilliseconds = smoothedRttMilliseconds.map { $0 * 0.8 + sample * 0.2 } ?? sample
        return true
    }

    private func gatherServerReflexiveCandidates() {
        for server in stunServers {
            let request = makeStunBindingRequest()
            if let transaction = stunTransaction(request) {
                gatheringTransactions.insert(transaction)
            }
            send(request, to: server)
        }
    }

    private func consumeGatheringResponse(_ data: Data) -> String? {
        guard let transaction = stunTransaction(data),
              gatheringTransactions.remove(transaction) != nil
        else {
            return nil
        }
        stunSent.removeValue(forKey: transaction)
        return xorMappedAddress(data)
    }
}

private extension NWEndpoint {
    var socketAddress: String {
        guard case let .hostPort(host, port) = self else { return String(describing: self) }
        let address = String(describing: host).split(separator: "%", maxSplits: 1).first
            .map(String.init) ?? String(describing: host)
        return makeSocketAddress(address, port.rawValue)
    }
}

private func endpoint(_ address: String) -> NWEndpoint? {
    if address.first == "[", let close = address.firstIndex(of: "]"),
       let port = UInt16(address[address.index(close, offsetBy: 2)...]) {
        return .hostPort(host: .init(String(address[address.index(after: address.startIndex) ..< close])),
                         port: .init(rawValue: port)!)
    }
    guard let split = address.lastIndex(of: ":"), let port = UInt16(address[address.index(after: split)...]) else {
        return nil
    }
    return .hostPort(host: .init(String(address[..<split])), port: .init(rawValue: port)!)
}

func makeSocketAddress(_ address: String, _ port: UInt16) -> String {
    address.contains(":") ? "[\(address)]:\(port)" : "\(address):\(port)"
}

private func addresses(_ name: String) -> [String] {
    var first: UnsafeMutablePointer<ifaddrs>?
    guard getifaddrs(&first) == 0, let first else { return [] }
    defer { freeifaddrs(first) }
    var values: [String] = []
    var current: UnsafeMutablePointer<ifaddrs>? = first
    while let interface = current?.pointee {
        defer { current = interface.ifa_next }
        guard String(cString: interface.ifa_name) == name, let address = interface.ifa_addr,
              address.pointee.sa_family == UInt8(AF_INET) || address.pointee.sa_family == UInt8(AF_INET6)
        else {
            continue
        }
        var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        guard getnameinfo(
            address,
            socklen_t(address.pointee.sa_len),
            &host,
            socklen_t(host.count),
            nil,
            0,
            NI_NUMERICHOST
        ) == 0 else {
            continue
        }
        let value = String(bytes: host.prefix { $0 != 0 }.map(UInt8.init(bitPattern:)), encoding: .utf8)?
            .split(separator: "%").first.map(String.init) ?? ""
        if !value.isEmpty && !value.hasPrefix("fe80:") {
            values.append(value)
        }
    }
    return Array(Set(values)).sorted()
}

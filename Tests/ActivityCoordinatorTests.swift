import Combine
import XCTest
@testable import Codenotch

@MainActor
final class ActivityCoordinatorTests: XCTestCase {
    private final class Monitor: AgentActivityMonitor {
        @Published var sessions: [AgentSession] = []
        var sessionsPublisher: AnyPublisher<[AgentSession], Never> { $sessions.eraseToAnyPublisher() }
        var starts = 0
        var stops = 0
        func start() { starts += 1 }
        func stop() { stops += 1 }
    }

    func testOnlyConnectedMonitorsRunAndReapplyingDoesNotRestartThem() {
        let a = Monitor(), b = Monitor()
        let coordinator = ActivityCoordinator(monitors: ["a": a, "b": b]) { _, _ in }
        coordinator.setEnabled(["a", "unknown"])
        coordinator.setEnabled(["a"])
        XCTAssertEqual(a.starts, 1)
        XCTAssertEqual(b.starts, 0)
        XCTAssertEqual(coordinator.activeIDs, ["a"])
        coordinator.setEnabled(["b"])
        XCTAssertEqual(a.stops, 1)
        XCTAssertEqual(b.starts, 1)
        coordinator.stop()
        coordinator.stop()
        XCTAssertEqual(b.stops, 1)
    }

    func testDisconnectClearsSessionsAndIgnoresLateUpdatesAndBusyState() async throws {
        let monitor = Monitor()
        var delivered: [[AgentSession]] = []
        let coordinator = ActivityCoordinator(monitors: ["a": monitor]) { _, sessions in
            delivered.append(sessions)
        }
        coordinator.setEnabled(["a"])
        monitor.sessions = [AgentSession(id: "work", name: "Work", detail: "Working",
                                         state: .busy, waitingFor: nil, since: Date())]
        XCTAssertTrue(coordinator.isBusy)
        coordinator.setEnabled([])
        let countAfterDisconnect = delivered.count
        XCTAssertEqual(delivered.last, [])
        XCTAssertFalse(coordinator.isBusy)
        // Even a buggy/stopped monitor that publishes again cannot update the UI.
        monitor.sessions = monitor.sessions
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(delivered.count, countAfterDisconnect)
    }

    func testReconnectResubscribesAndPublishesAgain() async {
        let monitor = Monitor()
        let received = expectation(description: "Reconnected activity reaches the notch")
        let coordinator = ActivityCoordinator(monitors: ["a": monitor]) { _, sessions in
            if sessions.first?.id == "new" { received.fulfill() }
        }
        defer { coordinator.stop() }
        coordinator.setEnabled(["a"])
        coordinator.stop()
        coordinator.setEnabled(["a"])
        monitor.sessions = [AgentSession(id: "new", name: "New", detail: "Working",
                                         state: .busy, waitingFor: nil, since: Date())]
        await fulfillment(of: [received], timeout: 1)
        XCTAssertEqual(monitor.starts, 2)
    }
}

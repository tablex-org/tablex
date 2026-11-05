import AppKit
import ApplicationServices
import CoreGraphics
import Darwin
import Foundation

let allowedScenarioTitles: Set<String> = [
    "TableX UI Scenario: new-connection",
    "TableX UI Scenario: connection-tree",
    "TableX UI Scenario: connection-overview",
    "TableX UI Scenario: result-grid",
    "TableX UI Scenario: result-footer",
    "TableX UI Scenario: mongo-grid",
    "TableX UI Scenario: table-diagram",
    "TableX UI Scenario: database-diagram",
    "TableX UI Scenario: table-properties-edit",
    "TableX UI Scenario: driver-manager",
    "TableX UI Scenario: driver-manager-tools",
    "TableX UI Scenario: missing-driver",
    "TableX UI Scenario: split-panes",
    "TableX UI Scenario: split-panes-drop",
    "TableX UI Scenario: grouped-tabs",
    "TableX UI Scenario: grouped-tabs-lone",
    "TableX UI Scenario: settings",
    "TableX UI Scenario: settings-shortcuts",
    "TableX UI Scenario: connection-overrides",
    "TableX UI Scenario: import-selection",
    "TableX UI Scenario: redis",
    "TableX UI Scenario: settings-ai",
    "TableX UI Scenario: settings-ai-ollama",
    "TableX UI Scenario: assistant",
    "TableX UI Scenario: assistant-streaming",
    "TableX UI Scenario: assistant-no-key",
    "TableX UI Scenario: assistant-access-off",
    "TableX UI Scenario: assistant-error",
    "TableX UI Scenario: assistant-approval",
    "TableX UI Scenario: assistant-approval-dangerous",
    "TableX UI Scenario: assistant-approval-long",
    "TableX UI Scenario: assistant-access-write",
    "TableX UI Scenario: assistant-history",
    "TableX UI Scenario: assistant-readonly",
    "TableX UI Scenario: assistant-editor-action",
]

struct HelperError: Error {
    let message: String
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(2)
}

func emit(_ value: Any) {
    guard JSONSerialization.isValidJSONObject(value),
          let data = try? JSONSerialization.data(withJSONObject: value),
          let text = String(data: data, encoding: .utf8) else {
        fail("helper could not encode JSON")
    }
    print(text)
}

func canonicalPath(_ path: String) -> String {
    URL(fileURLWithPath: path).resolvingSymlinksInPath().standardizedFileURL.path
}

func environment() -> [String: Any] {
    let session = CGSessionCopyCurrentDictionary() as? [String: Any]
    return [
        "desktop_available": session != nil,
        "locked": (session?["CGSSessionScreenIsLocked"] as? NSNumber)?.boolValue ?? false,
        "capture_allowed": CGPreflightScreenCaptureAccess(),
        "input_allowed": CGPreflightPostEventAccess() && AXIsProcessTrusted(),
    ]
}

func processPath(_ pid: pid_t) -> String? {
    var buffer = [CChar](repeating: 0, count: 4096)
    let length = proc_pidpath(pid, &buffer, UInt32(buffer.count))
    guard length > 0 else { return nil }
    return String(cString: buffer)
}

func processInfo(_ pid: pid_t) -> [String: Any] {
    guard kill(pid, 0) == 0 || errno == EPERM else {
        return ["pid": pid, "alive": false]
    }
    var result: [String: Any] = ["pid": pid, "alive": true]
    if let path = processPath(pid) {
        result["path"] = canonicalPath(path)
    }
    if let application = NSRunningApplication(processIdentifier: pid),
       let launchDate = application.launchDate {
        result["launch_time"] = launchDate.timeIntervalSince1970
    }
    return result
}

struct WindowTarget {
    let id: CGWindowID
    let x: Double
    let y: Double
    let width: Double
    let height: Double
}

func number(_ dictionary: NSDictionary, _ key: String) -> Double {
    (dictionary[key] as? NSNumber)?.doubleValue ?? 0
}

func windows(pid: pid_t, expectedPath: String) -> [[String: Any]] {
    guard let actualPath = processPath(pid),
          canonicalPath(actualPath) == canonicalPath(expectedPath) else {
        return []
    }
    guard let raw = CGWindowListCopyWindowInfo(
        [.optionOnScreenOnly, .excludeDesktopElements],
        kCGNullWindowID
    ) as? [[String: Any]] else {
        return []
    }
    return raw.compactMap { entry in
        guard (entry[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == pid,
              let id = (entry[kCGWindowNumber as String] as? NSNumber)?.uint32Value,
              let rawBounds = entry[kCGWindowBounds as String] as? NSDictionary else {
            return nil
        }
        let bounds: [String: Any] = [
            "x": number(rawBounds, "X"),
            "y": number(rawBounds, "Y"),
            "width": number(rawBounds, "Width"),
            "height": number(rawBounds, "Height"),
        ]
        return [
            "id": id,
            "pid": pid,
            "title": (entry[kCGWindowName as String] as? String) ?? "",
            "owner": (entry[kCGWindowOwnerName as String] as? String) ?? "",
            "layer": (entry[kCGWindowLayer as String] as? NSNumber)?.intValue ?? 0,
            "bounds": bounds,
        ]
    }
}

func target(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String) throws -> WindowTarget {
    guard allowedScenarioTitles.contains(expectedTitle) else {
        throw HelperError(message: "unsupported UI scenario window title")
    }
    let desktop = environment()
    guard desktop["desktop_available"] as? Bool == true, desktop["locked"] as? Bool == false else {
        throw HelperError(message: "desktop is locked or unavailable; unlock macOS before UI automation")
    }
    guard let entry = windows(pid: pid, expectedPath: expectedPath).first(where: {
        ($0["id"] as? UInt32) == id
    }), entry["title"] as? String == expectedTitle,
              let bounds = entry["bounds"] as? [String: Any] else {
        throw HelperError(message: "owned window was not found")
    }
    return WindowTarget(
        id: id,
        x: (bounds["x"] as? Double) ?? 0,
        y: (bounds["y"] as? Double) ?? 0,
        width: (bounds["width"] as? Double) ?? 0,
        height: (bounds["height"] as? Double) ?? 0
    )
}

func hasScenarioTitle(_ element: AXUIElement, expectedTitle: String) -> Bool {
    var title: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, kAXTitleAttribute as CFString, &title) == .success
        && title as? String == expectedTitle
}

func activate(_ pid: pid_t, window: WindowTarget, expectedTitle: String) throws {
    guard let application = NSRunningApplication(processIdentifier: pid) else {
        throw HelperError(message: "owned process is not a running application")
    }
    _ = application.activate(options: [.activateAllWindows])
    let element = AXUIElementCreateApplication(pid)
    let frontmostResult = AXUIElementSetAttributeValue(element, kAXFrontmostAttribute as CFString, kCFBooleanTrue)
    if frontmostResult != .success {
        throw HelperError(message: "could not activate owned application; accessibility permission is required")
    }
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, kAXWindowsAttribute as CFString, &value) == .success,
          let appWindows = value as? [AXUIElement] else {
        throw HelperError(message: "owned window is not exposed to Accessibility")
    }
    let candidates = appWindows.filter { hasScenarioTitle($0, expectedTitle: expectedTitle) }
    let cgWindows = (CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []).filter {
        ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == pid
            && $0[kCGWindowName as String] as? String == expectedTitle
    }
    guard candidates.count == 1, let selected = candidates.first,
          cgWindows.count == 1,
          (cgWindows.first?[kCGWindowNumber as String] as? NSNumber)?.uint32Value == window.id else {
        throw HelperError(message: "owned Accessibility window cannot be identified unambiguously")
    }
    guard AXUIElementSetAttributeValue(selected, kAXMainAttribute as CFString, kCFBooleanTrue) == .success,
          AXUIElementPerformAction(selected, kAXRaiseAction as CFString) == .success else {
        throw HelperError(message: "could not focus the owned window")
    }
    let deadline = Date(timeIntervalSinceNow: 2)
    while Date() < deadline {
        var focused: CFTypeRef?
        if AXUIElementCopyAttributeValue(element, kAXFocusedWindowAttribute as CFString, &focused) == .success,
           let focused, CFEqual(focused, selected) {
            return
        }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.01))
    }
    throw HelperError(message: "owned window did not receive focus")
}

func eventSource() throws -> CGEventSource {
    guard let source = CGEventSource(stateID: .hidSystemState) else {
        throw HelperError(message: "could not create HID event source")
    }
    return source
}

func drainPostedEvents(short: Bool = false) {
    // postToPid queues delivery through this process's run loop. Exiting immediately drops it.
    // This is transport draining, not evidence that the UI handled or rendered the action.
    RunLoop.current.run(until: Date(timeIntervalSinceNow: short ? 0.01 : 0.25))
}

func associate(_ event: CGEvent, with window: WindowTarget, clickState: Int64 = 1) {
    let windowID = Int64(window.id)
    event.setIntegerValueField(.mouseEventWindowUnderMousePointer, value: windowID)
    event.setIntegerValueField(.mouseEventWindowUnderMousePointerThatCanHandleThisEvent, value: windowID)
    event.setIntegerValueField(.mouseEventClickState, value: clickState)
}

func click(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String, x: Double, y: Double, count: Int = 1, modifierFlags: NSEvent.ModifierFlags = []) throws {
    guard count == 1 || count == 2 else { throw HelperError(message: "click count must be one or two") }
    let initialWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    guard x >= 0, y >= 0, x <= initialWindow.width, y <= initialWindow.height else {
        throw HelperError(message: "click is outside the owned window bounds")
    }
    try activate(pid, window: initialWindow, expectedTitle: expectedTitle)
    let window = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    guard x <= window.width, y <= window.height else {
        throw HelperError(message: "owned window changed before click")
    }
    // NSEvent.cgEvent flips against the primary screen even for another process's window.
    // Supply the inverse so the targeted event carries window-local, top-left coordinates.
    let point = NSPoint(x: x, y: CGDisplayBounds(CGMainDisplayID()).height - y)
    let timestamp = ProcessInfo.processInfo.systemUptime
    // PID-targeted CG mouse events otherwise arrive at AppKit with windowNumber zero.
    for current in 1...count {
        let stamp = timestamp + Double(current - 1) * 0.08
        guard let down = NSEvent.mouseEvent(with: .leftMouseDown, location: point, modifierFlags: modifierFlags, timestamp: stamp, windowNumber: Int(id), context: nil, eventNumber: current * 2 - 1, clickCount: current, pressure: 1)?.cgEvent,
              let up = NSEvent.mouseEvent(with: .leftMouseUp, location: point, modifierFlags: modifierFlags, timestamp: stamp + 0.02, windowNumber: Int(id), context: nil, eventNumber: current * 2, clickCount: current, pressure: 0)?.cgEvent else {
            throw HelperError(message: "could not create mouse event")
        }
        // The click state must carry the click number, otherwise AppKit reports
        // clickCount 1 for the second click of a double-click.
        associate(down, with: window, clickState: Int64(current))
        associate(up, with: window, clickState: Int64(current))
        down.postToPid(pid)
        up.postToPid(pid)
        if current < count { usleep(80_000) }
    }
    drainPostedEvents()
}

func mouseEvent(
    type: NSEvent.EventType,
    location: NSPoint,
    windowID: CGWindowID,
    eventNumber: Int,
    clickCount: Int,
    pressure: Float
) throws -> CGEvent {
    guard let event = NSEvent.mouseEvent(
        with: type,
        location: location,
        modifierFlags: [],
        timestamp: ProcessInfo.processInfo.systemUptime,
        windowNumber: Int(windowID),
        context: nil,
        eventNumber: eventNumber,
        clickCount: clickCount,
        pressure: pressure
    )?.cgEvent else {
        throw HelperError(message: "could not create mouse event")
    }
    event.setIntegerValueField(.mouseEventButtonNumber, value: type == .rightMouseDown || type == .rightMouseUp ? 1 : 0)
    return event
}

func localPoint(x: Double, y: Double) -> NSPoint {
    NSPoint(x: x, y: CGDisplayBounds(CGMainDisplayID()).height - y)
}

func validatePoint(_ x: Double, _ y: Double, in window: WindowTarget, name: String = "mouse input") throws {
    guard x >= 0, y >= 0, x <= window.width, y <= window.height else {
        throw HelperError(message: "\(name) is outside the owned window bounds")
    }
}

func rightClick(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String, x: Double, y: Double) throws {
    let initialWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try validatePoint(x, y, in: initialWindow, name: "right-click")
    try activate(pid, window: initialWindow, expectedTitle: expectedTitle)
    let window = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try validatePoint(x, y, in: window, name: "right-click")
    let point = localPoint(x: x, y: y)
    let down = try mouseEvent(type: .rightMouseDown, location: point, windowID: id, eventNumber: 1, clickCount: 1, pressure: 1)
    let up = try mouseEvent(type: .rightMouseUp, location: point, windowID: id, eventNumber: 2, clickCount: 1, pressure: 0)
    associate(down, with: window)
    associate(up, with: window)
    down.postToPid(pid)
    up.postToPid(pid)
    drainPostedEvents()
}

func mouseMoveTrace(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String) throws {
    let payload = try JSONSerialization.jsonObject(with: standardInputData())
    guard let object = payload as? [String: Any],
          let rawPoints = object["points"] as? [[NSNumber]],
          !rawPoints.isEmpty,
          rawPoints.count <= 4096,
          let rawHz = object["hz"] as? NSNumber,
          rawHz.doubleValue.isFinite,
          rawHz.doubleValue >= 1,
          rawHz.doubleValue <= 120,
          rawHz.doubleValue.rounded() == rawHz.doubleValue,
          rawPoints.allSatisfy({ $0.count == 2 && $0.allSatisfy({ $0.doubleValue.isFinite }) }) else {
        throw HelperError(message: "mouse-move-trace requires 1-4096 finite points and an integer cadence from 1 to 120 Hz")
    }

    let initialWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    let points = rawPoints.map { (x: $0[0].doubleValue, y: $0[1].doubleValue) }
    for point in points {
        try validatePoint(point.x, point.y, in: initialWindow, name: "mouse move")
    }
    try activate(pid, window: initialWindow, expectedTitle: expectedTitle)
    let window = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    for point in points {
        try validatePoint(point.x, point.y, in: window, name: "mouse move")
    }

    let hz = rawHz.doubleValue
    let period = 1.0 / hz
    let start = ProcessInfo.processInfo.systemUptime
    var sentUptimeNs: [UInt64] = []
    sentUptimeNs.reserveCapacity(points.count)
    var maximumLatenessNs: UInt64 = 0
    for (index, point) in points.enumerated() {
        let scheduled = start + Double(index) * period
        while true {
            let remaining = scheduled - ProcessInfo.processInfo.systemUptime
            if remaining <= 0 { break }
            Thread.sleep(forTimeInterval: remaining)
        }
        let cursorPosition = CGPoint(x: window.x + point.x, y: window.y + point.y)
        guard CGWarpMouseCursorPosition(cursorPosition) == .success else {
            throw HelperError(message: "could not move the native cursor to the owned window")
        }
        let eventNumber = index + 1
        let moved = try mouseEvent(
            type: .mouseMoved,
            location: localPoint(x: point.x, y: point.y),
            windowID: id,
            eventNumber: eventNumber,
            clickCount: 0,
            pressure: 0
        )
        associate(moved, with: window)
        moved.setIntegerValueField(.mouseEventButtonNumber, value: 0)
        moved.postToPid(pid)
        let sent = ProcessInfo.processInfo.systemUptime
        sentUptimeNs.append(UInt64(sent * 1_000_000_000))
        maximumLatenessNs = max(maximumLatenessNs, UInt64(max(0, sent - scheduled) * 1_000_000_000))
    }
    drainPostedEvents()
    emit([
        "event_count": points.count,
        "hz": hz,
        "start_uptime_ns": UInt64(start * 1_000_000_000),
        "sent_uptime_ns": sentUptimeNs,
        "maximum_lateness_ns": maximumLatenessNs,
    ])
}

func drag(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String, x1: Double, y1: Double, x2: Double, y2: Double) throws {
    let initialWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try validatePoint(x1, y1, in: initialWindow, name: "drag start")
    try validatePoint(x2, y2, in: initialWindow, name: "drag end")
    try activate(pid, window: initialWindow, expectedTitle: expectedTitle)

    let stepCount = 8
    var currentX = x1
    var currentY = y1
    var dragActive = false
    defer {
        if dragActive,
            let window = try? target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle),
            let up = try? mouseEvent(type: .leftMouseUp, location: localPoint(x: min(max(currentX, 0), max(window.width - 1, 0)), y: min(max(currentY, 0), max(window.height - 1, 0))), windowID: id, eventNumber: stepCount + 2, clickCount: 1, pressure: 0) {
            associate(up, with: window)
            up.postToPid(pid)
            drainPostedEvents()
        }
    }

    let downWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try activate(pid, window: downWindow, expectedTitle: expectedTitle)
    let focusedDownWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try validatePoint(x1, y1, in: focusedDownWindow, name: "drag start")
    let down = try mouseEvent(type: .leftMouseDown, location: localPoint(x: x1, y: y1), windowID: id, eventNumber: 1, clickCount: 1, pressure: 1)
    associate(down, with: focusedDownWindow)
    down.postToPid(pid)
    dragActive = true
    drainPostedEvents(short: true)

    for step in 1...stepCount {
        let progress = Double(step) / Double(stepCount)
        currentX = x1 + (x2 - x1) * progress
        currentY = y1 + (y2 - y1) * progress
        let stepWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
        try activate(pid, window: stepWindow, expectedTitle: expectedTitle)
        let focusedStepWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
        try validatePoint(currentX, currentY, in: focusedStepWindow, name: "drag step")
        let moved = try mouseEvent(type: .leftMouseDragged, location: localPoint(x: currentX, y: currentY), windowID: id, eventNumber: step + 1, clickCount: 1, pressure: 1)
        associate(moved, with: focusedStepWindow)
        moved.postToPid(pid)
        drainPostedEvents(short: true)
    }

    let upWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try activate(pid, window: upWindow, expectedTitle: expectedTitle)
    let focusedUpWindow = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try validatePoint(x2, y2, in: focusedUpWindow, name: "drag end")
    let up = try mouseEvent(type: .leftMouseUp, location: localPoint(x: x2, y: y2), windowID: id, eventNumber: stepCount + 2, clickCount: 1, pressure: 0)
    associate(up, with: focusedUpWindow)
    up.postToPid(pid)
    dragActive = false
    drainPostedEvents()
}

func keyCode(_ key: String) throws -> CGKeyCode {
    switch key {
    case "escape": return 53
    case "tab": return 48
    case "enter": return 36
    case "cmd-a": return 0
    case "cmd-f": return 3
    case "cmd-k": return 40
    case "cmd-shift-g": return 5
    case "backspace": return 51
    case "left": return 123
    case "right": return 124
    case "down": return 125
    case "up": return 126
    case "shift-left": return 123
    case "shift-right": return 124
    case "shift-down": return 125
    case "shift-up": return 126
    case "home": return 115
    case "end": return 119
    case "page-up": return 116
    case "page-down": return 121
    default: throw HelperError(message: "unsupported key: \(key)")
    }
}

func keyFlags(_ key: String) -> CGEventFlags {
    switch key {
    case "cmd-a": return .maskCommand
    case "cmd-f": return .maskCommand
    case "cmd-k": return .maskCommand
    case "cmd-shift-g": return [.maskCommand, .maskShift]
    case "shift-left", "shift-right", "shift-up", "shift-down": return .maskShift
    default: return []
    }
}

func clickModifier(_ name: String?) throws -> NSEvent.ModifierFlags {
    switch name {
    case nil: return []
    case "shift": return .shift
    case "cmd": return .command
    default: throw HelperError(message: "unsupported click modifier: \(name ?? "")")
    }
}

func key(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String, keyName: String) throws {
    let window = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try activate(pid, window: window, expectedTitle: expectedTitle)
    _ = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    let source = try eventSource()
    let code = try keyCode(keyName)
    let flags = keyFlags(keyName)
    guard let down = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: true),
          let up = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: false) else {
        throw HelperError(message: "could not create keyboard event")
    }
    down.flags = flags
    up.flags = flags
    down.postToPid(pid)
    up.postToPid(pid)
    drainPostedEvents()
}

func text(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String, value: String) throws {
    let window = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try activate(pid, window: window, expectedTitle: expectedTitle)
    _ = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    let source = try eventSource()
    let units = Array(value.utf16)
    var index = 0
    while index < units.count {
        var end = min(index + 20, units.count)
        if end < units.count && (0xD800...0xDBFF).contains(units[end - 1]) {
            end -= 1
        }
        let chunk = Array(units[index..<end])
        guard let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
              let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false) else {
            throw HelperError(message: "could not create Unicode keyboard event")
        }
        chunk.withUnsafeBufferPointer { buffer in
            down.keyboardSetUnicodeString(stringLength: buffer.count, unicodeString: buffer.baseAddress!)
            up.keyboardSetUnicodeString(stringLength: buffer.count, unicodeString: buffer.baseAddress!)
        }
        down.flags = []
        up.flags = []
        down.postToPid(pid)
        up.postToPid(pid)
        index = end
    }
    drainPostedEvents()
}

func resize(pid: pid_t, expectedPath: String, id: CGWindowID, expectedTitle: String, width: Double, height: Double) throws {
    guard width > 0, height > 0 else { throw HelperError(message: "resize dimensions must be positive") }
    let window = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    try activate(pid, window: window, expectedTitle: expectedTitle)
    _ = try target(pid: pid, expectedPath: expectedPath, id: id, expectedTitle: expectedTitle)
    let application = AXUIElementCreateApplication(pid)
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(application, kAXWindowsAttribute as CFString, &value) == .success,
          let appWindows = value as? [AXUIElement] else {
        throw HelperError(message: "owned window is not exposed to accessibility")
    }
    let candidates = appWindows.filter { hasScenarioTitle($0, expectedTitle: expectedTitle) }
    guard candidates.count == 1, let first = candidates.first else {
        throw HelperError(message: "owned Accessibility window cannot be identified unambiguously")
    }
    var size = CGSize(width: width, height: height)
    guard let axSize = AXValueCreate(.cgSize, &size) else {
        throw HelperError(message: "could not create accessibility size")
    }
    let result = AXUIElementSetAttributeValue(first, kAXSizeAttribute as CFString, axSize)
    guard result == .success else { throw HelperError(message: "could not resize owned window") }
}

func option(_ arguments: [String], _ name: String) -> String? {
    guard let index = arguments.firstIndex(of: name), index + 1 < arguments.count else { return nil }
    return arguments[index + 1]
}

func standardInputText() throws -> String {
    let data = FileHandle.standardInput.readDataToEndOfFile()
    guard let value = String(data: data, encoding: .utf8) else {
        throw HelperError(message: "text input is not valid UTF-8")
    }
    return value
}

func standardInputData() throws -> Data {
    FileHandle.standardInput.readDataToEndOfFile()
}

let arguments = Array(CommandLine.arguments.dropFirst())
        guard let command = arguments.first else { fail("usage: process-info|list-windows|click|right-click|mouse-move-trace|drag|text|key|resize") }

do {
    switch command {
    case "environment":
        emit(environment())
    case "process-info":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid) else { throw HelperError(message: "missing --pid") }
        emit(processInfo(pid))
    case "list-windows":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid), let path = option(arguments, "--path") else {
            throw HelperError(message: "list-windows requires --pid and --path")
        }
        emit(windows(pid: pid, expectedPath: path))
    case "activate":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let path = option(arguments, "--path"),
              let title = option(arguments, "--title"), !title.isEmpty,
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId) else {
            throw HelperError(message: "activate requires --pid, --path, --title, and --window-id")
        }
        let window = try target(pid: pid, expectedPath: path, id: id, expectedTitle: title)
        try activate(pid, window: window, expectedTitle: title)
        _ = try target(pid: pid, expectedPath: path, id: id, expectedTitle: title)
        emit(["ok": true])
    case "click":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              let rawX = option(arguments, "--x"), let x = Double(rawX),
              let rawY = option(arguments, "--y"), let y = Double(rawY),
              let path = option(arguments, "--path") else {
            throw HelperError(message: "click requires --pid, --path, --title, --window-id, --x, and --y")
        }
        guard let count = Int(option(arguments, "--click-count") ?? "1") else { throw HelperError(message: "invalid click count") }
        try click(pid: pid, expectedPath: path, id: id, expectedTitle: title, x: x, y: y, count: count, modifierFlags: clickModifier(option(arguments, "--modifier")))
        emit(["ok": true])
    case "right-click":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              let rawX = option(arguments, "--x"), let x = Double(rawX),
              let rawY = option(arguments, "--y"), let y = Double(rawY),
              let path = option(arguments, "--path") else {
            throw HelperError(message: "right-click requires --pid, --path, --title, --window-id, --x, and --y")
        }
        try rightClick(pid: pid, expectedPath: path, id: id, expectedTitle: title, x: x, y: y)
        emit(["ok": true])
    case "mouse-move-trace":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              let path = option(arguments, "--path") else {
            throw HelperError(message: "mouse-move-trace requires --pid, --path, --title, and --window-id")
        }
        try mouseMoveTrace(pid: pid, expectedPath: path, id: id, expectedTitle: title)
    case "drag":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              let rawX1 = option(arguments, "--x1"), let x1 = Double(rawX1),
              let rawY1 = option(arguments, "--y1"), let y1 = Double(rawY1),
              let rawX2 = option(arguments, "--x2"), let x2 = Double(rawX2),
              let rawY2 = option(arguments, "--y2"), let y2 = Double(rawY2),
              let path = option(arguments, "--path") else {
            throw HelperError(message: "drag requires --pid, --path, --title, --window-id, --x1, --y1, --x2, and --y2")
        }
        try drag(pid: pid, expectedPath: path, id: id, expectedTitle: title, x1: x1, y1: y1, x2: x2, y2: y2)
        emit(["ok": true])
    case "text":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let path = option(arguments, "--path"),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              arguments.contains("--text-stdin") else {
            throw HelperError(message: "text requires --pid, --path, --title, --window-id, and --text-stdin")
        }
        try text(pid: pid, expectedPath: path, id: id, expectedTitle: title, value: standardInputText())
        emit(["ok": true])
    case "key":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let path = option(arguments, "--path"),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              let keyName = option(arguments, "--key") else {
            throw HelperError(message: "key requires --pid, --path, --title, --window-id, and --key")
        }
        try key(pid: pid, expectedPath: path, id: id, expectedTitle: title, keyName: keyName)
        emit(["ok": true])
    case "resize":
        guard let rawPid = option(arguments, "--pid"), let pid = pid_t(rawPid),
              let path = option(arguments, "--path"),
              let rawId = option(arguments, "--window-id"), let id = CGWindowID(rawId),
              let title = option(arguments, "--title"), !title.isEmpty,
              let rawWidth = option(arguments, "--width"), let width = Double(rawWidth),
              let rawHeight = option(arguments, "--height"), let height = Double(rawHeight) else {
            throw HelperError(message: "resize requires --pid, --path, --title, --window-id, --width, and --height")
        }
        try resize(pid: pid, expectedPath: path, id: id, expectedTitle: title, width: width, height: height)
        emit(["ok": true])
    default:
        throw HelperError(message: "unknown command: \(command)")
    }
} catch let error as HelperError {
    fail(error.message)
} catch {
    fail(String(describing: error))
}

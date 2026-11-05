import AppKit
import Foundation

let title = "TableX UI Scenario: new-connection"

final class ProbeView: NSView {
    override var acceptsFirstResponder: Bool { true }

    override func becomeFirstResponder() -> Bool {
        true
    }
}

final class ProbeWindow: NSWindow {
    let role: String

    init(role: String, frame: NSRect) {
        self.role = role
        super.init(contentRect: frame, styleMask: [.titled, .closable], backing: .buffered, defer: false)
        title = role == "primary" ? "TableX UI Scenario: new-connection" : "TableX UI Scenario: secondary"
        contentView = ProbeView(frame: NSRect(origin: .zero, size: frame.size))
        acceptsMouseMovedEvents = true
        isReleasedWhenClosed = false
    }

    override func sendEvent(_ event: NSEvent) {
        let goToFolderShortcut = (event.type == .keyDown || event.type == .keyUp)
            && event.keyCode == 5
            && event.modifierFlags.contains([.command, .shift])
        let isLeftMouse = event.type == .leftMouseDown || event.type == .leftMouseUp
        let isRightMouse = event.type == .rightMouseDown || event.type == .rightMouseUp
        let isDragged = event.type == .leftMouseDragged
        let isMouseMove = event.type == .mouseMoved
        let base: [String: Any] = [
            "event": "received",
            "kind": isRightMouse ? "right" : isDragged ? "drag" : isLeftMouse ? "mouse" : isMouseMove ? "move" : "key",
            "windowNumber": windowNumber,
        ]
        if isLeftMouse || isRightMouse || isDragged {
            let point = event.locationInWindow
            let phase = event.type == .leftMouseDown || event.type == .rightMouseDown ? "down" : event.type == .leftMouseUp || event.type == .rightMouseUp ? "up" : "drag"
            emit(base.merging(["phase": phase, "buttonNumber": event.buttonNumber, "eventNumber": event.eventNumber, "localX": point.x, "localY": point.y, "clickCount": event.clickCount, "modifierFlags": event.modifierFlags.rawValue], uniquingKeysWith: { $1 }))
        } else if event.type == .keyDown || event.type == .keyUp {
            let length = event.charactersIgnoringModifiers?.utf16.count ?? 0
            let replacement = event.charactersIgnoringModifiers?.contains("\u{FFFD}") ?? false
            emit(base.merging(["phase": event.type == .keyDown ? "down" : "up", "keyCode": event.keyCode, "unicodeLength": length, "replacementCharacter": replacement, "modifierFlags": event.modifierFlags.rawValue], uniquingKeysWith: { $1 }))
        } else if isMouseMove {
            let point = event.locationInWindow
            let cursor = convertPoint(fromScreen: NSEvent.mouseLocation)
            emit(base.merging(["phase": "move", "buttonNumber": event.buttonNumber, "localX": point.x, "localY": point.y, "cursorX": cursor.x, "cursorY": cursor.y], uniquingKeysWith: { $1 }))
        }
        if !goToFolderShortcut {
            super.sendEvent(event)
        }
    }
}

func emit(_ value: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: value) else { return }
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([10]))
}

final class Delegate: NSObject, NSApplicationDelegate {
    var primary: ProbeWindow!
    var secondary: ProbeWindow!

    func applicationDidFinishLaunching(_ notification: Notification) {
        primary = ProbeWindow(role: "primary", frame: NSRect(x: 240, y: 180, width: 640, height: 480))
        secondary = ProbeWindow(role: "secondary", frame: NSRect(x: 920, y: 180, width: 320, height: 240))
        primary.makeKeyAndOrderFront(nil)
        secondary.orderFront(nil)
        primary.makeKey()
        primary.contentView?.becomeFirstResponder()
        emit(["event": "ready", "pid": ProcessInfo.processInfo.processIdentifier, "windowNumber": primary.windowNumber, "windowWidth": primary.frame.width, "windowHeight": primary.frame.height, "secondaryWindowNumber": secondary.windowNumber])
    }
}

let application = NSApplication.shared
let delegate = Delegate()
application.delegate = delegate
application.setActivationPolicy(.regular)
application.run()

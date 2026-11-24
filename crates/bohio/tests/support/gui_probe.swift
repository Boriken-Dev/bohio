// A window the GUI tests drive: a known block pattern, a focused text field, and one line of standard output per thing it
// receives. Built with `xcrun swiftc gui_probe.swift -o gui_probe_mac`; never touches anything but its own window.
//
//   gui_probe_mac X Y [SECONDS]
//
// X and Y are the pattern's top-left in points of the global space (the main display's top-left is the origin).
//
// Output (points of the global space):
//   ready X Y W H SCALE   the pattern's top-left, its size and the backing scale, once the window is up
//   focused / unfocused   the window became or stopped being the key window
//   mouse BUTTON X Y      a button went down over the window
//   wheel V DY H DX       the wheel turned (lines)
//   char U+XXXX           a character reached the text field
//   down CODE ctrl=0 cmd=0 shift=0 alt=0   a key went down
//   text ESCAPED          the field's content, URL-escaped, after each change
//   bye                   the window closed
import AppKit

let cells = 4
let cell = 32

func say(_ line: String) {
    print(line)
    fflush(stdout)
}

final class Pattern: NSView {
    override var isFlipped: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        for cy in 0..<cells {
            for cx in 0..<cells {
                let red = CGFloat(30 + 50 * cx) / 255
                let green = CGFloat(30 + 50 * cy) / 255
                let blue = CGFloat(60 + 30 * ((cx + cy) % 4)) / 255
                NSColor(srgbRed: red, green: green, blue: blue, alpha: 1).setFill()
                NSBezierPath.fill(NSRect(x: cx * cell, y: cy * cell, width: cell, height: cell))
            }
        }
    }
}

final class Window: NSWindow {
    override var canBecomeKey: Bool { true }
}

let arguments = CommandLine.arguments
let left = arguments.count > 1 ? Double(arguments[1]) ?? 100 : 100
let top = arguments.count > 2 ? Double(arguments[2]) ?? 100 : 100
let seconds = arguments.count > 3 ? Double(arguments[3]) ?? 120 : 120

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

let side = cells * cell
let field = 28
let mainHeight = NSScreen.screens.first?.frame.height ?? 0
let window = Window(
    contentRect: NSRect(x: left, y: mainHeight - top - Double(side + field), width: Double(side), height: Double(side + field)),
    styleMask: [.borderless],
    backing: .buffered,
    defer: false
)
window.level = .floating
window.hasShadow = false
window.isReleasedWhenClosed = false
let content = NSView(frame: NSRect(x: 0, y: 0, width: side, height: side + field))
let pattern = Pattern(frame: NSRect(x: 0, y: field, width: side, height: side))
let text = NSTextField(frame: NSRect(x: 0, y: 0, width: side, height: field))
content.addSubview(pattern)
content.addSubview(text)
window.contentView = content

func escaped(_ value: String) -> String {
    return value.addingPercentEncoding(withAllowedCharacters: CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.~")) ?? ""
}

func place() -> (Int, Int) {
    let at = CGEvent(source: nil)?.location ?? .zero
    return (Int(at.x.rounded(.down)), Int(at.y.rounded(.down)))
}

NotificationCenter.default.addObserver(forName: NSWindow.didBecomeKeyNotification, object: window, queue: nil) { _ in say("focused") }
NotificationCenter.default.addObserver(forName: NSWindow.didResignKeyNotification, object: window, queue: nil) { _ in say("unfocused") }
NotificationCenter.default.addObserver(forName: NSControl.textDidChangeNotification, object: text, queue: nil) { _ in
    say("text " + escaped(text.stringValue))
}
NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: window, queue: nil) { _ in say("bye") }

let mask: NSEvent.EventTypeMask = [.keyDown, .leftMouseDown, .rightMouseDown, .otherMouseDown, .scrollWheel]
NSEvent.addLocalMonitorForEvents(matching: mask) { event in
    switch event.type {
    case .leftMouseDown, .rightMouseDown, .otherMouseDown:
        let name = event.type == .leftMouseDown ? "left" : (event.type == .rightMouseDown ? "right" : "middle")
        let (x, y) = place()
        say("mouse \(name) \(x) \(y)")
    case .scrollWheel:
        say("wheel V \(event.deltaY) H \(event.deltaX)")
    case .keyDown:
        for scalar in (event.characters ?? "").unicodeScalars {
            say(String(format: "char U+%04X", scalar.value))
        }
        let flags = event.modifierFlags
        let ctrl = flags.contains(.control) ? 1 : 0
        let cmd = flags.contains(.command) ? 1 : 0
        let shift = flags.contains(.shift) ? 1 : 0
        let alt = flags.contains(.option) ? 1 : 0
        say("down \(event.keyCode) ctrl=\(ctrl) cmd=\(cmd) shift=\(shift) alt=\(alt)")
    default:
        break
    }
    return event
}

window.makeKeyAndOrderFront(nil)
window.makeFirstResponder(text)
app.activate(ignoringOtherApps: true)
say("ready \(Int(left)) \(Int(top)) \(side) \(side) \(window.backingScaleFactor)")

Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in
    window.close()
    app.terminate(nil)
}
app.run()

// Renders docs/assets/icon.svg to app/AppIcon.icns, adding the film grain an SVG can't hold.
// Usage, from the repo root: swift scripts/make-icon.swift [icon.svg] [AppIcon.icns]
// The icons to pick from in Settings: swift scripts/make-icon.swift docs/assets/icons/<name>.svg app/Icons/icon-<name>.png
import AppKit

let arguments = CommandLine.arguments
let source = arguments.count > 1 ? arguments[1] : "docs/assets/icon.svg"
let output = arguments.count > 2 ? arguments[2] : "app/AppIcon.icns"

func bitmap(_ pixels: Int, draw: (NSRect) -> Void) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                               bytesPerRow: pixels * 4, bitsPerPixel: 32)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.imageInterpolation = .high
    draw(NSRect(x: 0, y: 0, width: pixels, height: pixels))
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

guard let svg = NSImage(contentsOfFile: source) else { fatalError("Can't read \(source)") }
let master = bitmap(1024) { svg.draw(in: $0) }

// Grain: each 2x2 block of opaque pixels a little lighter or darker. Hashed from the
// position rather than random, so the icon comes out the same on every run.
let data = master.bitmapData!
for y in 0..<1024 {
    for x in 0..<1024 where data[(y * 1024 + x) * 4 + 3] == 255 {
        var hash = UInt32(truncatingIfNeeded: (x / 2) &* 73_856_093 ^ (y / 2) &* 19_349_663)
        hash = (hash ^ hash >> 13) &* 1_274_126_177
        let shift = Int((hash ^ hash >> 16) % 45) - 22
        for channel in 0..<3 {
            let index = (y * 1024 + x) * 4 + channel
            data[index] = UInt8(clamping: Int(data[index]) + shift)
        }
    }
}

let image = NSImage(size: NSSize(width: 1024, height: 1024))
image.addRepresentation(master)
// A .png output is one of the icons to pick from in Settings: a single 256px image.
if output.hasSuffix(".png") {
    try! bitmap(256) { image.draw(in: $0) }.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: output))
    print("Wrote \(output)")
    exit(0)
}
let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for (name, pixels) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128),
                       ("128x128@2x", 256), ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    let png = bitmap(pixels) { image.draw(in: $0) }.representation(using: .png, properties: [:])!
    try! png.write(to: iconset.appendingPathComponent("icon_\(name).png"))
}
let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", iconset.path, "-o", output]
try! iconutil.run()
iconutil.waitUntilExit()
print("Wrote \(output)")

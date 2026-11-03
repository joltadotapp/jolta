import AppKit

// Draws the app icon: a white kite on an orange squircle, its cross spar
// folded like an envelope flap. Variant b is the dark one (orange kite on
// graphite). Writes every macOS size into the asset catalog:
//
//   swiftc -O -o /tmp/app-icon scripts/app-icon.swift && /tmp/app-icon a apple/Jolta/Assets.xcassets/AppIcon.appiconset
let args = CommandLine.arguments
let variant = args[1], folder = args[2]

for size in [16, 32, 64, 128, 256, 512, 1024] {
    draw(size: CGFloat(size), to: "\(folder)/icon_\(size).png")
}

func color(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: a)
}

func draw(size: CGFloat, to out: String) {
    let s = size / 1024
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let c = NSGraphicsContext.current!.cgContext
    c.scaleBy(x: s, y: s)

    // macOS icon grid: an 824 pt squircle centred in the 1024 canvas, with a soft drop shadow.
    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    let squircle = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -10 * s), blur: 28 * s, color: color(0x000000, 0.35))
    c.addPath(squircle); c.setFillColor(color(0xE5521A)); c.fillPath()
    c.restoreGState()

    c.saveGState()
    c.addPath(squircle); c.clip()
    let dark = variant == "b"
    let top = dark ? color(0x2A2A30) : color(0xFF8A4C)
    let bottom = dark ? color(0x121214) : color(0xD9420F)
    let bg = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: [top, bottom] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(bg, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
    // A faint highlight across the top, like light on glass.
    let shine = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: [color(0xFFFFFF, dark ? 0.06 : 0.18), color(0xFFFFFF, 0)] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(shine, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 560), options: [])

    // The kite, tilted into the wind.
    c.translateBy(x: 530, y: 545)
    c.rotate(by: -0.26)
    let topV = CGPoint(x: 0, y: 270), left = CGPoint(x: -185, y: 60), right = CGPoint(x: 185, y: 60), bottomV = CGPoint(x: 0, y: -250)
    let kite = CGMutablePath()
    kite.move(to: topV); kite.addLine(to: right); kite.addLine(to: bottomV); kite.addLine(to: left); kite.closeSubpath()

    let paper = dark ? color(0xFF6B2C) : color(0xFFFFFF)
    let ink = dark ? color(0x121214, 0.55) : color(0xE5521A, 0.9)

    // Tail first, so the kite covers its start.
    let tail = CGMutablePath()
    tail.move(to: bottomV)
    tail.addCurve(to: CGPoint(x: -60, y: -470), control1: CGPoint(x: 60, y: -330), control2: CGPoint(x: -150, y: -380))
    c.addPath(tail); c.setStrokeColor(paper); c.setLineWidth(18); c.setLineCap(.round); c.strokePath()
    func bow(_ p: CGPoint, _ angle: CGFloat) {
        c.saveGState(); c.translateBy(x: p.x, y: p.y); c.rotate(by: angle)
        let b = CGMutablePath()
        b.move(to: .zero); b.addLine(to: CGPoint(x: -40, y: 24)); b.addLine(to: CGPoint(x: -40, y: -24)); b.closeSubpath()
        b.move(to: .zero); b.addLine(to: CGPoint(x: 40, y: 24)); b.addLine(to: CGPoint(x: 40, y: -24)); b.closeSubpath()
        c.addPath(b); c.setFillColor(paper); c.fillPath()
        c.restoreGState()
    }
    bow(CGPoint(x: 22, y: -322), 0.5)
    bow(CGPoint(x: -78, y: -415), 0.15)

    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -6 * s), blur: 18 * s, color: color(0x000000, dark ? 0.5 : 0.22))
    c.addPath(kite); c.setFillColor(paper); c.fillPath()
    c.restoreGState()

    // The cross spar folds like an envelope flap: left point, centre, right point.
    let flap = CGMutablePath()
    flap.move(to: left); flap.addLine(to: CGPoint(x: 0, y: -40)); flap.addLine(to: right)
    c.addPath(flap); c.setStrokeColor(ink); c.setLineWidth(16); c.setLineJoin(.round); c.setLineCap(.round); c.strokePath()
    let spar = CGMutablePath()
    spar.move(to: CGPoint(x: 0, y: 230)); spar.addLine(to: CGPoint(x: 0, y: -40))
    c.addPath(spar); c.setStrokeColor(ink.copy(alpha: 0.35)!); c.setLineWidth(8); c.strokePath()
    c.restoreGState()

    NSGraphicsContext.current = nil
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
}

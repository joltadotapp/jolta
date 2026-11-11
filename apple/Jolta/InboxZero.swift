import AppKit
import SwiftUI

/// What an empty inbox looks like: a sky in the colours of the theme and the
/// time of day, with the app icon's lightning bolt floating above the words.
/// Drawn, not a photo, so it costs nothing to ship and fits every window size.
/// The theme's `Scenery` holds the colours; a dark theme's sky stays dark.
///
/// Core Animation layers draw the sky, the hills and the bolt, in the window
/// server. When SwiftUI itself draws window-sized shapes or a Canvas, Jolta
/// holds window-sized GPU buffers: on the MacBook's XDR display about 200 MB
/// for a moment each time the screen appears or its colours change, and
/// 190 MB for as long as a 30 fps animation ran (measured 2026-11-03).
struct InboxZero: View {
    let compose: () -> Void
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let scenery = ThemeStore.shared.active[scheme == .dark ? .dark : .light].scenery
        // The sky changes on the hour at most, and when the theme does.
        TimelineView(.periodic(from: Calendar.current.dateInterval(of: .hour, for: .now)?.start ?? .now, by: 3600)) { context in
            let sky = Sky.now(context.date, scenery)
            ZStack {
                SkyBackground(sky: sky)
                VStack(spacing: 10) {
                    FloatingBolt(sky: sky)
                        .frame(width: FloatingBolt.size.width, height: FloatingBolt.size.height)
                        .padding(.bottom, 4)
                    Text("Inbox zero").font(.system(size: 26, weight: .semibold)).tracking(-0.4)
                    Text(sky.line).font(.system(size: 14)).opacity(0.8)
                    Button(action: compose) {
                        HStack(spacing: 6) { Text("Write something"); KeyCap(key: "C", style: .inherit) }
                    }
                    .buttonStyle(.plain)
                    .font(.system(size: 12.5))
                    .padding(.horizontal, 12)
                    .frame(height: 28)
                    .background(Capsule().fill(Color(NSColor(hex: sky.ink)).opacity(0.14)))
                    .padding(.top, 8)
                }
                .foregroundStyle(Color(NSColor(hex: sky.ink)))
                .padding(.bottom, 120)
            }
        }
        .ignoresSafeArea()
    }
}

/// The gradient sky, the night stars and the two hills, as Core Animation
/// layers.
private struct SkyBackground: NSViewRepresentable {
    let sky: Sky

    func makeNSView(context: Context) -> SkyLayers { SkyLayers() }

    func updateNSView(_ view: SkyLayers, context: Context) {
        view.show(sky)
    }
}

private final class SkyLayers: NSView {
    private let gradient = CAGradientLayer()
    private static let starBrightness = [0.3, 0.55, 0.8]
    private let stars = starBrightness.map { _ in CAShapeLayer() }
    private let far = CAShapeLayer()
    private let near = CAShapeLayer()

    init() {
        super.init(frame: .zero)
        wantsLayer = true
        for sublayer in [gradient] + stars + [far, near] {
            layer?.addSublayer(sublayer)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    /// Top-left origin, as the hill and star paths expect.
    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        gradient.frame = bounds
        for (band, layer) in stars.enumerated() {
            layer.frame = bounds
            layer.path = Stars(band: band).path(in: bounds).cgPath
        }
        for layer in [far, near] {
            layer.frame = bounds
            layer.path = Hill(far: layer === far).path(in: bounds).cgPath
        }
        CATransaction.commit()
    }

    func show(_ sky: Sky) {
        CATransaction.begin()
        CATransaction.setAnimationDuration(1)
        gradient.colors = [NSColor(hex: sky.top).cgColor, NSColor(hex: sky.bottom).cgColor]
        far.fillColor = NSColor(hex: sky.far).cgColor
        near.fillColor = NSColor(hex: sky.near).cgColor
        for (layer, brightness) in zip(stars, Self.starBrightness) {
            layer.fillColor = NSColor(hex: sky.starColor, alpha: brightness).cgColor
            layer.isHidden = !sky.stars
        }
        CATransaction.commit()
    }
}

/// The outline of one of the two hills along the bottom: the far one higher.
private struct Hill {
    let far: Bool

    func path(in rect: CGRect) -> Path {
        var hill = Path()
        let base = rect.height * (far ? 0.78 : 0.86)
        hill.move(to: CGPoint(x: 0, y: rect.height))
        hill.addLine(to: CGPoint(x: 0, y: base))
        hill.addCurve(to: CGPoint(x: rect.width, y: base + (far ? -20 : 10)),
                      control1: CGPoint(x: rect.width * (far ? 0.3 : 0.45), y: base - (far ? 70 : 50)),
                      control2: CGPoint(x: rect.width * (far ? 0.65 : 0.8), y: base + 40))
        hill.addLine(to: CGPoint(x: rect.width, y: rect.height))
        hill.closeSubpath()
        return hill
    }
}

/// A third of the 70 night stars, spread over the top three quarters;
/// each third has its own brightness.
private struct Stars {
    let band: Int

    func path(in rect: CGRect) -> Path {
        var stars = Path()
        for i in stride(from: band, to: 70, by: 3) {
            let x = Double((i * 7919) % 1000) / 1000 * rect.width
            let y = Double((i * 104_729) % 1000) / 1000 * rect.height * 0.75
            stars.addEllipse(in: CGRect(x: x, y: y, width: 1.6, height: 1.6))
        }
        return stars
    }
}

/// The app icon's bolt: amber, glossy, with a soft glow behind it, bobbing
/// and tilting slowly while the glow breathes. Core Graphics draws the glow and
/// the bolt into two images once per sky; Core Animation moves them.
private struct FloatingBolt: NSViewRepresentable {
    static let size = CGSize(width: 150, height: 170)
    let sky: Sky

    func makeNSView(context: Context) -> BoltLayers { BoltLayers() }

    func updateNSView(_ view: BoltLayers, context: Context) {
        view.show(sky)
    }

    /// Draws into a bitmap with Core Graphics, top-left origin, in points.
    /// SwiftUI's ImageRenderer held about 145 MB of GPU memory for a moment
    /// on each render of these two small images.
    static func image(scale: CGFloat, _ draw: (CGContext) -> Void) -> CGImage? {
        let width = Int(size.width * scale), height = Int(size.height * scale)
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.translateBy(x: 0, y: CGFloat(height))
        ctx.scaleBy(x: scale, y: -scale)
        draw(ctx)
        return ctx.makeImage()
    }

    /// Where the bolt and the glow are centred.
    static let centre = CGPoint(x: size.width / 2, y: size.height / 2 - 6)

    /// The glow, 148 points across.
    static func glow(_ sky: Sky, scale: CGFloat) -> CGImage? {
        image(scale: scale) { ctx in
            let colors = [1, 0.55, 0.18, 0].map { NSColor(hex: sky.glowColor, alpha: sky.glow * $0).cgColor }
            guard let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: colors as CFArray,
                                            locations: [0, 0.3, 0.6, 1]) else { return }
            ctx.drawRadialGradient(gradient, startCenter: centre, startRadius: 0, endCenter: centre, endRadius: 74, options: [])
        }
    }

    /// The bolt with its shadow, gloss and edge. Its outline is the icon's,
    /// closed to a point.
    static func bolt(_ sky: Sky, scale: CGFloat) -> CGImage? {
        image(scale: scale) { ctx in
            let height = 128.0, width = height * 0.42
            let points: [(Double, Double)] = [(310, -20), (380, -20), (310, 214), (366, 214), (198, 560), (272, 250), (226, 250)]
            let bolt = CGMutablePath()
            bolt.addLines(between: points.map {
                CGPoint(x: centre.x + ($0.0 - 198) / 182 * width - width / 2, y: centre.y + ($0.1 + 20) / 580 * height - height / 2)
            })
            bolt.closeSubpath()
            let box = bolt.boundingBox
            let srgb = CGColorSpace(name: CGColorSpace.sRGB)

            // Shadow offsets and blur are in pixels and point up, whatever the transform.
            ctx.setShadow(offset: CGSize(width: 5 * scale, height: -9 * scale), blur: 8 * scale,
                          color: CGColor(gray: 0, alpha: sky.stars ? 0.5 : 0.3))
            ctx.beginTransparencyLayer(auxiliaryInfo: nil)
            ctx.addPath(bolt)
            ctx.clip()
            let fillColors = [sky.boltTop, sky.boltMiddle, sky.boltTip].map { NSColor(hex: $0).cgColor }
            if let fill = CGGradient(colorsSpace: srgb, colors: fillColors as CFArray, locations: [0, 0.45, 1]) {
                ctx.drawLinearGradient(fill, start: CGPoint(x: box.minX, y: box.minY), end: CGPoint(x: box.maxX * 0.6 + centre.x * 0.4, y: box.maxY), options: [])
            }
            ctx.endTransparencyLayer()
            ctx.setShadow(offset: .zero, blur: 0, color: nil)

            ctx.saveGState()
            ctx.addPath(bolt)
            ctx.clip()
            let gloss = [CGColor(gray: 1, alpha: 0.55), CGColor(gray: 1, alpha: 0)]
            if let shine = CGGradient(colorsSpace: srgb, colors: gloss as CFArray, locations: [0, 1]) {
                ctx.drawLinearGradient(shine, start: CGPoint(x: box.minX, y: box.minY),
                                       end: CGPoint(x: box.midX, y: box.minY + box.height * 0.45), options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
            }
            ctx.restoreGState()

            ctx.addPath(bolt)
            ctx.setStrokeColor(NSColor(hex: sky.boltEdge, alpha: 0.55).cgColor)
            ctx.setLineWidth(1.2)
            ctx.setLineJoin(.round)
            ctx.strokePath()
        }
    }
}

/// Two image layers in a container: the container bobs and tilts, the glow
/// layer breathes. Each motion is a sine, as an eased back-and-forth.
private final class BoltLayers: NSView {
    private let float = CALayer()
    private let glow = CALayer()
    private let bolt = CALayer()
    private var sky: Sky?
    private var shown: (sky: Sky, scale: CGFloat)?

    init() {
        super.init(frame: CGRect(origin: .zero, size: FloatingBolt.size))
        wantsLayer = true
        layer?.masksToBounds = false
        float.addSublayer(glow)
        float.addSublayer(bolt)
        layer?.addSublayer(float)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        float.frame = bounds
        glow.frame = float.bounds
        bolt.frame = float.bounds
        CATransaction.commit()
    }

    /// Moving to a screen with another scale draws the images again.
    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        if let sky { show(sky) }
    }

    func show(_ sky: Sky) {
        self.sky = sky
        let scale = window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2
        // Two small images, drawn again only when the sky or the screen's scale changes.
        if let shown, shown.sky == sky, shown.scale == scale { return }
        shown = (sky, scale)
        for (layer, image) in [(glow, FloatingBolt.glow(sky, scale: scale)), (bolt, FloatingBolt.bolt(sky, scale: scale))] {
            layer.contents = image
            layer.contentsScale = scale
        }
        animate()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        animate()
    }

    private func animate() {
        float.removeAllAnimations()
        glow.removeAllAnimations()
        guard window != nil, !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else { return }
        float.add(swing("transform.translation.y", by: 5, period: 2 * .pi / 0.9), forKey: "bob")
        float.add(swing("transform.rotation.z", by: 0.04, period: 2 * .pi / 0.6), forKey: "tilt")
        let breathe = CABasicAnimation(keyPath: "opacity")
        breathe.fromValue = 0.85
        breathe.toValue = 1.0
        breathe.duration = .pi / 1.1
        breathe.autoreverses = true
        breathe.repeatCount = .infinity
        breathe.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        glow.add(breathe, forKey: "breathe")
    }

    /// From -amplitude to +amplitude and back once per period, eased at the ends.
    private func swing(_ keyPath: String, by amplitude: Double, period: Double) -> CABasicAnimation {
        let a = CABasicAnimation(keyPath: keyPath)
        a.fromValue = -amplitude
        a.toValue = amplitude
        a.duration = period / 2
        a.autoreverses = true
        a.repeatCount = .infinity
        a.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        a.isAdditive = true
        return a
    }
}

/// The theme's sky for the hour: dawn, day, dusk or night.
private struct Sky: Equatable {
    let top, bottom, far, near, ink: UInt32
    let boltTop, boltMiddle, boltTip, boltEdge, glowColor, starColor: UInt32
    /// How strongly the bolt glows: more against a dark sky.
    let glow: Double
    let stars: Bool
    let line: String

    static func now(_ date: Date, _ scenery: Scenery) -> Sky {
        // `-JoltaSkyHour 14` previews another time of day.
        let hour = UserDefaults.standard.string(forKey: "JoltaSkyHour").flatMap { Int($0) } ?? Calendar.current.component(.hour, from: date)
        let (colors, glow, stars, line) = switch hour {
        case 5..<9: (scenery.dawn, 0.4, false, "Nothing waiting. A quiet start to the day.")
        case 9..<17: (scenery.day, 0.18, false, "Nothing waiting. Go do something else.")
        case 17..<21: (scenery.dusk, 0.45, false, "Nothing waiting. Enjoy your evening.")
        default: (scenery.night, 0.55, true, "Nothing waiting. Time to rest.")
        }
        return Sky(top: colors.top, bottom: colors.bottom, far: colors.far, near: colors.near, ink: colors.ink,
                   boltTop: scenery.boltTop, boltMiddle: scenery.boltMiddle, boltTip: scenery.boltTip,
                   boltEdge: scenery.boltEdge, glowColor: scenery.glow, starColor: scenery.stars,
                   glow: glow, stars: stars, line: line)
    }
}

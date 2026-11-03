import SwiftUI

/// What an empty inbox looks like: a sky in the colours of the time of day,
/// with the app icon's lightning bolt floating above the words. Drawn, not a
/// photo, so it costs nothing to ship and fits every window size.
struct InboxZero: View {
    let compose: () -> Void

    var body: some View {
        TimelineView(.animation(minimumInterval: 1 / 30)) { context in
            let t = context.date.timeIntervalSinceReferenceDate
            let sky = Sky.now(context.date)
            ZStack {
                LinearGradient(colors: [sky.top, sky.bottom], startPoint: .top, endPoint: .bottom)
                Canvas { g, size in
                    if sky.stars {
                        for i in 0..<70 {
                            let x = Double((i * 7919) % 1000) / 1000 * size.width
                            let y = Double((i * 104_729) % 1000) / 1000 * size.height * 0.75
                            let twinkle = 0.45 + 0.35 * sin(t * 0.8 + Double(i))
                            g.fill(Path(ellipseIn: CGRect(x: x, y: y, width: 1.6, height: 1.6)), with: .color(.white.opacity(twinkle)))
                        }
                    }
                    drawHills(g, size: size, sky: sky)
                }
                VStack(spacing: 10) {
                    Canvas { g, size in drawBolt(g, size: size, t: t, sky: sky) }
                        .frame(width: 150, height: 170)
                        .padding(.bottom, 4)
                    Text("Inbox zero").font(.system(size: 26, weight: .semibold)).tracking(-0.4)
                    Text(sky.line).font(.system(size: 14)).opacity(0.8)
                    Button(action: compose) {
                        HStack(spacing: 6) { Text("Write something"); Text("C").font(.system(size: 11, weight: .medium)).opacity(0.7) }
                    }
                    .buttonStyle(.plain)
                    .font(.system(size: 12.5))
                    .padding(.horizontal, 12)
                    .frame(height: 28)
                    .background(Capsule().fill(.white.opacity(0.16)))
                    .padding(.top, 8)
                }
                .foregroundStyle(sky.ink)
                .padding(.bottom, 120)
            }
        }
        .ignoresSafeArea()
    }

    private func drawHills(_ g: GraphicsContext, size: CGSize, sky: Sky) {
        for (i, tint) in [sky.far, sky.near].enumerated() {
            var hill = Path()
            let base = size.height * (i == 0 ? 0.78 : 0.86)
            hill.move(to: CGPoint(x: 0, y: size.height))
            hill.addLine(to: CGPoint(x: 0, y: base))
            hill.addCurve(to: CGPoint(x: size.width, y: base + (i == 0 ? -20 : 10)),
                          control1: CGPoint(x: size.width * (i == 0 ? 0.3 : 0.45), y: base - (i == 0 ? 70 : 50)),
                          control2: CGPoint(x: size.width * (i == 0 ? 0.65 : 0.8), y: base + 40))
            hill.addLine(to: CGPoint(x: size.width, y: size.height))
            hill.closeSubpath()
            g.fill(hill, with: .color(tint))
        }
    }

    /// The app icon's bolt: amber, glossy, with a soft glow behind it,
    /// bobbing slowly. Its outline is the icon's, closed to a point.
    private func drawBolt(_ g: GraphicsContext, size: CGSize, t: Double, sky: Sky) {
        let height = 128.0, width = height * 0.42
        var b = g
        b.translateBy(x: size.width / 2, y: size.height / 2 - 6 + 5 * sin(t * 0.9))
        b.rotate(by: .radians(0.04 * sin(t * 0.6)))

        let glow = sky.glow * (0.85 + 0.15 * sin(t * 1.1))
        b.fill(Path(ellipseIn: CGRect(x: -74, y: -74, width: 148, height: 148)),
               with: .radialGradient(Gradient(stops: [0, 0.3, 0.6, 1].enumerated().map { i, at in
                   .init(color: Color(red: 1, green: 0.86, blue: 0.45).opacity(glow * [1, 0.55, 0.18, 0][i]), location: at)
               }),
                                     center: .zero, startRadius: 0, endRadius: 74))

        let points: [(Double, Double)] = [(310, -20), (380, -20), (310, 214), (366, 214), (198, 560), (272, 250), (226, 250)]
        var bolt = Path()
        bolt.addLines(points.map { CGPoint(x: ($0.0 - 198) / 182 * width - width / 2, y: ($0.1 + 20) / 580 * height - height / 2) })
        bolt.closeSubpath()
        let box = bolt.boundingRect

        var shadowed = b
        shadowed.addFilter(.shadow(color: .black.opacity(sky.stars ? 0.5 : 0.3), radius: 8, x: 5, y: 9))
        shadowed.fill(bolt, with: .linearGradient(
            Gradient(stops: [.init(color: Color(red: 1, green: 0.878, blue: 0.4), location: 0),
                             .init(color: Color(red: 1, green: 0.753, blue: 0.125), location: 0.45),
                             .init(color: Color(red: 1, green: 0.541, blue: 0), location: 1)]),
            startPoint: CGPoint(x: box.minX, y: box.minY), endPoint: CGPoint(x: box.maxX * 0.6, y: box.maxY)))
        b.fill(bolt, with: .linearGradient(Gradient(colors: [.white.opacity(0.55), .white.opacity(0)]),
                                           startPoint: CGPoint(x: box.minX, y: box.minY),
                                           endPoint: CGPoint(x: box.midX, y: box.minY + box.height * 0.45)))
        b.stroke(bolt, with: .color(Color(red: 0.784, green: 0.416, blue: 0).opacity(0.55)),
                 style: StrokeStyle(lineWidth: 1.2, lineJoin: .round))
    }
}

/// Sky colours for the hour: dawn, day, dusk, night.
private struct Sky {
    let top: Color, bottom: Color, far: Color, near: Color
    let ink: Color
    /// How strongly the bolt glows: more against a dark sky.
    let glow: Double
    let stars: Bool
    let line: String

    static func now(_ date: Date) -> Sky {
        let c = { (hex: UInt32) in Color(red: Double(hex >> 16 & 0xFF) / 255, green: Double(hex >> 8 & 0xFF) / 255, blue: Double(hex & 0xFF) / 255) }
        // `-JoltaSkyHour 14` previews another time of day.
        let hour = UserDefaults.standard.string(forKey: "JoltaSkyHour").flatMap { Int($0) } ?? Calendar.current.component(.hour, from: date)
        switch hour {
        case 5..<9:
            return Sky(top: c(0x5B6EA8), bottom: c(0xF2B48A), far: c(0x6E6A8E), near: c(0x48466A),
                       ink: .white, glow: 0.4, stars: false, line: "Nothing waiting. A quiet start to the day.")
        case 9..<17:
            return Sky(top: c(0x5FA8E8), bottom: c(0xCFE8F7), far: c(0x8DBF8A), near: c(0x5F9A63),
                       ink: c(0x17304A), glow: 0.18, stars: false, line: "Nothing waiting. Go do something else.")
        case 17..<21:
            return Sky(top: c(0x2E3A78), bottom: c(0xF08A5D), far: c(0x6B4F7A), near: c(0x3B2E52),
                       ink: .white, glow: 0.45, stars: false, line: "Nothing waiting. Enjoy your evening.")
        default:
            return Sky(top: c(0x0B1026), bottom: c(0x26305A), far: c(0x1C2244), near: c(0x121630),
                       ink: .white, glow: 0.55, stars: true, line: "Nothing waiting. Time to rest.")
        }
    }
}

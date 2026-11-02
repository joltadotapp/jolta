import SwiftUI

/// What an empty inbox looks like: a sky in the colours of the time of day,
/// with Kite's kite drifting on the wind. Drawn, not a photo, so it costs
/// nothing to ship and fits every window size.
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
                    drawKite(g, size: size, t: t, sky: sky)
                }
                VStack(spacing: 10) {
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

    /// The app icon's kite, swaying on a line from the near hill.
    private func drawKite(_ g: GraphicsContext, size: CGSize, t: Double, sky: Sky) {
        let anchor = CGPoint(x: size.width * 0.22, y: size.height * 0.88)
        let center = CGPoint(x: size.width * 0.74 + 18 * sin(t * 0.35), y: size.height * 0.2 + 12 * sin(t * 0.53))
        var line = Path()
        line.move(to: anchor)
        line.addQuadCurve(to: center, control: CGPoint(x: (anchor.x + center.x) / 2, y: max(anchor.y, center.y) - 30))
        g.stroke(line, with: .color(sky.ink.opacity(0.35)), lineWidth: 1)

        var k = g
        k.translateBy(x: center.x, y: center.y)
        k.rotate(by: .radians(0.26 + 0.07 * sin(t * 0.7)))
        let s = 0.17
        let top = CGPoint(x: 0, y: -270 * s), left = CGPoint(x: -185 * s, y: -60 * s)
        let right = CGPoint(x: 185 * s, y: -60 * s), bottom = CGPoint(x: 0, y: 250 * s)
        var tail = Path()
        tail.move(to: bottom)
        tail.addCurve(to: CGPoint(x: (-60 + 40 * sin(t)) * s, y: 470 * s),
                      control1: CGPoint(x: 60 * s, y: 330 * s), control2: CGPoint(x: (-150 + 30 * sin(t * 1.3)) * s, y: 380 * s))
        k.stroke(tail, with: .color(sky.kite), style: StrokeStyle(lineWidth: 2.5, lineCap: .round))
        var body = Path()
        body.move(to: top); body.addLine(to: right); body.addLine(to: bottom); body.addLine(to: left); body.closeSubpath()
        k.fill(body, with: .color(sky.kite))
        var flap = Path()
        flap.move(to: left); flap.addLine(to: CGPoint(x: 0, y: 40 * s)); flap.addLine(to: right)
        k.stroke(flap, with: .color(sky.flap), style: StrokeStyle(lineWidth: 2.5, lineCap: .round, lineJoin: .round))
    }
}

/// Sky colours for the hour: dawn, day, dusk, night.
private struct Sky {
    let top: Color, bottom: Color, far: Color, near: Color
    let kite: Color, flap: Color, ink: Color
    let stars: Bool
    let line: String

    static func now(_ date: Date) -> Sky {
        let c = { (hex: UInt32) in Color(red: Double(hex >> 16 & 0xFF) / 255, green: Double(hex >> 8 & 0xFF) / 255, blue: Double(hex & 0xFF) / 255) }
        // `-KiteSkyHour 14` previews another time of day.
        let hour = UserDefaults.standard.string(forKey: "KiteSkyHour").flatMap { Int($0) } ?? Calendar.current.component(.hour, from: date)
        switch hour {
        case 5..<9:
            return Sky(top: c(0x5B6EA8), bottom: c(0xF2B48A), far: c(0x6E6A8E), near: c(0x48466A),
                       kite: c(0xFF8A4C), flap: c(0xFFFFFF), ink: .white, stars: false, line: "Nothing waiting. A quiet start to the day.")
        case 9..<17:
            return Sky(top: c(0x5FA8E8), bottom: c(0xCFE8F7), far: c(0x8DBF8A), near: c(0x5F9A63),
                       kite: c(0xE5521A), flap: c(0xFFFFFF), ink: c(0x17304A), stars: false, line: "Nothing waiting. Go do something else.")
        case 17..<21:
            return Sky(top: c(0x2E3A78), bottom: c(0xF08A5D), far: c(0x6B4F7A), near: c(0x3B2E52),
                       kite: c(0xFF8A4C), flap: c(0xFFFFFF), ink: .white, stars: false, line: "Nothing waiting. Enjoy your evening.")
        default:
            return Sky(top: c(0x0B1026), bottom: c(0x26305A), far: c(0x1C2244), near: c(0x121630),
                       kite: c(0xFF6B2C), flap: c(0x121214), ink: .white, stars: true, line: "Nothing waiting. Time to rest.")
        }
    }
}

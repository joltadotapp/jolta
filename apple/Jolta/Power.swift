@preconcurrency import JoltaFFI
import Foundation
import IOKit.ps

/// Tells the core when background AI may use the local model: on mains
/// power, outside Low Power Mode, and while the Mac isn't running hot.
@MainActor
final class PowerWatch {
    private var core: Core?
    private var observers: [NSObjectProtocol] = []
    private var source: CFRunLoopSource?

    func start(core: Core) {
        self.core = core
        let center = NotificationCenter.default
        for name in [Notification.Name.NSProcessInfoPowerStateDidChange, ProcessInfo.thermalStateDidChangeNotification] {
            observers.append(center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.update() }
            })
        }
        let context = Unmanaged.passUnretained(self).toOpaque()
        if let source = IOPSNotificationCreateRunLoopSource({ context in
            guard let context else { return }
            let watch = Unmanaged<PowerWatch>.fromOpaque(context).takeUnretainedValue()
            MainActor.assumeIsolated { watch.update() }
        }, context)?.takeRetainedValue() {
            CFRunLoopAddSource(CFRunLoopGetMain(), source, .defaultMode)
            self.source = source
        }
        update()
    }

    private func update() {
        let info = ProcessInfo.processInfo
        let onMains = IOPSGetProvidingPowerSourceType(IOPSCopyPowerSourcesInfo()?.takeRetainedValue())?
            .takeUnretainedValue() as String? != kIOPMBatteryPowerKey
        let cool = info.thermalState == .nominal || info.thermalState == .fair
        core?.setAiBackgroundAllowed(allowed: onMains && !info.isLowPowerModeEnabled && cool)
    }
}

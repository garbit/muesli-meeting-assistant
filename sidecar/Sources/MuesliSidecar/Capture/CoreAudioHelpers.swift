import AudioToolbox
import CoreAudio
import Foundation

struct CoreAudioError: Error, CustomStringConvertible {
    let what: String
    let status: OSStatus
    var description: String { "\(what) failed: OSStatus \(status) (\(fourCC(UInt32(bitPattern: status))))" }
}

func fourCC(_ v: UInt32) -> String {
    let bytes = [UInt8((v >> 24) & 0xFF), UInt8((v >> 16) & 0xFF), UInt8((v >> 8) & 0xFF), UInt8(v & 0xFF)]
    if bytes.allSatisfy({ $0 >= 0x20 && $0 < 0x7F }) { return String(bytes: bytes, encoding: .ascii) ?? "\(v)" }
    return "\(v)"
}

func address(
    _ selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
    element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: element)
}

extension AudioObjectID {
    static let system = AudioObjectID(kAudioObjectSystemObject)
    var isValid: Bool { self != kAudioObjectUnknown }

    func read<T>(_ selector: AudioObjectPropertySelector, scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal, initial: T) throws -> T {
        var addr = address(selector, scope: scope)
        var size = UInt32(MemoryLayout<T>.size)
        var value = initial
        let st = withUnsafeMutablePointer(to: &value) { AudioObjectGetPropertyData(self, &addr, 0, nil, &size, $0) }
        guard st == noErr else { throw CoreAudioError(what: "get \(fourCC(selector))", status: st) }
        return value
    }

    func read<T, Q>(_ selector: AudioObjectPropertySelector, qualifier: Q, initial: T) throws -> T {
        var addr = address(selector)
        var size = UInt32(MemoryLayout<T>.size)
        var value = initial
        var q = qualifier
        let st = withUnsafeMutablePointer(to: &q) { qp in
            withUnsafeMutablePointer(to: &value) {
                AudioObjectGetPropertyData(self, &addr, UInt32(MemoryLayout<Q>.size), qp, &size, $0)
            }
        }
        guard st == noErr else { throw CoreAudioError(what: "get \(fourCC(selector))", status: st) }
        return value
    }

    func readArray<T>(_ selector: AudioObjectPropertySelector, scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal, of: T.Type) throws -> [T] {
        var addr = address(selector, scope: scope)
        var size: UInt32 = 0
        var st = AudioObjectGetPropertyDataSize(self, &addr, 0, nil, &size)
        guard st == noErr else { throw CoreAudioError(what: "size \(fourCC(selector))", status: st) }
        let count = Int(size) / MemoryLayout<T>.stride
        guard count > 0 else { return [] }
        let out = [T](unsafeUninitializedCapacity: count) { buf, n in
            st = AudioObjectGetPropertyData(self, &addr, 0, nil, &size, buf.baseAddress!)
            n = st == noErr ? count : 0
        }
        guard st == noErr else { throw CoreAudioError(what: "get \(fourCC(selector))", status: st) }
        return out
    }

    func readString(_ selector: AudioObjectPropertySelector) throws -> String {
        var addr = address(selector)
        var size = UInt32(MemoryLayout<CFString?>.size)
        var value: Unmanaged<CFString>?
        let st = withUnsafeMutablePointer(to: &value) { AudioObjectGetPropertyData(self, &addr, 0, nil, &size, $0) }
        guard st == noErr, let v = value?.takeRetainedValue() else { throw CoreAudioError(what: "get string \(fourCC(selector))", status: st) }
        return v as String
    }

    func readBool(_ selector: AudioObjectPropertySelector) -> Bool {
        ((try? read(selector, initial: UInt32(0))) ?? 0) != 0
    }

    var name: String { (try? readString(kAudioObjectPropertyName)) ?? "?" }
    var deviceUID: String { get throws { try readString(kAudioDevicePropertyDeviceUID) } }

    func inputStreamCount() -> Int {
        (try? readArray(kAudioDevicePropertyStreams, scope: kAudioObjectPropertyScopeInput, of: AudioStreamID.self).count) ?? 0
    }

    static func defaultInputDevice() throws -> AudioDeviceID {
        try AudioObjectID.system.read(kAudioHardwarePropertyDefaultInputDevice, initial: AudioDeviceID(kAudioObjectUnknown))
    }

    static func defaultOutputDevice() throws -> AudioDeviceID {
        try AudioObjectID.system.read(kAudioHardwarePropertyDefaultOutputDevice, initial: AudioDeviceID(kAudioObjectUnknown))
    }

    static func processObject(forPID pid: pid_t) -> AudioObjectID? {
        guard let id = try? AudioObjectID.system.read(kAudioHardwarePropertyTranslatePIDToProcessObject, qualifier: pid, initial: AudioObjectID(kAudioObjectUnknown)), id.isValid else { return nil }
        return id
    }

    /// True when any audio process other than the given pids is currently producing output.
    static func otherProcessesRunningOutput(excluding pids: Set<pid_t>) -> Bool {
        guard let list = try? AudioObjectID.system.readArray(kAudioHardwarePropertyProcessObjectList, of: AudioObjectID.self) else { return false }
        for obj in list {
            let pid = (try? obj.read(kAudioProcessPropertyPID, initial: pid_t(0))) ?? 0
            if pids.contains(pid) { continue }
            if obj.readBool(kAudioProcessPropertyIsRunningOutput) { return true }
        }
        return false
    }
}

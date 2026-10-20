// The Mac's sound on the phone. The engine decodes it (48 kHz stereo,
// 16-bit); this pulls it from the engine's buffer whenever the speaker
// asks for more. Started with the stream, stopped with it.

import AVFoundation
import Foundation

final class AudioOutput {
    private let engine = AVAudioEngine()
    private var node: AVAudioSourceNode?
    private var scratch = [Int16](repeating: 0, count: 4096)

    private var interruption: NSObjectProtocol?

    func start() {
        let session = AVAudioSession.sharedInstance()
        // A phone call or Siri stops the engine; start it again after.
        interruption = NotificationCenter.default.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) { [weak self] n in
            guard let self, let raw = n.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                  AVAudioSession.InterruptionType(rawValue: raw) == .ended else { return }
            try? session.setActive(true)
            if !self.engine.isRunning { try? self.engine.start() }
        }
        try? session.setCategory(.playback, mode: .moviePlayback, options: [.mixWithOthers])
        try? session.setPreferredSampleRate(48_000)
        try? session.setPreferredIOBufferDuration(0.01)
        try? session.setActive(true)

        let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2)!
        let node = AVAudioSourceNode(format: format) { [weak self] _, _, frameCount, buffers in
            guard let self else { return noErr }
            let frames = Int(frameCount)
            let needed = frames * 2
            if self.scratch.count < needed { self.scratch = [Int16](repeating: 0, count: needed) }
            self.scratch.withUnsafeMutableBufferPointer { _ = aa_mobile_audio_pull($0.baseAddress, needed) }
            let abl = UnsafeMutableAudioBufferListPointer(buffers)
            // Planar float output: de-interleave and scale.
            for (ch, buf) in abl.enumerated() {
                guard let out = buf.mData?.assumingMemoryBound(to: Float.self) else { continue }
                for i in 0..<frames {
                    out[i] = Float(self.scratch[i * 2 + min(ch, 1)]) / 32768
                }
            }
            return noErr
        }
        self.node = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        engine.prepare()
        try? engine.start()
    }

    func stop() {
        if let interruption { NotificationCenter.default.removeObserver(interruption) }
        interruption = nil
        engine.stop()
        if let node { engine.detach(node) }
        node = nil
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}

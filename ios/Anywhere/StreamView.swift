// The Mac's picture on the phone: a Metal view that asks the engine for
// the newest decoded frame on every screen refresh, uploads it to a
// texture and draws it letterboxed. Touch on it moves the pointer (tap =
// click, long press = right-click, two fingers = scroll), so the app works
// without the controller too.

import MetalKit
import SwiftUI
import UIKit

/// The part of the picture shown when zoomed: a window of 1/zoom around
/// the pointer, kept inside the picture.
enum ZoomWindow {
    static func around(_ p: CGPoint, zoom: CGFloat) -> CGRect {
        let z = max(1, zoom)
        let w = 1 / z
        let x = min(1 - w, max(0, p.x - w / 2))
        let y = min(1 - w, max(0, p.y - w / 2))
        return CGRect(x: x, y: y, width: w, height: w)
    }
}

struct StreamView: UIViewRepresentable {
    @EnvironmentObject var engine: Engine
    let fps: Int
    /// Called with the picture's frame inside the view (for the pointer).
    let onLayout: (CGRect) -> Void

    func makeUIView(context: Context) -> StreamMetalView {
        let v = StreamMetalView()
        v.engine = engine
        v.onLayout = onLayout
        v.preferredFramesPerSecond = fps
        return v
    }

    func updateUIView(_ v: StreamMetalView, context: Context) {
        v.preferredFramesPerSecond = fps
        v.onLayout = onLayout
    }
}

final class StreamMetalView: MTKView, MTKViewDelegate {
    weak var engine: Engine?
    var onLayout: ((CGRect) -> Void)?
    private var queue: MTLCommandQueue?
    private var pipeline: MTLRenderPipelineState?
    private var texture: MTLTexture?
    private var textureFormat: UInt32 = 99
    private var videoRect = CGRect.zero
    private var videoSize = CGSize.zero
    private var reported = CGRect.zero
    private var hasPicture = false
    /// The window of the picture on screen (all of it, or the zoomed part).
    private var window = CGRect(x: 0, y: 0, width: 1, height: 1)

    // Touch state.
    private var lastPan: CGPoint?
    private var scrollAcc = CGPoint.zero

    init() {
        let dev = MTLCreateSystemDefaultDevice()
        super.init(frame: .zero, device: dev)
        colorPixelFormat = .bgra8Unorm
        framebufferOnly = true
        isPaused = false
        enableSetNeedsDisplay = false
        delegate = self
        backgroundColor = .black
        isMultipleTouchEnabled = true
        if let dev {
            queue = dev.makeCommandQueue()
            if let lib = dev.makeDefaultLibrary() {
                let d = MTLRenderPipelineDescriptor()
                d.vertexFunction = lib.makeFunction(name: "stream_vertex")
                d.fragmentFunction = lib.makeFunction(name: "stream_fragment")
                d.colorAttachments[0].pixelFormat = colorPixelFormat
                pipeline = try? dev.makeRenderPipelineState(descriptor: d)
            }
        }
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        let long = UILongPressGestureRecognizer(target: self, action: #selector(longPressed(_:)))
        long.minimumPressDuration = 0.45
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
        pan.maximumNumberOfTouches = 1
        let two = UIPanGestureRecognizer(target: self, action: #selector(twoFinger(_:)))
        two.minimumNumberOfTouches = 2
        two.maximumNumberOfTouches = 2
        addGestureRecognizer(tap)
        addGestureRecognizer(long)
        addGestureRecognizer(pan)
        addGestureRecognizer(two)
    }

    required init(coder: NSCoder) { fatalError("not used") }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        guard let device, let queue, let pipeline else { return }
        var f = AAFrame()
        if aa_mobile_frame(&f) == 1, let data = f.data, f.format <= 1, f.len >= Int(f.width * f.height * 4) {
            let format: MTLPixelFormat = f.format == 0 ? .bgra8Unorm : .rgba8Unorm
            if texture == nil || texture!.width != Int(f.width) || texture!.height != Int(f.height) || textureFormat != f.format {
                let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: Int(f.width), height: Int(f.height), mipmapped: false)
                d.usage = .shaderRead
                texture = device.makeTexture(descriptor: d)
                textureFormat = f.format
                videoSize = CGSize(width: Int(f.width), height: Int(f.height))
            }
            texture?.replace(region: MTLRegionMake2D(0, 0, Int(f.width), Int(f.height)), mipmapLevel: 0,
                             withBytes: data, bytesPerRow: Int(f.width) * 4)
            hasPicture = true
        }
        guard let drawable = currentDrawable, let pass = currentRenderPassDescriptor else { return }
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        guard let cmd = queue.makeCommandBuffer(), let enc = cmd.makeRenderCommandEncoder(descriptor: pass) else { return }
        if hasPicture, let texture {
            // Letterbox: fit the picture, keep its shape.
            let vw = drawableSize.width, vh = drawableSize.height
            let scale = min(vw / videoSize.width, vh / videoSize.height)
            let w = videoSize.width * scale, h = videoSize.height * scale
            let x = (vw - w) / 2, y = (vh - h) / 2
            videoRect = CGRect(x: x / vw * bounds.width, y: y / vh * bounds.height, width: w / vw * bounds.width, height: h / vh * bounds.height)
            if videoRect != reported {
                reported = videoRect
                onLayout?(videoRect)
            }
            var rect = SIMD4<Float>(Float(x / vw * 2 - 1), Float(1 - y / vh * 2), Float((x + w) / vw * 2 - 1), Float(1 - (y + h) / vh * 2))
            // Zoom follows the pointer.
            if let engine {
                let p = engine.pad?.mapper.pointer ?? CGPoint(x: 0.5, y: 0.5)
                window = ZoomWindow.around(p, zoom: engine.zoom)
            }
            var win = SIMD4<Float>(Float(window.minX), Float(window.minY), Float(window.width), Float(window.height))
            enc.setRenderPipelineState(pipeline)
            enc.setVertexBytes(&rect, length: MemoryLayout<SIMD4<Float>>.size, index: 0)
            enc.setVertexBytes(&win, length: MemoryLayout<SIMD4<Float>>.size, index: 1)
            enc.setFragmentTexture(texture, index: 0)
            enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }
        enc.endEncoding()
        cmd.present(drawable)
        cmd.commit()
    }

    // MARK: Touch → pointer

    private func normalised(_ p: CGPoint) -> CGPoint? {
        guard videoRect.width > 0 else { return nil }
        let x = (p.x - videoRect.minX) / videoRect.width
        let y = (p.y - videoRect.minY) / videoRect.height
        guard x >= 0, x <= 1, y >= 0, y <= 1 else { return nil }
        // Through the zoom window: a touch lands where it looks.
        return CGPoint(x: window.minX + x * window.width, y: window.minY + y * window.height)
    }

    private func move(to p: CGPoint) {
        guard let n = normalised(p), let engine else { return }
        engine.pad?.mapper.setPointer(n)
        engine.mouseAbs(x: n.x, y: n.y)
    }

    @objc private func tapped(_ g: UITapGestureRecognizer) {
        move(to: g.location(in: self))
        engine?.mouseButton(0, true)
        engine?.mouseButton(0, false)
    }

    @objc private func longPressed(_ g: UILongPressGestureRecognizer) {
        guard g.state == .began else { return }
        move(to: g.location(in: self))
        engine?.mouseButton(1, true)
        engine?.mouseButton(1, false)
    }

    @objc private func panned(_ g: UIPanGestureRecognizer) {
        move(to: g.location(in: self))
    }

    @objc private func twoFinger(_ g: UIPanGestureRecognizer) {
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        // A finger's point of movement ≈ 1/8 of a notch, in 1/120ths.
        scrollAcc.x += t.x * 15
        scrollAcc.y += t.y * 15
        let ix = Int(scrollAcc.x), iy = Int(scrollAcc.y)
        if ix != 0 || iy != 0 {
            engine?.scroll(dx: ix, dy: iy)
            scrollAcc.x -= CGFloat(ix)
            scrollAcc.y -= CGFloat(iy)
        }
    }
}

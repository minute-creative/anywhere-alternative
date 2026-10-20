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

    /// While typing: the part of the picture that fills a view of `view`
    /// size edge to edge, around `focus` (the text field, or a point).
    /// Wide enough for the field with some room either side, never less
    /// than 45% of the picture's width (enough to see where you are).
    /// nil when the view is tall enough that the plain picture fits better.
    static func band(around focus: CGRect, view: CGSize, picture: CGSize) -> CGRect? {
        guard view.width > 0, view.height > 0, picture.width > 0, picture.height > 0 else { return nil }
        // Height of the window (0…1) for a given width, so the window keeps
        // the view's shape on screen.
        let ratio = (view.height / view.width) * (picture.width / picture.height)
        var w = min(1, max(0.45, focus.width * 1.4))
        var h = w * ratio
        if h > 1 {
            h = 1
            w = 1 / ratio
            if w >= 1 { return nil }
        }
        let x = min(1 - w, max(0, focus.midX - w / 2))
        let y = min(1 - h, max(0, focus.midY - h / 2))
        return CGRect(x: x, y: y, width: w, height: h)
    }
}

struct StreamView: UIViewRepresentable {
    @EnvironmentObject var engine: Engine
    let fps: Int
    /// Called with the picture's frame inside the view (for the pointer).
    let onLayout: (CGRect) -> Void

    /// Draw at the stream's rate, never above what the screen can show.
    static func drawRate(_ fps: Int) -> Int { min(max(fps, 30), UIScreen.main.maximumFramesPerSecond) }

    func makeUIView(context: Context) -> StreamMetalView {
        let v = StreamMetalView()
        v.engine = engine
        v.onLayout = onLayout
        v.preferredFramesPerSecond = Self.drawRate(fps)
        return v
    }

    func updateUIView(_ v: StreamMetalView, context: Context) {
        v.preferredFramesPerSecond = Self.drawRate(fps)
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
    private var shown = CGRect(x: 0, y: 0, width: 1, height: 1)

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
            let vw = drawableSize.width, vh = drawableSize.height
            let p = engine?.pad?.mapper.pointer ?? CGPoint(x: 0.5, y: 0.5)
            // Typing: fill the space above the keyboard with a band of the
            // picture around the field, not the whole screen shrunk small.
            let focus = engine?.status.textField ?? CGRect(x: p.x, y: p.y, width: 0, height: 0)
            let band = engine?.typingBand == true
                ? ZoomWindow.band(around: focus, view: CGSize(width: vw, height: vh), picture: videoSize) : nil
            // Otherwise letterbox: fit the picture, keep its shape.
            let scale = min(vw / videoSize.width, vh / videoSize.height)
            let w = band == nil ? videoSize.width * scale : vw, h = band == nil ? videoSize.height * scale : vh
            let x = (vw - w) / 2, y = (vh - h) / 2
            videoRect = CGRect(x: x / vw * bounds.width, y: y / vh * bounds.height, width: w / vw * bounds.width, height: h / vh * bounds.height)
            if videoRect != reported {
                reported = videoRect
                onLayout?(videoRect)
            }
            var rect = SIMD4<Float>(Float(x / vw * 2 - 1), Float(1 - y / vh * 2), Float((x + w) / vw * 2 - 1), Float(1 - (y + h) / vh * 2))
            // Zoom follows the pointer.
            if let band {
                shown = band
            } else if let engine {
                shown = ZoomWindow.around(p, zoom: engine.zoom)
            }
            var win = SIMD4<Float>(Float(shown.minX), Float(shown.minY), Float(shown.width), Float(shown.height))
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
        return CGPoint(x: shown.minX + x * shown.width, y: shown.minY + y * shown.height)
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

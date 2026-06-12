import CoreGraphics

// CoreGraphics geometry types are `Equatable` but, in the macOS 14 / Swift 5.10
// SDK, not `Hashable` — which blocks synthesized `Hashable` for types that store
// them (e.g. `ScreenElement.frame`, `ComputerUseAction.Kind`'s points). Provide
// the missing conformance once, retroactively, hashing the components.

extension CGPoint: @retroactive Hashable {
    public func hash(into hasher: inout Hasher) {
        hasher.combine(x)
        hasher.combine(y)
    }
}

extension CGSize: @retroactive Hashable {
    public func hash(into hasher: inout Hasher) {
        hasher.combine(width)
        hasher.combine(height)
    }
}

extension CGRect: @retroactive Hashable {
    public func hash(into hasher: inout Hasher) {
        hasher.combine(origin)
        hasher.combine(size)
    }
}

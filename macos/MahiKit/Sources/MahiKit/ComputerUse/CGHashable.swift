import CoreGraphics

// CoreGraphics geometry types are `Equatable` but, in the macOS 14 / Swift 5.10
// SDK, not `Hashable` — which blocks synthesized `Hashable` for types that store
// them (`ScreenElement.frame`, the points in `ComputerUseAction.Kind`). Add the
// missing conformance once, hashing the components. (Swift 5.10 emits a benign
// retroactive-conformance warning; the `@retroactive` attribute that silences it
// is Swift 6 only, so it is omitted here.)

extension CGPoint: Hashable {
    public func hash(into hasher: inout Hasher) {
        hasher.combine(x)
        hasher.combine(y)
    }
}

extension CGSize: Hashable {
    public func hash(into hasher: inout Hasher) {
        hasher.combine(width)
        hasher.combine(height)
    }
}

extension CGRect: Hashable {
    public func hash(into hasher: inout Hasher) {
        hasher.combine(origin)
        hasher.combine(size)
    }
}

import AppKit
import SwiftUI

/// Every accent color the app uses, by role. Ported from Argus's palettes: the
/// dark variants are Argus's own colors, and the light variants are deeper
/// versions that stay readable on white. Pick one from the palette button in the
/// review window's toolbar (click for the next, hold to choose); it's remembered.
struct Palette: Identifiable {
    let id: String
    let name: String
    /// Waiting on your review: the sidebar dot, repo counts, selection and focus.
    let accent: Color
    /// Approved, checks passed.
    let pass: Color
    /// Changes requested, checks failed, errors.
    let fail: Color
    /// Review required, checks running, warnings.
    let pending: Color
    /// Drafts written by an agent, and the chat's agent accents.
    let agent: Color
    /// Selected diff lines. Kept apart from `removed`, which some accents are close to.
    let selection: Color
    /// Diff lines: the +/− signs at full strength, row backgrounds as a light tint.
    let added: Color
    let removed: Color
    /// Repo names in the sidebar, so neighbouring repos are easy to tell apart.
    let repo: (String) -> Color

    /// One cool family for the states, plus a single warm color kept for what's broken.
    static let cool = Palette(
        id: "cool", name: "Cool",
        accent: hsb(0.76, light: (0.65, 0.72), dark: (0.55, 1.00)),   // violet
        pass: hsb(0.40, light: (0.75, 0.55), dark: (0.62, 0.86)),     // green
        fail: hsb(0.01, light: (0.75, 0.80), dark: (0.62, 1.00)),     // coral
        pending: hsb(0.49, light: (0.80, 0.58), dark: (0.65, 0.92)),  // teal
        agent: hsb(0.57, light: (0.80, 0.75), dark: (0.72, 1.00)),    // sky blue
        selection: hsb(0.76, light: (0.65, 0.72), dark: (0.55, 1.00)),
        added: hsb(0.40, light: (0.75, 0.55), dark: (0.62, 0.86)),
        removed: hsb(0.01, light: (0.75, 0.80), dark: (0.62, 1.00)),
        // Pastel, because the saturated hues belong to the states.
        repo: { hsb(0.30 + 0.50 * Double(stableHash($0) % 100) / 100, light: (0.60, 0.55), dark: (0.38, 0.97)) }
    )

    /// Six hand-picked muted tones for repos; rose, sage and amber for the states.
    static let soft = Palette(
        id: "soft", name: "Soft",
        accent: hex(light: 0xC04A55, dark: 0xE06C75),
        pass: hex(light: 0x5E7F45, dark: 0xA3BE8C),
        fail: hex(light: 0xC04A55, dark: 0xE06C75),
        pending: hex(light: 0xA97F1F, dark: 0xEBCB8B),
        agent: hex(light: 0x8A5F86, dark: 0xB48EAD),
        selection: hex(light: 0x4F6F92, dark: 0x81A1C1),
        added: hex(light: 0x5E7F45, dark: 0xA3BE8C),
        removed: hex(light: 0xC04A55, dark: 0xE06C75),
        repo: { repo in
            let dark: [UInt32] = [0x88C0D0, 0x81A1C1, 0xB48EAD, 0xA3BE8C, 0xEBCB8B, 0xD08770]
            let light: [UInt32] = [0x3F7F92, 0x4F6F92, 0x8A5F86, 0x5E7F45, 0xA0801F, 0xA65A3A]
            let index = Int(stableHash(repo) % UInt64(dark.count))
            return hex(light: light[index], dark: dark[index])
        }
    )

    /// Neutral everywhere except what needs a look: waiting on you, failing, running.
    static let focus = Palette(
        id: "focus", name: "Focus",
        accent: hex(light: 0xD23A5E, dark: 0xFF6B8B),
        pass: Color(light: .black.opacity(0.5), dark: .white.opacity(0.55)),
        fail: hex(light: 0xD23A5E, dark: 0xFF6B8B),
        pending: hex(light: 0xB07614, dark: 0xF2B45A),
        agent: hex(light: 0xB07614, dark: 0xF2B45A),
        selection: hex(light: 0x5B6B86, dark: 0x9AA8C2),
        // Muted, but a diff still has to tell additions from removals.
        added: hsb(0.40, light: (0.45, 0.45), dark: (0.30, 0.80)),
        removed: hsb(0.98, light: (0.50, 0.62), dark: (0.35, 0.92)),
        repo: { _ in Color(light: .black.opacity(0.7), dark: .white.opacity(0.7)) }
    )

    /// Classic system colors, and any hue on the wheel per repo.
    static let vivid = Palette(
        id: "vivid", name: "Vivid",
        accent: .pink, pass: .green, fail: .red, pending: .orange, agent: .purple,
        selection: .blue, added: .green, removed: .red,
        repo: { hsb(Double(stableHash($0) % 360) / 360, light: (0.75, 0.62), dark: (0.65, 0.95)) }
    )

    static let all = [cool, soft, focus, vivid]

    static func named(_ id: String) -> Palette {
        all.first { $0.id == id } ?? .cool
    }
}

/// Light, dark, or whatever macOS is set to.
enum Appearance: String, CaseIterable, Identifiable {
    case system, light, dark

    var id: Self { self }

    var name: String {
        switch self {
        case .system: "System"
        case .light: "Light"
        case .dark: "Dark"
        }
    }

    var nsAppearance: NSAppearance? {
        switch self {
        case .system: nil
        case .light: NSAppearance(named: .aqua)
        case .dark: NSAppearance(named: .darkAqua)
        }
    }
}

/// `UserDefaults` keys, shared by every view that reads or sets the theme.
enum ThemeKey {
    static let palette = "palette"
    static let appearance = "appearance"
}

private struct PaletteKey: EnvironmentKey {
    static let defaultValue = Palette.cool
}

extension EnvironmentValues {
    /// The chosen palette. Read it with `@Environment(\.palette)`.
    var palette: Palette {
        get { self[PaletteKey.self] }
        set { self[PaletteKey.self] = newValue }
    }
}

extension View {
    /// Applies the chosen palette and its accent. Put it on each window's root view.
    func themed() -> some View {
        modifier(Themed())
    }
}

private struct Themed: ViewModifier {
    @AppStorage(ThemeKey.palette) private var paletteID = Palette.cool.id
    @AppStorage(ThemeKey.appearance) private var appearance = Appearance.system

    func body(content: Content) -> some View {
        let palette = Palette.named(paletteID)
        content
            .environment(\.palette, palette)
            .tint(palette.accent)
            // App-wide rather than `.preferredColorScheme`, which doesn't switch back to System reliably.
            .task(id: appearance) { NSApp.appearance = appearance.nsAppearance }
    }
}

/// Toolbar button: click for the next palette, hold (or use the arrow) to pick a palette or appearance.
struct ThemeMenu: View {
    @AppStorage(ThemeKey.palette) private var paletteID = Palette.cool.id
    @AppStorage(ThemeKey.appearance) private var appearance = Appearance.system

    var body: some View {
        let current = Palette.named(paletteID)
        Menu {
            Picker("Palette", selection: $paletteID) {
                ForEach(Palette.all) { Text($0.name).tag($0.id) }
            }
            .pickerStyle(.inline)
            Picker("Appearance", selection: $appearance) {
                ForEach(Appearance.allCases) { Text($0.name).tag($0) }
            }
            .pickerStyle(.inline)
        } label: {
            Label("Theme", systemImage: "paintpalette")
        } primaryAction: {
            let index = Palette.all.firstIndex { $0.id == current.id } ?? 0
            paletteID = Palette.all[(index + 1) % Palette.all.count].id
        }
        .help("Colors: \(current.name), \(appearance.name). Click for the next palette, hold to choose")
    }
}

extension Color {
    /// `light` in light mode and `dark` in dark mode, following the view's appearance.
    init(light: Color, dark: Color) {
        self.init(nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? NSColor(dark) : NSColor(light)
        })
    }
}

/// One hue with separate (saturation, brightness) for light and dark mode.
private func hsb(_ hue: Double, light: (Double, Double), dark: (Double, Double)) -> Color {
    Color(light: Color(hue: hue, saturation: light.0, brightness: light.1),
          dark: Color(hue: hue, saturation: dark.0, brightness: dark.1))
}

private func hex(light: UInt32, dark: UInt32) -> Color {
    func color(_ value: UInt32) -> Color {
        Color(red: Double(value >> 16 & 0xFF) / 255, green: Double(value >> 8 & 0xFF) / 255,
              blue: Double(value & 0xFF) / 255)
    }
    return Color(light: color(light), dark: color(dark))
}

/// FNV-1a. `String.hashValue` changes on every launch, which would recolor repos each time.
private func stableHash(_ text: String) -> UInt64 {
    text.utf8.reduce(14_695_981_039_346_656_037) { ($0 ^ UInt64($1)) &* 1_099_511_628_211 }
}

import SwiftUI
import UIKit

extension ViewerAppearance {
    var preferredColorScheme: ColorScheme? {
        switch self {
        case .system: return nil
        case .light: return .light
        case .dark: return .dark
        }
    }
}

extension ViewerTheme {
    var fontDesign: Font.Design {
        self == .terminal ? .monospaced : .default
    }
}

private struct PaletteValues {
    let bg: UInt32
    let surface: UInt32
    let surfaceHi: UInt32
    let accent: UInt32
    let buttonInk: UInt32
    let onBg: UInt32
    let muted: UInt32
    let outline: UInt32
}

private extension UIColor {
    convenience init(rgb: UInt32) {
        self.init(
            red: CGFloat((rgb >> 16) & 0xFF) / 255,
            green: CGFloat((rgb >> 8) & 0xFF) / 255,
            blue: CGFloat(rgb & 0xFF) / 255,
            alpha: 1
        )
    }
}

/// The three native palettes shared with Android. UIKit's dynamic provider
/// resolves light/dark from the effective trait collection, while the selected
/// theme comes from the same device-local preference that AppModel publishes.
enum Palette {
    static var bg: Color { adaptive(\.bg) }
    static var surface: Color { adaptive(\.surface) }
    static var surfaceHi: Color { adaptive(\.surfaceHi) }
    static var accent: Color { adaptive(\.accent) }
    static var buttonInk: Color { adaptive(\.buttonInk) }
    static var onBg: Color { adaptive(\.onBg) }
    static var muted: Color { adaptive(\.muted) }
    static var outline: Color { adaptive(\.outline) }

    /// Playback stays true black in every theme and appearance. These surfaces
    /// sit over the movie, not the app's room chrome.
    static let playerChrome = Color(red: 0x0A / 255, green: 0x0A / 255, blue: 0x0C / 255)

    private static func adaptive(_ keyPath: KeyPath<PaletteValues, UInt32>) -> Color {
        Color(uiColor: UIColor { traits in
            let stored = UserDefaults.standard.string(forKey: "plurx.theme")
            let theme = ViewerTheme(rawValue: stored ?? "") ?? .noirr
            let values = values(theme: theme, dark: traits.userInterfaceStyle != .light)
            return UIColor(rgb: values[keyPath: keyPath])
        })
    }

    private static func values(theme: ViewerTheme, dark: Bool) -> PaletteValues {
        switch (theme, dark) {
        case (.classic, true):
            return PaletteValues(
                bg: 0x0E0F13, surface: 0x171922, surfaceHi: 0x1E2230,
                accent: 0x6EA8FE, buttonInk: 0x0E0F13, onBg: 0xE8EAF0,
                muted: 0x9AA2B4, outline: 0x2A2F3E
            )
        case (.classic, false):
            return PaletteValues(
                bg: 0xF6F7F9, surface: 0xFFFFFF, surfaceHi: 0xEEF1F6,
                accent: 0x2F6FE0, buttonInk: 0xFFFFFF, onBg: 0x1A1D24,
                muted: 0x5B6472, outline: 0xDDE2EA
            )
        case (.terminal, true):
            return PaletteValues(
                bg: 0x050705, surface: 0x0B100B, surfaceHi: 0x111A12,
                accent: 0x3FE170, buttonInk: 0x041508, onBg: 0xC9E8C0,
                muted: 0x7A9670, outline: 0x1E3020
            )
        case (.terminal, false):
            return PaletteValues(
                bg: 0xEEE8D5, surface: 0xFDF6E3, surfaceHi: 0xE6DFC8,
                accent: 0x6E7F00, buttonInk: 0xFDF6E3, onBg: 0x073642,
                muted: 0x657B83, outline: 0xD5CDB4
            )
        case (.noirr, true):
            return PaletteValues(
                bg: 0x0A0A0C, surface: 0x101014, surfaceHi: 0x16161B,
                accent: 0xE5484D, buttonInk: 0x0A0A0C, onBg: 0xEDEDEF,
                muted: 0x9A9AA3, outline: 0x29292E
            )
        case (.noirr, false):
            return PaletteValues(
                bg: 0xF2EFE8, surface: 0xFAF8F2, surfaceHi: 0xFFFFFF,
                accent: 0xC2343A, buttonInk: 0xFFFFFF, onBg: 0x1A1A1E,
                muted: 0x5D5C63, outline: 0xDEDAD2
            )
        }
    }
}

#if os(tvOS)
/// tvOS 26 can apply a button's tint to both its glass and its label, making a
/// muted bordered action look blank. This style owns both sides of the
/// contrast pair. Focus is expressed with the plurx red signal, lift, and glow
/// instead of turning the entire control into a bright white plate.
struct TVReadableButtonStyle: ButtonStyle {
    let prominent: Bool

    static func foregroundColor(prominent: Bool, focused: Bool) -> Color {
        prominent ? Palette.buttonInk : Palette.onBg
    }

    static func backgroundColor(prominent: Bool, focused: Bool) -> Color {
        return prominent ? Palette.accent : Palette.surfaceHi
    }

    func makeBody(configuration: Configuration) -> Body {
        Body(configuration: configuration, prominent: prominent)
    }

    struct Body: View {
        let configuration: ButtonStyle.Configuration
        let prominent: Bool
        @Environment(\.isFocused) private var isFocused
        @Environment(\.isEnabled) private var isEnabled

        var body: some View {
            configuration.label
                .foregroundStyle(
                    TVReadableButtonStyle.foregroundColor(
                        prominent: prominent,
                        focused: isFocused
                    )
                )
                .padding(.horizontal, 28)
                .padding(.vertical, 16)
                .frame(minHeight: 66)
                .background(
                    TVReadableButtonStyle.backgroundColor(
                        prominent: prominent,
                        focused: isFocused
                    ),
                    in: RoundedRectangle(cornerRadius: 14, style: .continuous)
                )
                .overlay {
                    RoundedRectangle(cornerRadius: 14, style: .continuous)
                        .stroke(
                            isFocused ? Palette.accent : Palette.outline,
                            lineWidth: isFocused ? 4 : 1
                        )
                }
                .scaleEffect(isFocused ? 1.045 : (configuration.isPressed ? 0.98 : 1))
                .shadow(color: Palette.accent.opacity(isFocused ? 0.3 : 0), radius: 16, y: 7)
                .opacity(isEnabled ? 1 : 0.45)
                .animation(.easeOut(duration: 0.14), value: isFocused)
                .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
        }
    }
}

/// Compact trailing action for shelf headers. tvOS 26 can tint a default
/// NavigationLink's label and background identically, leaving only an empty
/// accent-colored capsule. This style owns that contrast pair explicitly.
struct TVShelfActionButtonStyle: ButtonStyle {
    static func foregroundColor(focused: Bool) -> Color { Palette.buttonInk }
    static func backgroundColor(focused: Bool) -> Color { Palette.accent }

    func makeBody(configuration: Configuration) -> Body {
        Body(configuration: configuration)
    }

    struct Body: View {
        let configuration: ButtonStyle.Configuration
        @Environment(\.isFocused) private var isFocused

        var body: some View {
            configuration.label
                .foregroundStyle(TVShelfActionButtonStyle.foregroundColor(focused: isFocused))
                .padding(.horizontal, 22)
                .padding(.vertical, 10)
                .frame(minWidth: 126, minHeight: 48)
                .background(
                    TVShelfActionButtonStyle.backgroundColor(focused: isFocused),
                    in: Capsule()
                )
                .brightness(isFocused ? 0.08 : 0)
                .scaleEffect(isFocused ? 1.07 : (configuration.isPressed ? 0.97 : 1))
                .shadow(
                    color: Palette.accent.opacity(isFocused ? 0.42 : 0),
                    radius: 14,
                    y: 6
                )
                .animation(.easeOut(duration: 0.14), value: isFocused)
                .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
        }
    }
}

/// The player chrome should stay subordinate to the picture. These controls
/// keep a compact, dark footprint and use only a hairline accent when focused
/// instead of tvOS's large white focus plate.
struct TVPlayerControlButtonStyle: ButtonStyle {
    static let width: CGFloat = 54
    static let height: CGFloat = 46

    func makeBody(configuration: Configuration) -> Body {
        Body(configuration: configuration)
    }

    struct Body: View {
        let configuration: ButtonStyle.Configuration
        @Environment(\.isFocused) private var isFocused
        @Environment(\.isEnabled) private var isEnabled

        var body: some View {
            configuration.label
                .font(.system(size: 23, weight: .semibold))
                .foregroundStyle(.white)
                .frame(
                    width: TVPlayerControlButtonStyle.width,
                    height: TVPlayerControlButtonStyle.height
                )
                .background(
                    Palette.playerChrome.opacity(isFocused ? 0.9 : 0.68),
                    in: RoundedRectangle(cornerRadius: 11, style: .continuous)
                )
                .overlay {
                    RoundedRectangle(cornerRadius: 11, style: .continuous)
                        .stroke(
                            isFocused ? Palette.accent.opacity(0.95) : Palette.outline.opacity(0.7),
                            lineWidth: isFocused ? 1 : 0.5
                        )
                }
                .scaleEffect(isFocused ? 1.035 : (configuration.isPressed ? 0.97 : 1))
                .shadow(color: .black.opacity(isFocused ? 0.55 : 0), radius: 8, y: 4)
                .opacity(isEnabled ? 1 : 0.42)
                .animation(.easeOut(duration: 0.12), value: isFocused)
                .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
        }
    }
}

/// Three concentric sub-two-point strokes make the progress focus indicator a
/// red hairline whose two edges fall back into black.
enum TVPlayerProgressFocusRing {
    static let outerStrokeWidth: CGFloat = 1.5
    static let fadeStrokeWidth: CGFloat = 0.9
    static let accentStrokeWidth: CGFloat = 0.3
}

/// Opaque player chrome is limited to the pixels immediately surrounding copy.
/// The timeline and controls already carry their own contrast and need no
/// full-width material panel behind them.
enum TVPlayerChromeMetrics {
    static let headerHorizontalInset: CGFloat = 8
    static let headerVerticalInset: CGFloat = 5
    static let timeHorizontalInset: CGFloat = 6
    static let timeVerticalInset: CGFloat = 3
    static let infoHeadingFontSize: CGFloat = 13
    static let infoBodyFontSize: CGFloat = 19
    static let infoLineLimit = 8
}

/// Replaces tvOS's thick white focus plate with the same restrained red
/// signal used throughout plurx. The hero still lifts enough to read from the
/// couch without turning the artwork into a glowing white rectangle.
struct TVHeroButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> Body {
        Body(configuration: configuration)
    }

    struct Body: View {
        let configuration: ButtonStyle.Configuration
        @Environment(\.isFocused) private var isFocused

        var body: some View {
            configuration.label
                .overlay {
                    RoundedRectangle(cornerRadius: 24, style: .continuous)
                        .stroke(Palette.accent.opacity(isFocused ? 0.95 : 0.18),
                                lineWidth: isFocused ? 4 : 1)
                }
                .scaleEffect(isFocused ? 1.012 : (configuration.isPressed ? 0.994 : 1))
                .shadow(
                    color: Palette.accent.opacity(isFocused ? 0.22 : 0),
                    radius: 20,
                    y: 8
                )
                .animation(.easeOut(duration: 0.15), value: isFocused)
                .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
        }
    }
}

/// Shelf cards get a subtle lift and shadow, not the default opaque tvOS
/// surround that hides the edges of posters and backdrops.
struct TVMediaCardButtonStyle: ButtonStyle {
    // Strokes share one center line, so progressively narrower layers produce
    // a symmetric black → muted red → signal red → muted red → black
    // cross-section. Keep the complete surround narrow enough to read as a
    // selection ring rather than a frame around the card.
    static let outerStrokeWidth: CGFloat = 6
    static let fadeStrokeWidth: CGFloat = 4
    static let accentStrokeWidth: CGFloat = 2
    static let contentClearance: CGFloat = 7

    func makeBody(configuration: Configuration) -> Body {
        Body(configuration: configuration)
    }

    struct Body: View {
        let configuration: ButtonStyle.Configuration
        @Environment(\.isFocused) private var isFocused

        var body: some View {
            let shape = RoundedRectangle(cornerRadius: 13, style: .continuous)
            let ringShape = shape.inset(by: -TVMediaCardButtonStyle.contentClearance)
            configuration.label
                .background(Palette.bg.opacity(isFocused ? 0.96 : 0), in: shape)
                .overlay {
                    ZStack {
                        ringShape.stroke(
                            .black.opacity(0.96),
                            lineWidth: TVMediaCardButtonStyle.outerStrokeWidth
                        )
                        ringShape.stroke(
                            Palette.accent.opacity(0.32),
                            lineWidth: TVMediaCardButtonStyle.fadeStrokeWidth
                        )
                        ringShape.stroke(
                            Palette.accent.opacity(0.95),
                            lineWidth: TVMediaCardButtonStyle.accentStrokeWidth
                        )
                    }
                    .opacity(isFocused ? 1 : 0)
                }
                .scaleEffect(isFocused ? 1.035 : (configuration.isPressed ? 0.985 : 1))
                .shadow(color: .black.opacity(isFocused ? 0.72 : 0), radius: 19, y: 10)
                .animation(.easeOut(duration: 0.14), value: isFocused)
                .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
        }
    }
}
#endif

#if os(iOS)
/// Full-width touch actions own their complete shape. The system bordered
/// styles add horizontal chrome outside an already-flexible label, which can
/// paint past a compact detail column's trailing inset.
struct IOSFullWidthActionButtonStyle: ButtonStyle {
    let prominent: Bool

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(prominent ? Color.white : Palette.onBg.opacity(0.7))
            .frame(maxWidth: .infinity, minHeight: 24)
            .padding(.vertical, 12)
            .background(
                prominent ? Palette.accent : Palette.surfaceHi,
                in: Capsule()
            )
            .overlay {
                if !prominent {
                    Capsule().stroke(Palette.outline, lineWidth: 1)
                }
            }
            .contentShape(Capsule())
            .opacity(configuration.isPressed ? 0.76 : 1)
    }
}
#endif

extension View {
    /// Poster/link focus treatment: the tvOS "card" lift on focus, a plain
    /// (untinted) button on touch platforms.
    @ViewBuilder
    func posterButtonStyle() -> some View {
        #if os(tvOS)
        self
            .buttonStyle(TVMediaCardButtonStyle())
            .focusEffectDisabled()
        #else
        self.buttonStyle(.plain)
        #endif
    }

    /// Large featured-card treatment kept separate from shelf cards so its
    /// focus indication follows the hero's rounded artwork bounds.
    @ViewBuilder
    func featuredButtonStyle() -> some View {
        #if os(tvOS)
        self
            .buttonStyle(TVHeroButtonStyle())
            .focusEffectDisabled()
        #else
        self.buttonStyle(.plain)
        #endif
    }

    /// Keeps shelf-header navigation readable under tvOS's tint and focus
    /// transformations while retaining a simple plain link on touch devices.
    @ViewBuilder
    func shelfActionButtonStyle() -> some View {
        #if os(tvOS)
        self
            .buttonStyle(TVShelfActionButtonStyle())
            .focusEffectDisabled()
        #else
        self.buttonStyle(.plain)
        #endif
    }

    /// Rounded-border text field on iOS; tvOS has no such style (its fields are
    /// focus-driven), so leave the default there.
    @ViewBuilder
    func plurxFieldStyle() -> some View {
        #if os(tvOS)
        self
        #else
        self.textFieldStyle(.roundedBorder)
        #endif
    }
}

// MARK: - Small formatting helpers

func progressFraction(_ watch: Watch?, runtimeMs: Int?) -> Double {
    guard let watch, let pos = watch.positionMs else { return 0 }
    let dur = watch.durationMs ?? runtimeMs ?? 0
    guard dur > 0 else { return 0 }
    return min(max(Double(pos) / Double(dur), 0), 1)
}

/// `m:ss` or `h:mm:ss` for scrubber / resume labels.
func formatTime(_ ms: Int) -> String {
    guard ms > 0 else { return "0:00" }
    let total = ms / 1000
    let h = total / 3600, m = (total % 3600) / 60, s = total % 60
    return h > 0 ? String(format: "%d:%02d:%02d", h, m, s) : String(format: "%d:%02d", m, s)
}

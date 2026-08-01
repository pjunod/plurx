import SwiftUI

/// noirr palette — the ink-dark base and signal-red accent shared with the web
/// and Android clients.
enum Palette {
    static let bg = Color(red: 0x0A / 255, green: 0x0A / 255, blue: 0x0C / 255)
    static let surface = Color(red: 0x14 / 255, green: 0x14 / 255, blue: 0x18 / 255)
    static let surfaceHi = Color(red: 0x1C / 255, green: 0x1C / 255, blue: 0x22 / 255)
    static let accent = Color(red: 0xE5 / 255, green: 0x48 / 255, blue: 0x4D / 255)
    static let onBg = Color(red: 0xEC / 255, green: 0xEC / 255, blue: 0xEF / 255)
    static let muted = Color(red: 0x8A / 255, green: 0x8A / 255, blue: 0x94 / 255)
    static let outline = Color(red: 0x2A / 255, green: 0x2A / 255, blue: 0x31 / 255)
}

#if os(tvOS)
/// tvOS 26 can apply a button's tint to both its glass and its label, making a
/// muted bordered action look blank. This style owns both sides of the
/// contrast pair. Focus is expressed with the plurx red signal, lift, and glow
/// instead of turning the entire control into a bright white plate.
struct TVReadableButtonStyle: ButtonStyle {
    let prominent: Bool

    static func foregroundColor(prominent: Bool, focused: Bool) -> Color {
        prominent ? Palette.bg : Palette.onBg
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
    func makeBody(configuration: Configuration) -> Body {
        Body(configuration: configuration)
    }

    struct Body: View {
        let configuration: ButtonStyle.Configuration
        @Environment(\.isFocused) private var isFocused

        var body: some View {
            configuration.label
                .scaleEffect(isFocused ? 1.045 : (configuration.isPressed ? 0.985 : 1))
                .shadow(color: .black.opacity(isFocused ? 0.6 : 0), radius: 18, y: 10)
                .overlay(alignment: .top) {
                    Capsule()
                        .fill(Palette.accent)
                        .frame(width: 42, height: 4)
                        .offset(y: -10)
                        .opacity(isFocused ? 1 : 0)
                }
                .animation(.easeOut(duration: 0.14), value: isFocused)
                .animation(.easeOut(duration: 0.08), value: configuration.isPressed)
        }
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

func mediaSubtitle(_ item: Item) -> String {
    if item.kind == "episode", let s = item.seasonNumber, let e = item.episodeNumber {
        return "S\(s)·E\(e)"
    }
    if let y = item.year { return String(y) }
    if let show = item.showTitle { return show }
    return item.kind.prefix(1).uppercased() + item.kind.dropFirst()
}

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

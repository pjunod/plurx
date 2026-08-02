package tv.plurx.app.data

enum class ThemeId(val storageValue: String, val label: String) {
    Classic("classic", "Classic"),
    Terminal("terminal", "Terminal"),
    Noirr("noirr", "noirr");

    companion object {
        fun fromStorage(value: String?): ThemeId = entries.firstOrNull {
            it.storageValue == value
        } ?: Classic
    }
}

enum class Appearance(val storageValue: String, val label: String) {
    System("system", "Auto (system)"),
    Light("light", "Light"),
    Dark("dark", "Dark");

    companion object {
        fun fromStorage(value: String?): Appearance = entries.firstOrNull {
            it.storageValue == value
        } ?: System
    }
}

enum class PosterSize(val storageValue: String, val label: String, val widthDp: Int) {
    Small("small", "Small", 112),
    Medium("medium", "Medium", 136),
    Large("large", "Large", 164),
    ExtraLarge("extra_large", "Extra large", 196);

    companion object {
        fun fromStorage(value: String?): PosterSize = entries.firstOrNull {
            it.storageValue == value
        } ?: Medium
    }
}

enum class HomeGrouping(val storageValue: String, val label: String) {
    Category("category", "Category"),
    Library("library", "Share name");

    companion object {
        fun fromStorage(value: String?): HomeGrouping = entries.firstOrNull {
            it.storageValue == value
        } ?: Category
    }
}

/**
 * The stored quality preference. The *menu* is built from the server's
 * advertised ladder (see `qualityOptions`); this enum is the storage vocabulary
 * and the fallback menu for a server too old to send one.
 */
enum class PlaybackQuality(val storageValue: String, val label: String) {
    Auto("auto", "Auto"),
    Original("original", "Original"),
    Q2160("2160", "4K · 2160p"),
    Q1440("1440", "1440p"),
    Q1080("1080", "1080p"),
    Q720("720", "720p"),
    Q480("480", "480p"),
    Q360("360", "360p");

    /** The transcode rung this preference names, or null for Auto/Original. */
    val rungHeight: Int? get() = storageValue.toIntOrNull()

    companion object {
        fun fromStorage(value: String?): PlaybackQuality = entries.firstOrNull {
            it.storageValue == value
        } ?: Auto
    }
}

data class ViewerPreferences(
    val theme: ThemeId = ThemeId.Classic,
    val appearance: Appearance = Appearance.System,
    val posterSize: PosterSize = PosterSize.Medium,
    val homeGrouping: HomeGrouping = HomeGrouping.Category,
    val playbackQuality: PlaybackQuality = PlaybackQuality.Auto,
    val autoSkip: Boolean = false,
    val autoplayNext: Boolean = true,
)

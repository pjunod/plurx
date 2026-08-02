package tv.plurx.app.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

private val Context.dataStore: DataStore<Preferences> by preferencesDataStore(name = "plurx")

/**
 * Small persisted state: the last server + token (so the app reconnects
 * silently on launch) and the default audio/subtitle languages. The languages
 * mirror the server's Settings → Playback defaults and feed ExoPlayer's track
 * selector so the right embedded track is chosen on direct play — English out
 * of the box.
 */
class SettingsStore(private val context: Context) {

    data class Saved(
        val origin: String = "",
        val instanceId: String? = null,
        val token: String? = null,
        val username: String? = null,
        val audioLang: String = "eng",
        val subLang: String = "eng",
        val preferences: ViewerPreferences = ViewerPreferences(),
    )

    private object Keys {
        val ORIGIN = stringPreferencesKey("origin")
        val INSTANCE_ID = stringPreferencesKey("instance_id")
        val TOKEN = stringPreferencesKey("token")
        val USERNAME = stringPreferencesKey("username")
        val AUDIO_LANG = stringPreferencesKey("audio_lang")
        val SUB_LANG = stringPreferencesKey("sub_lang")
        val THEME = stringPreferencesKey("theme")
        val APPEARANCE = stringPreferencesKey("appearance")
        val POSTER_SIZE = stringPreferencesKey("poster_size")
        val HOME_GROUPING = stringPreferencesKey("home_grouping")
        val PLAYBACK_QUALITY = stringPreferencesKey("playback_quality")
        val AUTO_SKIP = booleanPreferencesKey("auto_skip")
        val AUTOPLAY_NEXT = booleanPreferencesKey("autoplay_next")
    }

    val flow: Flow<Saved> = context.dataStore.data.map { p ->
        Saved(
            origin = p[Keys.ORIGIN] ?: "",
            instanceId = p[Keys.INSTANCE_ID],
            token = p[Keys.TOKEN],
            username = p[Keys.USERNAME],
            audioLang = p[Keys.AUDIO_LANG] ?: "eng",
            subLang = p[Keys.SUB_LANG] ?: "eng",
            preferences = ViewerPreferences(
                theme = ThemeId.fromStorage(p[Keys.THEME]),
                appearance = Appearance.fromStorage(p[Keys.APPEARANCE]),
                posterSize = PosterSize.fromStorage(p[Keys.POSTER_SIZE]),
                homeGrouping = HomeGrouping.fromStorage(p[Keys.HOME_GROUPING]),
                playbackQuality = PlaybackQuality.fromStorage(p[Keys.PLAYBACK_QUALITY]),
                autoSkip = p[Keys.AUTO_SKIP] ?: false,
                autoplayNext = p[Keys.AUTOPLAY_NEXT] ?: true,
            ),
        )
    }

    /**
     * Point the app at a server the *user* chose. The only writer of a new
     * origin, and therefore the one owner of the pairing invariant: a token
     * belongs to the origin that issued it, so a different origin drops it in
     * the same write. Without that, killing the app between connecting to B
     * and logging in sends server A's bearer to server B in clear text — and
     * A's still-valid session dies when B answers 401.
     */
    suspend fun saveOrigin(origin: String, instanceId: String?) {
        context.dataStore.edit { p ->
            val kept = credentialsForNewOrigin(
                stored = StoredCredentials(p[Keys.ORIGIN], p[Keys.TOKEN], p[Keys.USERNAME]),
                origin = origin,
            )
            p[Keys.ORIGIN] = origin
            if (instanceId == null) p.remove(Keys.INSTANCE_ID) else p[Keys.INSTANCE_ID] = instanceId
            kept.token?.let { p[Keys.TOKEN] = it } ?: p.remove(Keys.TOKEN)
            kept.username?.let { p[Keys.USERNAME] = it } ?: p.remove(Keys.USERNAME)
        }
    }

    /**
     * Record where a *known* server now lives — Bonjour recovery found the
     * same instance on a new address. The token still belongs to it, so this
     * deliberately keeps it; that is the whole difference from [saveOrigin].
     */
    suspend fun saveServerIdentity(origin: String, instanceId: String?) {
        context.dataStore.edit { p ->
            p[Keys.ORIGIN] = origin
            if (instanceId == null) p.remove(Keys.INSTANCE_ID) else p[Keys.INSTANCE_ID] = instanceId
        }
    }

    /** Forget the server entirely — origin, identity, and credentials in one write. */
    suspend fun clearServer() {
        context.dataStore.edit { p ->
            p.remove(Keys.ORIGIN)
            p.remove(Keys.INSTANCE_ID)
            p.remove(Keys.TOKEN)
            p.remove(Keys.USERNAME)
        }
    }

    suspend fun saveSession(origin: String, token: String, username: String) {
        context.dataStore.edit { p ->
            p[Keys.ORIGIN] = origin
            p[Keys.TOKEN] = token
            p[Keys.USERNAME] = username
        }
    }

    /** Drop the token (sign out) but keep the origin so the login screen is pre-filled. */
    suspend fun clearToken() {
        context.dataStore.edit { it.remove(Keys.TOKEN) }
    }

    suspend fun saveLangs(audio: String, sub: String) {
        context.dataStore.edit { p ->
            p[Keys.AUDIO_LANG] = audio
            p[Keys.SUB_LANG] = sub
        }
    }

    suspend fun saveViewerPreferences(value: ViewerPreferences) {
        context.dataStore.edit { p ->
            p[Keys.THEME] = value.theme.storageValue
            p[Keys.APPEARANCE] = value.appearance.storageValue
            p[Keys.POSTER_SIZE] = value.posterSize.storageValue
            p[Keys.HOME_GROUPING] = value.homeGrouping.storageValue
            p[Keys.PLAYBACK_QUALITY] = value.playbackQuality.storageValue
            p[Keys.AUTO_SKIP] = value.autoSkip
            p[Keys.AUTOPLAY_NEXT] = value.autoplayNext
        }
    }
}

/** The credential half of a persisted server record. */
internal data class StoredCredentials(
    val origin: String?,
    val token: String?,
    val username: String?,
)

/**
 * What survives pointing the app at [origin]. Credentials survive only when
 * the origin is unchanged; anything else is a different server, and its
 * predecessor's bearer must never reach it.
 */
internal fun credentialsForNewOrigin(
    stored: StoredCredentials,
    origin: String,
): StoredCredentials {
    val sameServer = !stored.origin.isNullOrBlank() && stored.origin == origin
    return if (sameServer) {
        StoredCredentials(origin, stored.token, stored.username)
    } else {
        StoredCredentials(origin, null, null)
    }
}

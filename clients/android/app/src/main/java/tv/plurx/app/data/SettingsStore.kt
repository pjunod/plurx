package tv.plurx.app.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
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
        val token: String? = null,
        val username: String? = null,
        val audioLang: String = "eng",
        val subLang: String = "eng",
    )

    private object Keys {
        val ORIGIN = stringPreferencesKey("origin")
        val TOKEN = stringPreferencesKey("token")
        val USERNAME = stringPreferencesKey("username")
        val AUDIO_LANG = stringPreferencesKey("audio_lang")
        val SUB_LANG = stringPreferencesKey("sub_lang")
    }

    val flow: Flow<Saved> = context.dataStore.data.map { p ->
        Saved(
            origin = p[Keys.ORIGIN] ?: "",
            token = p[Keys.TOKEN],
            username = p[Keys.USERNAME],
            audioLang = p[Keys.AUDIO_LANG] ?: "eng",
            subLang = p[Keys.SUB_LANG] ?: "eng",
        )
    }

    suspend fun saveOrigin(origin: String) {
        context.dataStore.edit { it[Keys.ORIGIN] = origin }
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
}

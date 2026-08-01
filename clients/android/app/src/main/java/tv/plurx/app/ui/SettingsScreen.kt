package tv.plurx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.data.Appearance
import tv.plurx.app.data.HomeGrouping
import tv.plurx.app.data.PlaybackQuality
import tv.plurx.app.data.PosterSize
import tv.plurx.app.data.ThemeId
import tv.plurx.app.ui.components.ChoicePicker
import tv.plurx.app.ui.components.SafeTopRow
import tv.plurx.app.ui.theme.Muted

private val LANGS = listOf(
    "eng" to "English", "jpn" to "Japanese", "spa" to "Spanish", "fre" to "French",
    "ger" to "German", "ita" to "Italian", "por" to "Portuguese", "kor" to "Korean",
    "chi" to "Chinese", "rus" to "Russian", "hin" to "Hindi", "ara" to "Arabic",
)
private val SUB_LANGS = listOf("off" to "Off") + LANGS

@Composable
fun SettingsScreen(vm: AppViewModel, onBack: () -> Unit) {
    val preferences by vm.preferences.collectAsStateWithLifecycle()
    val formFactor = currentFormFactor()
    val side = formFactor.horizontalPadding()
    var audio by remember { mutableStateOf(LANGS.firstOrNull { it.first == vm.audioLang } ?: LANGS.first()) }
    var sub by remember { mutableStateOf(SUB_LANGS.firstOrNull { it.first == vm.subLang } ?: SUB_LANGS.first()) }

    Column(Modifier.fillMaxSize().navigationBarsPadding()) {
        SafeTopRow(
            Modifier.fillMaxWidth().padding(start = side - 12.dp, end = side, top = 8.dp),
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text("Settings", style = MaterialTheme.typography.titleLarge)
        }

        val sectionModifier = Modifier.weight(1f)
        val content: @Composable () -> Unit = {
            SettingsSection("Appearance", "Theme and room brightness are independent, matching the web viewer.") {
                ChoicePicker("Theme", preferences.theme, ThemeId.entries, { it.label }, vm::setTheme)
                ChoicePicker("Appearance", preferences.appearance, Appearance.entries, { it.label }, vm::setAppearance)
                ChoicePicker("Poster size", preferences.posterSize, PosterSize.entries, { it.label }, vm::setPosterSize)
                ChoicePicker("Home grouping", preferences.homeGrouping, HomeGrouping.entries, { it.label }, vm::setHomeGrouping)
            }
        }
        val playback: @Composable () -> Unit = {
            SettingsSection("Playback defaults", "Applied to new playback sessions and remembered on this device.") {
                ChoicePicker("Quality", preferences.playbackQuality, PlaybackQuality.entries, { it.label }, vm::setPlaybackQuality)
                ChoicePicker("Audio language", audio, LANGS, { it.second }, onSelect = {
                    audio = it
                    vm.setLanguages(audio.first, sub.first)
                })
                ChoicePicker("Subtitle language", sub, SUB_LANGS, { it.second }, onSelect = {
                    sub = it
                    vm.setLanguages(audio.first, sub.first)
                })
                PreferenceSwitch("Auto-skip intro and credits", preferences.autoSkip, vm::setAutoSkip)
                PreferenceSwitch("Autoplay next episode", preferences.autoplayNext, vm::setAutoplayNext)
            }
        }

        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = side, vertical = 16.dp),
            verticalArrangement = Arrangement.spacedBy(28.dp),
        ) {
            if (formFactor == FormFactor.Compact) {
                content()
                playback()
            } else {
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(32.dp)) {
                    Column(sectionModifier) { content() }
                    Column(sectionModifier) { playback() }
                }
            }

            SettingsSection("Account", null) {
                Text(
                    "Signed in as ${vm.username ?: "—"} on ${vm.serverName ?: vm.origin}",
                    color = Muted,
                    style = MaterialTheme.typography.bodyMedium,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    OutlinedButton(onClick = vm::logout) { Text("Sign out") }
                    OutlinedButton(onClick = vm::changeServer) { Text("Change server") }
                }
            }
        }
    }
}

@Composable
private fun SettingsSection(
    title: String,
    description: String?,
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
        Text(title, style = MaterialTheme.typography.titleMedium)
        description?.let { Text(it, color = Muted, style = MaterialTheme.typography.bodyMedium) }
        content()
    }
}

@Composable
private fun PreferenceSwitch(label: String, checked: Boolean, onCheckedChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Text(label, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
        Switch(checked = checked, onCheckedChange = onCheckedChange)
    }
}

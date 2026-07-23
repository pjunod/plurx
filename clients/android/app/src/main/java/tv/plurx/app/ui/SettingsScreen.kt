package tv.plurx.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted
import tv.plurx.app.ui.theme.Outline
import tv.plurx.app.ui.theme.SurfaceHi

// Common audio/subtitle languages (ISO 639-2/B, matching the server's codes).
private val LANGS = listOf(
    "eng" to "English", "jpn" to "Japanese", "spa" to "Spanish", "fre" to "French",
    "ger" to "German", "ita" to "Italian", "por" to "Portuguese", "kor" to "Korean",
    "chi" to "Chinese", "rus" to "Russian", "hin" to "Hindi", "ara" to "Arabic",
)
private val SUB_LANGS = listOf("off" to "Off") + LANGS

private fun labelFor(code: String, options: List<Pair<String, String>>): String =
    options.firstOrNull { it.first == code }?.second ?: code

@Composable
fun SettingsScreen(vm: AppViewModel, onBack: () -> Unit) {
    var audio by remember { mutableStateOf(vm.audioLang) }
    var sub by remember { mutableStateOf(vm.subLang) }

    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(start = 4.dp, top = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text("Settings", style = MaterialTheme.typography.titleLarge)
        }

        Column(
            Modifier.padding(20.dp).fillMaxWidth(),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            Text("Playback defaults", style = MaterialTheme.typography.titleMedium)
            Text(
                "Preferred tracks when a title has more than one. Applied on the fly for direct play.",
                color = Muted,
                style = MaterialTheme.typography.labelMedium,
            )

            LanguagePicker("Audio language", audio, LANGS) {
                audio = it
                vm.setLanguages(audio, sub)
            }
            LanguagePicker("Subtitle language", sub, SUB_LANGS) {
                sub = it
                vm.setLanguages(audio, sub)
            }

            Box(Modifier.padding(top = 8.dp)) {
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text("Account", style = MaterialTheme.typography.titleMedium)
                    Text(
                        "Signed in as ${vm.username ?: "—"} on ${vm.serverName ?: vm.origin}",
                        color = Muted,
                        style = MaterialTheme.typography.labelMedium,
                    )
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedButton(onClick = { vm.logout() }) { Text("Sign out") }
                OutlinedButton(onClick = { vm.changeServer() }) { Text("Change server") }
            }
        }
    }
}

@Composable
private fun LanguagePicker(
    label: String,
    value: String,
    options: List<Pair<String, String>>,
    onSelect: (String) -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    Column {
        Text(label, color = Muted, style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(bottom = 6.dp))
        Box {
            Row(
                Modifier
                    .clip(RoundedCornerShape(8.dp))
                    .background(SurfaceHi)
                    .border(1.dp, Outline, RoundedCornerShape(8.dp))
                    .clickable { open = true }
                    .padding(horizontal = 16.dp, vertical = 14.dp)
                    .width(240.dp),
            ) {
                Text(labelFor(value, options), fontWeight = FontWeight.SemiBold)
            }
            DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                options.forEach { (code, name) ->
                    DropdownMenuItem(
                        text = { Text(name, color = if (code == value) Accent else MaterialTheme.colorScheme.onSurface) },
                        onClick = { onSelect(code); open = false },
                    )
                }
            }
        }
    }
}

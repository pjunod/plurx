package tv.plurx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted

@Composable
private fun AuthScaffold(
    subtitle: String,
    error: String?,
    content: @Composable androidx.compose.foundation.layout.ColumnScope.() -> Unit,
) {
    Box(Modifier.fillMaxSize().padding(24.dp), contentAlignment = Alignment.Center) {
        Column(
            modifier = Modifier.widthIn(max = 420.dp).fillMaxWidth(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text("plurx", fontSize = 40.sp, color = Accent, style = MaterialTheme.typography.headlineMedium)
            Text(subtitle, color = Muted, style = MaterialTheme.typography.bodyMedium)
            content()
            if (error != null) {
                Text(error, color = Accent, style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

@Composable
fun ConnectScreen(vm: AppViewModel, busy: Boolean, error: String?) {
    var url by rememberSaveable { mutableStateOf(vm.origin) }
    AuthScaffold("Connect to your server", error) {
        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            label = { Text("Server address") },
            placeholder = { Text("192.168.1.10:32600") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Go),
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            onClick = { vm.connect(url) },
            enabled = !busy && url.isNotBlank(),
            modifier = Modifier.fillMaxWidth(),
        ) {
            if (busy) CircularProgressIndicator(Modifier.padding(2.dp), strokeWidth = 2.dp, color = androidx.compose.ui.graphics.Color.White)
            else Text("Connect")
        }
        Text(
            "Enter the address shown in plurx → Settings, e.g. http://192.168.1.10:32600",
            color = Muted,
            style = MaterialTheme.typography.labelMedium,
        )
    }
}

@Composable
fun LoginScreen(vm: AppViewModel, busy: Boolean, error: String?) {
    var user by rememberSaveable { mutableStateOf(vm.username ?: "") }
    var pass by remember { mutableStateOf("") }
    AuthScaffold(vm.serverName ?: vm.origin, error) {
        OutlinedTextField(
            value = user,
            onValueChange = { user = it },
            label = { Text("Username") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Next),
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = pass,
            onValueChange = { pass = it },
            label = { Text("Password") },
            singleLine = true,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, imeAction = ImeAction.Go),
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            onClick = { vm.login(user, pass) },
            enabled = !busy && user.isNotBlank() && pass.isNotBlank(),
            modifier = Modifier.fillMaxWidth(),
        ) {
            if (busy) CircularProgressIndicator(Modifier.padding(2.dp), strokeWidth = 2.dp, color = androidx.compose.ui.graphics.Color.White)
            else Text("Sign in")
        }
        TextButton(onClick = { vm.changeServer() }) { Text("Use a different server", color = Muted) }
    }
}

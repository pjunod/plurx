package tv.plurx.app.ui

import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import tv.plurx.app.ui.theme.Accent
import tv.plurx.app.ui.theme.Muted

private const val LocalNetworkPermission = "android.permission.ACCESS_LOCAL_NETWORK"

@Composable
private fun AuthScaffold(
    subtitle: String,
    error: String?,
    content: @Composable androidx.compose.foundation.layout.ColumnScope.() -> Unit,
) {
    Box(
        Modifier
            .fillMaxSize()
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .imePadding()
            .padding(24.dp),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            modifier = Modifier.widthIn(max = 420.dp).fillMaxWidth().verticalScroll(rememberScrollState()),
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
    val context = LocalContext.current
    val discovery by vm.serverDiscovery.collectAsStateWithLifecycle()
    var url by rememberSaveable { mutableStateOf(vm.origin) }
    var showManual by rememberSaveable { mutableStateOf(vm.origin.isNotBlank()) }
    var permissionDenied by rememberSaveable { mutableStateOf(false) }

    fun hasLocalNetworkPermission(): Boolean = Build.VERSION.SDK_INT < 37 ||
        ContextCompat.checkSelfPermission(context, LocalNetworkPermission) ==
        PackageManager.PERMISSION_GRANTED

    val permissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        permissionDenied = !granted
        if (granted) vm.startServerDiscovery()
    }

    LaunchedEffect(Unit) {
        if (hasLocalNetworkPermission()) {
            vm.startServerDiscovery()
        } else {
            permissionLauncher.launch(LocalNetworkPermission)
        }
    }
    DisposableEffect(Unit) {
        onDispose { vm.stopServerDiscovery() }
    }

    AuthScaffold("Servers on your network", error) {
        if (discovery.servers.isEmpty()) {
            Row(
                modifier = Modifier.padding(vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.Center,
            ) {
                if (discovery.isSearching) {
                    CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                    Spacer(Modifier.width(10.dp))
                }
                Text(
                    if (discovery.isSearching) "Looking for plurx…" else "No servers found",
                    color = Muted,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
        } else {
            Text(
                if (discovery.servers.size == 1) "1 plurx server found"
                else "${discovery.servers.size} plurx servers found",
                color = Muted,
                style = MaterialTheme.typography.labelMedium,
            )
            discovery.servers.forEach { server ->
                OutlinedButton(
                    onClick = { vm.connect(server) },
                    enabled = !busy,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Column(
                        modifier = Modifier.weight(1f),
                        horizontalAlignment = Alignment.Start,
                    ) {
                        Text(server.name, style = MaterialTheme.typography.titleSmall)
                        Text(server.detail, color = Muted, style = MaterialTheme.typography.labelMedium)
                    }
                    Text("›", color = Muted, fontSize = 24.sp)
                }
            }
        }

        TextButton(onClick = { showManual = !showManual }) {
            Text(if (showManual) "Hide manual setup" else "+ Add server manually", color = Muted)
        }

        if (showManual) {
            OutlinedTextField(
                value = url,
                onValueChange = { url = it },
                label = { Text("Server address") },
                placeholder = { Text("192.168.1.10:32400") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Go),
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { vm.connect(url) },
                enabled = !busy && url.isNotBlank(),
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (busy) {
                    CircularProgressIndicator(
                        Modifier.padding(2.dp),
                        strokeWidth = 2.dp,
                        color = androidx.compose.ui.graphics.Color.White,
                    )
                } else {
                    Text("Connect")
                }
            }
            Text(
                "Enter a hostname or address. A bare host uses port 32400.",
                color = Muted,
                style = MaterialTheme.typography.labelMedium,
            )
        }

        if (permissionDenied) {
            Text(
                "Local-network access is off. Allow it to discover or connect to a server on this network.",
                color = Muted,
                style = MaterialTheme.typography.labelMedium,
            )
        } else if (discovery.error != null) {
            Text(discovery.error!!, color = Muted, style = MaterialTheme.typography.labelMedium)
        }

        TextButton(
            onClick = {
                if (hasLocalNetworkPermission()) {
                    vm.restartServerDiscovery()
                } else {
                    permissionLauncher.launch(LocalNetworkPermission)
                }
            },
            enabled = !busy,
        ) { Text("Scan again", color = Muted) }
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

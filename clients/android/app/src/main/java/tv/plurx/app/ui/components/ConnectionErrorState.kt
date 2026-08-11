package tv.plurx.app.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import tv.plurx.app.data.ConnectionAction
import tv.plurx.app.data.ConnectionFailure
import tv.plurx.app.data.copyFor
import tv.plurx.app.ui.theme.Muted

/**
 * The full-surface rendering of a connectivity class — `title`, `detail`, and
 * every action the contract gives that class (docs §5, "Full surface").
 *
 * Nothing here writes copy: every string comes from
 * `tests/contracts/connectivity-copy.json` by way of [copyFor]. A screen that
 * wants a sentence the contract does not have is a signal the taxonomy is
 * short a class, not a licence to add a string.
 *
 * Initial D-pad focus lands on Retry, following `PlayerScreen.PlaybackFailed`
 * — the app's pre-existing well-formed error UI. An error state a TV viewer
 * cannot reach the button of is the same as having no button.
 */
@Composable
fun ConnectionErrorState(
    failure: ConnectionFailure,
    server: String?,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
    onChangeServer: (() -> Unit)? = null,
) {
    val copy = remember(failure, server) { copyFor(failure, server) }
    val retryFocusRequester = remember { FocusRequester() }
    RequestInitialFocus(retryFocusRequester)
    Box(modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        Column(
            Modifier.widthIn(max = 460.dp).padding(24.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(
                copy.title,
                style = MaterialTheme.typography.titleLarge,
                textAlign = TextAlign.Center,
            )
            Text(
                copy.detail,
                color = Muted,
                style = MaterialTheme.typography.bodyMedium,
                textAlign = TextAlign.Center,
            )
            Spacer(Modifier.size(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                copy.actions.forEach { action ->
                    when (action) {
                        ConnectionAction.Retry -> TvButton(
                            onClick = onRetry,
                            modifier = Modifier.focusRequester(retryFocusRequester),
                        ) { Text(action.label) }
                        // A class can carry `change_server` on a surface with
                        // nowhere to send the viewer (a detail page has no
                        // server picker). Offering a dead button is worse than
                        // offering one action, so it is drawn only when the
                        // surface handed us somewhere to go.
                        ConnectionAction.ChangeServer -> if (onChangeServer != null) {
                            TvOutlinedButton(onClick = onChangeServer) { Text(action.label) }
                        }
                    }
                }
            }
        }
    }
}

/**
 * The transient-notice shape (docs §5, `cached_content_wins`): a refresh failed
 * over content already on screen, so the class gets one `short` line above the
 * content and a Retry — it must not replace what the viewer is reading.
 */
@Composable
fun ConnectionErrorBanner(
    failure: ConnectionFailure,
    server: String?,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val copy = remember(failure, server) { copyFor(failure, server) }
    Row(
        modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .padding(start = 16.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            copy.short,
            color = Muted,
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.weight(1f),
        )
        // Rule `every_error_offers_retry`: the banner is a surface too.
        TvTextButton(onClick = onRetry) { Text(ConnectionAction.Retry.label) }
    }
}

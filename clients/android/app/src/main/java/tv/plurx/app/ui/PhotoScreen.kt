package tv.plurx.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import coil.compose.AsyncImage
import coil.request.ImageRequest
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import tv.plurx.app.data.Session
import tv.plurx.app.ui.components.SafeBackButton

@Composable
fun PhotoScreen(itemId: Long, onBack: () -> Unit) {
    Box(Modifier.fillMaxSize().background(Color.Black)) {
        AsyncImage(
            model = ImageRequest.Builder(LocalContext.current)
                .data(Session.url("/api/v1/items/$itemId/photo"))
                .crossfade(true)
                .build(),
            contentDescription = "Full-size photo",
            contentScale = ContentScale.Fit,
            modifier = Modifier.fillMaxSize().pointerInput(Unit) {
                detectTapGestures(onDoubleTap = { /* Reserved for zoom state. */ })
            },
        )
        SafeBackButton(onBack = onBack, modifier = Modifier.align(Alignment.TopStart))
    }
}

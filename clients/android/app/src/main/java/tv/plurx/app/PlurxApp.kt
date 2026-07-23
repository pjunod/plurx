package tv.plurx.app

import android.app.Application
import coil.ImageLoader
import coil.ImageLoaderFactory
import tv.plurx.app.data.Net

/**
 * Routes Coil's poster/backdrop loading through the same authenticated
 * OkHttpClient the API and player use, so image requests to
 * `/api/v1/images/…` carry the bearer token.
 */
class PlurxApp : Application(), ImageLoaderFactory {
    override fun newImageLoader(): ImageLoader =
        ImageLoader.Builder(this)
            .okHttpClient(Net.client)
            .crossfade(true)
            .build()
}

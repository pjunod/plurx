package tv.plurx.app.ui

import android.app.Application
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.ViewModelStore
import androidx.lifecycle.ViewModelStoreOwner
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import tv.plurx.app.data.Session
import tv.plurx.app.data.SettingsStore

@RunWith(AndroidJUnit4::class)
class SavedSessionResumeTest {
    @Test
    fun savedSessionPaintsHomeBeforeAnUnreachableServerFinishesValidation() = runBlocking {
        val application = ApplicationProvider.getApplicationContext<Application>()
        val settings = SettingsStore(application)
        val store = ViewModelStore()
        val owner = object : ViewModelStoreOwner {
            override val viewModelStore: ViewModelStore = store
        }

        settings.clearServer()
        settings.saveSession(
            origin = "http://127.0.0.1:1",
            token = "resume-test-token",
            username = "resume-test-user",
            userId = 91,
        )

        try {
            val factory = ViewModelProvider.AndroidViewModelFactory.getInstance(application)
            val vm = ViewModelProvider(owner, factory)[AppViewModel::class.java]

            val firstPaint = withTimeout(2_000) {
                vm.phase.first { it != Phase.Loading }
            }

            assertEquals(Phase.Ready, firstPaint)
        } finally {
            store.clear()
            settings.clearServer()
            Session.origin = ""
            Session.token = null
        }
    }
}

package tv.plurx.app.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SettingsStoreTest {

    @Test
    fun pointingAtADifferentServerDropsThePreviousToken() {
        // Connect to A, log in, then connect to B and get killed before the
        // login screen. On relaunch the record must not offer A's bearer to B
        // — over plain HTTP, and A's session dies when B answers 401.
        val afterA = StoredCredentials("http://a:32400", "token-a", "paul")
        val afterB = credentialsForNewOrigin(afterA, "http://b:32400")

        assertEquals("http://b:32400", afterB.origin)
        assertNull(afterB.token)
        assertNull(afterB.username)
    }

    @Test
    fun reconnectingToTheSameServerKeepsTheSession() {
        val stored = StoredCredentials("http://a:32400", "token-a", "paul")
        assertEquals(stored, credentialsForNewOrigin(stored, "http://a:32400"))
    }

    @Test
    fun aFirstConnectionHasNothingToKeep() {
        assertEquals(
            StoredCredentials("http://a:32400", null, null),
            credentialsForNewOrigin(StoredCredentials(null, null, null), "http://a:32400"),
        )
        // A token with no origin behind it cannot be proven to belong here.
        assertNull(
            credentialsForNewOrigin(StoredCredentials("", "orphan", "paul"), "http://a:32400").token,
        )
    }
}

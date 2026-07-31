package tv.plurx.app.data

import org.junit.Assert.assertEquals
import org.junit.Test

class ServerDiscoveryTest {
    @Test
    fun formatsIpv4AndBonjourHosts() {
        assertEquals("http://192.168.1.10:32400", nsdOrigin("192.168.1.10", 32400))
        assertEquals("http://plurx-home.local:32400", nsdOrigin("plurx-home.local.", 32400))
    }

    @Test
    fun bracketsIpv6AndEscapesItsScope() {
        assertEquals(
            "http://[fe80::1%25wlan0]:32400",
            nsdOrigin("fe80::1%wlan0", 32400),
        )
    }
}

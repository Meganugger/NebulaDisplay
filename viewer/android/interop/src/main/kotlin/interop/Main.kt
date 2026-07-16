// SPAKE2 interop harness: the Kotlin client (the Android app's real code)
// against the Rust server. See README.md.
package interop

import dev.nebuladisplay.viewer.NdspCrypto
import dev.nebuladisplay.viewer.Spake2
import java.io.BufferedReader
import java.io.InputStreamReader
import java.util.Base64
import kotlin.system.exitProcess

private fun b64(b: ByteArray): String = Base64.getEncoder().encodeToString(b)
private fun unb64(s: String): ByteArray = Base64.getDecoder().decode(s)

/** Minimal one-level JSON string/bool field extraction (test tool only). */
private fun field(json: String, key: String): String? {
    val m = Regex("\"$key\"\\s*:\\s*(\"([^\"]*)\"|true|false)").find(json) ?: return null
    return m.groups[2]?.value ?: m.groups[1]!!.value
}

private class RustServer(bin: String) {
    private val proc = ProcessBuilder(bin).redirectErrorStream(false).start()
    private val out = proc.outputStream.bufferedWriter()
    private val inp = BufferedReader(InputStreamReader(proc.inputStream))
    fun send(line: String) { out.write(line); out.write("\n"); out.flush() }
    fun recv(): String = inp.readLine() ?: error("rust server closed unexpectedly")
    fun close() { proc.destroy() }
}

fun main(args: Array<String>) {
    require(args.size == 1) { "usage: interop <path-to-spake2_interop-binary>" }
    val bin = args[0]
    var failures = 0

    fun check(cond: Boolean, what: String) {
        if (cond) println("ok   $what") else { println("FAIL $what"); failures++ }
    }

    // Positive rounds: varied PINs and nonces, keys must agree exactly.
    for (round in 0 until 25) {
        val pin = "%06d".format((round * 373_211 + 42) % 1_000_000)
        val nonce = ByteArray(16) { i -> ((i * 31 + round * 7) and 0xFF).toByte() }
        val client = Spake2.Client(pin, nonce)
        val rust = RustServer(bin)
        rust.send("""{"pin":"$pin","nonce":"${b64(nonce)}","pa":"${b64(client.share)}"}""")
        val pb = unb64(field(rust.recv(), "pb") ?: error("no pb"))
        val keys = client.finish(pb)
        rust.send("""{"mac":"${b64(keys.confirmClient)}"}""")
        val fin = rust.recv()
        rust.close()
        check(field(fin, "ok") == "true", "round $round: Rust accepted the Kotlin confirm MAC")
        check(
            Spake2.macEqual(unb64(field(fin, "mac")!!), keys.confirmServer),
            "round $round: Kotlin accepted the Rust confirm MAC",
        )
        check(
            unb64(field(fin, "session_key")!!).contentEquals(keys.sessionKey),
            "round $round: session keys agree",
        )
        check(
            unb64(field(fin, "token_key")!!).contentEquals(keys.tokenKey),
            "round $round: token keys agree",
        )
        // The full pairing flow seals the trust token under token_key —
        // exercise seal/open across stacks too.
        val sealed = NdspCrypto.seal(keys.tokenKey, "trust-token-bytes".toByteArray(), "token".toByteArray())
        check(
            NdspCrypto.open(keys.tokenKey, sealed, "token".toByteArray())
                .contentEquals("trust-token-bytes".toByteArray()),
            "round $round: token seal/open roundtrip",
        )
    }

    // Negative: wrong PIN → Rust must reject the confirm MAC and the
    // Kotlin side must reject the Rust MAC.
    run {
        val nonce = ByteArray(16) { 9 }
        val client = Spake2.Client("111111", nonce)
        val rust = RustServer(bin)
        rust.send("""{"pin":"222222","nonce":"${b64(nonce)}","pa":"${b64(client.share)}"}""")
        val pb = unb64(field(rust.recv(), "pb") ?: error("no pb"))
        val keys = client.finish(pb)
        rust.send("""{"mac":"${b64(keys.confirmClient)}"}""")
        val fin = rust.recv()
        rust.close()
        check(field(fin, "ok") == "false", "wrong PIN: Rust rejects the client MAC")
        check(
            !Spake2.macEqual(unb64(field(fin, "mac")!!), keys.confirmServer),
            "wrong PIN: Kotlin rejects the server MAC",
        )
    }

    // Negative: degenerate server share must be rejected client-side.
    run {
        val client = Spake2.Client("123456", ByteArray(16) { 3 })
        val threw = try { client.finish(ByteArray(65) { 4 }); false } catch (_: Exception) { true }
        check(threw, "off-curve server share rejected")
    }

    if (failures > 0) {
        println("FAIL: $failures assertion(s)")
        exitProcess(1)
    }
    println("PASS: Kotlin SPAKE2 is byte-compatible with the Rust reference")
}

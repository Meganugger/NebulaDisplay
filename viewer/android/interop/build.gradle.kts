plugins {
    kotlin("jvm") version "2.0.21"
    application
}

dependencies {
    implementation("org.bouncycastle:bcprov-jdk18on:1.78.1")
}

sourceSets.main {
    kotlin {
        // The *shipped* Android sources under test (pure-JVM files only).
        srcDir("../app/src/main/java")
        include(
            "interop/**",
            "dev/nebuladisplay/viewer/Spake2.kt",
            "dev/nebuladisplay/viewer/NdspCrypto.kt",
        )
    }
}

application {
    mainClass.set("interop.MainKt")
}

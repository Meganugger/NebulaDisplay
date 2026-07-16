// Standalone JVM project (no Android SDK needed): compiles the app's real
// Spake2.kt/NdspCrypto.kt and cross-verifies them against the Rust
// implementation. See README.md.
pluginManagement {
    repositories { gradlePluginPortal(); mavenCentral() }
}
dependencyResolutionManagement {
    repositories { mavenCentral() }
}
rootProject.name = "spake2-interop"

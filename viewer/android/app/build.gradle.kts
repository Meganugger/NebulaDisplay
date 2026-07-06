plugins {
    id("com.android.application") version "8.5.2"
    id("org.jetbrains.kotlin.android") version "2.0.20"
}

android {
    namespace = "dev.nebuladisplay.viewer"
    compileSdk = 35
    defaultConfig {
        applicationId = "dev.nebuladisplay.viewer"
        minSdk = 26          // MediaCodec async + modern crypto
        targetSdk = 35
        versionCode = 1
        versionName = "0.2.0"
    }
    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { buildConfig = true }
}

dependencies {
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
}

// The Android app, on the core in crates/mobile: scripts/release-android.sh builds that for each ABI into
// src/generated/jniLibs and its Kotlin bindings into src/generated/kotlin, then runs this build.

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// The workspace's version, and a build number that only grows: minutes since 2020, unless given.
val cairnVersion = (findProperty("cairn.version") as String?) ?: "0.0.0"
val cairnBuild = (findProperty("cairn.build") as String?)?.toInt()
    ?: ((System.currentTimeMillis() / 1000 - 1_577_836_800) / 60).toInt()

// The Kotlin half of rustls-platform-verifier must be the version Cargo.lock gives its Rust half.
abstract class RustlsVersion : ValueSource<String, RustlsVersion.Params> {
    interface Params : ValueSourceParameters {
        val lockFile: RegularFileProperty
    }

    override fun obtain(): String {
        val lines = parameters.lockFile.get().asFile.readLines()
        val at = lines.indexOfFirst { it.trim() == "name = \"rustls-platform-verifier-android\"" }
        val version = if (at < 0) null else lines.drop(at + 1)
            .firstOrNull { it.trimStart().startsWith("version = ") }
            ?.substringAfter('"')?.substringBefore('"')
        return version ?: error("rustls-platform-verifier-android is not in Cargo.lock")
    }
}

val rustlsVersion = providers.of(RustlsVersion::class.java) {
    parameters.lockFile.set(rootProject.layout.projectDirectory.file("../Cargo.lock"))
}

configurations.configureEach {
    resolutionStrategy.eachDependency {
        if (requested.group == "org.rustls" && requested.name == "rustls-platform-verifier") {
            useVersion(rustlsVersion.get())
        }
    }
}

android {
    namespace = "storage.cairn.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "storage.cairn.android"
        // MediaStore's generation numbers, by which new photos are found, came in Android 11.
        minSdk = 30
        targetSdk = 36
        versionCode = cairnBuild
        versionName = cairnVersion
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    signingConfigs {
        create("release") {
            // From the maintainer's secrets, by scripts/release-android.sh: never in the repo.
            val store = System.getenv("CAIRN_ANDROID_KEYSTORE")
            if (store != null) {
                storeFile = file(store)
                storePassword = System.getenv("CAIRN_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("CAIRN_ANDROID_KEY_ALIAS") ?: "cairn"
                keyPassword = System.getenv("CAIRN_ANDROID_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // JNA and the bindings are reached by reflection, the verifier's Kotlin through JNI: kept whole.
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("release").takeIf { it.storeFile != null }
        }
    }

    sourceSets["main"].java.srcDir("src/generated/kotlin")
    sourceSets["main"].jniLibs.srcDir("src/generated/jniLibs")

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    packaging {
        // The core's library is stripped already; JNA's own native parts are only needed for these ABIs.
        jniLibs.useLegacyPackaging = false
    }
}

kotlin {
    compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.01.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.12.2")
    implementation("androidx.core:core-ktx:1.17.0")
    implementation("androidx.work:work-runtime-ktx:2.11.0")
    implementation("androidx.exifinterface:exifinterface:1.4.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    implementation("org.rustls:rustls-platform-verifier:latest.release")
}

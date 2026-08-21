import javax.inject.Inject
import org.gradle.api.DefaultTask
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.file.FileSystemOperations
import org.gradle.api.tasks.InputDirectory
import org.gradle.api.tasks.OutputDirectory
import org.gradle.api.tasks.TaskAction

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

abstract class GenerateReaderAssets @Inject constructor(
    private val fileSystem: FileSystemOperations,
) : DefaultTask() {
    @get:InputDirectory
    abstract val sourceDirectory: DirectoryProperty

    @get:OutputDirectory
    abstract val outputDirectory: DirectoryProperty

    @TaskAction
    fun generate() {
        fileSystem.sync {
            from(sourceDirectory)
            include("reader.js", "offline-reader.js", "offline-reader.html")
            into(outputDirectory)
        }
    }
}

val generateReaderAssets = tasks.register<GenerateReaderAssets>("generateReaderAssets") {
    sourceDirectory.set(layout.projectDirectory.dir("../../../crates/plurxd/src/web"))
    outputDirectory.set(layout.buildDirectory.dir("generated/reader-assets"))
}

android {
    namespace = "tv.plurx.app"
    compileSdk = 37

    defaultConfig {
        applicationId = "tv.plurx.app"
        // 23 covers phones and the vast majority of Android TV / Google TV boxes.
        minSdk = 23
        targetSdk = 37
        versionCode = 38
        versionName = "0.2.7"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        create("capabilityProbe") {
            initWith(getByName("debug"))
            applicationIdSuffix = ".capabilityprobe"
            versionNameSuffix = "-capability-probe"
            matchingFallbacks += listOf("debug")
        }
        release {
            // `material-icons-extended` alone is several thousand vector assets
            // of which this app draws about twenty, and every library it pulls
            // in ships code for surfaces the viewer never opens. R8 removes
            // what nothing reaches; `shrinkResources` removes the drawables and
            // strings that go with it. `proguard-rules.pro` had been dead
            // configuration since the day it was written.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        buildConfig = true
        compose = true
    }
    sourceSets {
        getByName("test").resources.directories.add("../../../tests/contracts")
    }
}

androidComponents {
    onVariants(selector().all()) { variant ->
        variant.sources.assets?.addGeneratedSourceDirectory(
            generateReaderAssets,
            GenerateReaderAssets::outputDirectory,
        )
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.androidx.material.icons)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.navigation.compose)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.kotlinx.serialization.json)

    implementation(libs.media3.exoplayer)
    implementation(libs.media3.exoplayer.hls)
    implementation(libs.media3.ui)
    implementation(libs.media3.session)
    implementation(libs.media3.datasource.okhttp)

    implementation(libs.retrofit)
    implementation(libs.retrofit.serialization)
    implementation(libs.okhttp)
    implementation(libs.okhttp.logging)

    implementation(libs.coil.compose)
    implementation(libs.datastore.preferences)
    implementation(libs.google.code.scanner)

    testImplementation(libs.junit)
    androidTestImplementation(libs.androidx.test.ext.junit)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.espresso)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.ui.test.junit4)
    debugImplementation(libs.androidx.ui.test.manifest)
}

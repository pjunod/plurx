# Reproducible Android build environment — JDK 25 + the exact SDK this app pins.
# The image is the toolchain only; the source is bind-mounted at run time (see
# `make android`), so the same image builds any revision without a rebuild.
# The image is linux/amd64 on every host because AGP's Linux AAPT2 distribution
# is x86-64; Docker Desktop emulates it on Apple Silicon.
#
#   docker build --platform linux/amd64 -t plurx-android-build clients/android
#   docker run --rm --platform linux/amd64 -u $(id -u):$(id -g) -e HOME=/tmp \
#     -v "$PWD":/workspace -w /workspace/clients/android \
#     plurx-android-build ./gradlew --no-daemon :app:assembleDebug
#
# Or just `make android` from the repo root.
FROM eclipse-temurin:25-jdk-jammy

ENV ANDROID_HOME=/opt/android-sdk \
    ANDROID_SDK_ROOT=/opt/android-sdk \
    DEBIAN_FRONTEND=noninteractive

# unzip for the cmdline-tools bundle; curl + ca-certificates to fetch it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends unzip curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Android command-line tools (pinned build; bump CLT_VERSION to move it). The
# bundle must live at $ANDROID_HOME/cmdline-tools/latest for sdkmanager to work.
ARG CLT_VERSION=15859902
RUN mkdir -p "${ANDROID_HOME}/cmdline-tools" \
    && curl -fsSL "https://dl.google.com/android/repository/commandlinetools-linux-${CLT_VERSION}_latest.zip" -o /tmp/clt.zip \
    && unzip -q /tmp/clt.zip -d "${ANDROID_HOME}/cmdline-tools" \
    && mv "${ANDROID_HOME}/cmdline-tools/cmdline-tools" "${ANDROID_HOME}/cmdline-tools/latest" \
    && rm /tmp/clt.zip

ENV PATH="${ANDROID_HOME}/cmdline-tools/latest/bin:${ANDROID_HOME}/platform-tools:${PATH}"

# The exact packages the app uses. Android 17 is published as platform 37.0 on
# the preview channel; `android-37` is not a package name. AGP 9.3.1 still
# invokes build-tools 36 for this project, while 37 is the platform-matched
# toolset, so both are present and Gradle never mutates the image at runtime.
# Accepting the licenses non-interactively is required for an unattended build.
RUN yes | sdkmanager --licenses > /dev/null \
    && sdkmanager --channel=3 --install "platform-tools" "platforms;android-37.0" "build-tools;36.0.0" "build-tools;37.0.0" > /dev/null \
    # World-read/write so a build run as an arbitrary host UID (`-u $(id -u)`, so
    # the APK lands host-owned) can read the SDK and, if AGP ever needs to, write
    # to it. This is a throwaway build container, not a runtime image.
    && chmod -R a+rwX "${ANDROID_HOME}"

WORKDIR /workspace/clients/android

# FilmCraft for Android

The Gradle project that packages `apps/filmcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/filmcraft-android/src/lib.rs`: `android_main`, the session (auto-save, preferences,
  crash logs in the app's files directory), the host hooks (pickers, save paths) and the JNI
  bridge to `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker (picked files
  are copied into `files/Media/`), publishing saved projects and finished exports into
  `Downloads/FilmCraft/`, and the full-screen window.
- `cargo ndk` drops `libfilmcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p filmcraft-android
cd android && ./gradlew assembleDebug
```

## Known limits (first version)

- No audio output yet: the desktop app plays sound through cpal, which is not built into this
  shell, so playback runs silently on the wall clock. No voice-over recording (microphone).
- File › Import / Open copy the picked files into the app's private `files/Media/` folder
  (media are read by range, so a real file is needed); large videos take a moment and use
  storage. Clearing the app's data removes them.
- Save writes `files/Projects/<name>.fcproj` and Export renders into `files/Exports/<name>`;
  each finished file is copied to `Downloads/FilmCraft/<name>` without a dialog (the same name
  saved again in one session overwrites it).
- No folder picker (Link Media search, proxy and Project Manager destinations), no "Edit
  Original" / "Reveal Log Files", no OS hardware video decoding (the pure-Rust decoders do
  everything), no TCP control channel.
- The desktop layout needs a tablet-sized screen; on a phone's cover screen it is cramped.

# R8 rules for the release build.
#
# Everything reachable from an entry point is kept automatically. What needs
# saying here is what is reached *reflectively*, which R8 cannot see: the
# generated kotlinx.serialization serializers for the wire models, and the
# generic signatures Retrofit reads off the API interface at runtime. Retrofit,
# OkHttp, Media3, Compose, DataStore and Coil each ship their own consumer
# rules inside their artifacts — do not restate them here, because a stale copy
# is worse than none.

# --- Reflection metadata -----------------------------------------------------
# Retrofit resolves a suspend function's return type from its generic
# signature; kotlinx.serialization looks up @Serializable annotations.
-keepattributes Signature, InnerClasses, EnclosingMethod
-keepattributes RuntimeVisibleAnnotations, RuntimeVisibleParameterAnnotations
-keepattributes AnnotationDefault

# --- kotlinx.serialization ---------------------------------------------------
# The plugin generates a `Companion.serializer()` per @Serializable class and
# looks it up by name. Without these the wire models decode to nothing.
-dontnote kotlinx.serialization.**
-keepclassmembers class tv.plurx.app.data.** {
    *** Companion;
}
-keepclasseswithmembers class tv.plurx.app.data.** {
    kotlinx.serialization.KSerializer serializer(...);
}
-keepclassmembers @kotlinx.serialization.Serializable class ** {
    static **$* *;
    *** INSTANCE;
    kotlinx.serialization.KSerializer serializer(...);
}
-keepclassmembers class **$WhenMappings {
    <fields>;
}

# --- Retrofit's API surface --------------------------------------------------
# The interface itself is only ever reached through `Proxy`.
-keep,allowobfuscation,allowshrinking interface tv.plurx.app.data.PlurxApi
-keep,allowobfuscation,allowshrinking class kotlin.coroutines.Continuation

# --- Diagnostics -------------------------------------------------------------
# A stripped stack trace from a viewer's TV is unreadable otherwise.
-keepattributes SourceFile, LineNumberTable
-renamesourcefileattribute SourceFile

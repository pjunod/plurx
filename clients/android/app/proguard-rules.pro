# kotlinx.serialization keeps generated serializers; keep @Serializable classes.
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.**
-keepclassmembers class tv.plurx.app.data.** {
    *** Companion;
}
-keepclasseswithmembers class tv.plurx.app.data.** {
    kotlinx.serialization.KSerializer serializer(...);
}

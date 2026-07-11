package com.example.gradlelib;

/** Message from the foreign Gradle library. */
public final class GradleGreeter {
    private GradleGreeter() {}

    public static String greet(String name) {
        return "Hello from Gradle, " + name + "!";
    }
}

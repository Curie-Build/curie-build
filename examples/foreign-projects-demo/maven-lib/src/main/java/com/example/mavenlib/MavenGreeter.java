package com.example.mavenlib;

/** Message from the foreign Maven library. */
public final class MavenGreeter {
    private MavenGreeter() {}

    public static String greet(String name) {
        return "Hello from Maven, " + name + "!";
    }
}

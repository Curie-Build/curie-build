package com.example.osgi;

/** A tiny greeting helper packaged as an OSGi bundle. */
public class Greeter {
    public String greet(String name) {
        if (name == null || name.isEmpty()) {
            return "Hello!";
        }
        return "Hello, " + name + "!";
    }
}

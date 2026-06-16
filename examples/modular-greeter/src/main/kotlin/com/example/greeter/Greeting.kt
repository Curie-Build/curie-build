package com.example.greeter

/**
 * Kotlin companion to the modular-greeter example.
 *
 * Demonstrates that Kotlin sources work inside a JPMS explicit module.
 * Phase 1 compiles this with kotlinc (classpath mode); Phase 2 re-compiles
 * module-info.java with javac --patch-module so the module descriptor covers
 * both the Java and Kotlin classes.
 */
class Greeting(val message: String, val module: String, val language: String) {
    companion object {
        @JvmStatic
        fun create(moduleName: String): Greeting =
            Greeting(
                message = "Hello from a modular Java + Kotlin application!",
                module = moduleName,
                language = "Java + Kotlin",
            )
    }
}
